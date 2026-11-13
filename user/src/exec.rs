//! execve and execveat: the process runs another program, in place.
//!
//! The pid, parent, process group, working directory, umask, descriptors (those not close-on-exec)
//! and signal mask stay; the address space is a new one, laid out and loaded as `Process::start`
//! does (a dynamic executable with its loader, lazily mapped pages), the other threads are gone,
//! handled signals go back to their default and the registers start over. A script (`#!`) runs
//! its interpreter. A parent suspended by vfork goes on.
//!
//! Nothing changes unless the new program loads: a file that is not there, not executable or not
//! an ELF is an error the caller gets back, and one whose pages haven't arrived makes the call
//! wait for them and run again.

use armless::cpu::Cpu;
use armless::dev::fs::{S_IFMT, S_IFREG};
use armless::Host;

use crate::files::{Kind, EACCES, EBADF, ENOENT};
use crate::mm::Image;
use crate::signal::AltStack;
use crate::{Ctx, ExecError, Layout, Process, Ret, PAGE};

const E2BIG: i64 = 7;
const ENOEXEC: i64 = 8;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ELOOP: i64 = 40;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_EMPTY_PATH: u64 = 0x1000;

/// The longest argument or environment string, and the most of them together (Linux's limits
/// for an 8 MiB stack)
const MAX_STRING: usize = 128 << 10;
const MAX_ARGS: usize = 2 << 20;
/// How many interpreters deep a script may go (a script whose interpreter is a script)
pub(crate) const MAX_SCRIPT_DEPTH: usize = 5;
/// An interpreter line is cut here
const SHEBANG_MAX: usize = 255;

/// The interpreter and the one optional argument of a script's `#!` line.
pub(crate) fn shebang(head: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let rest = head.strip_prefix(b"#!")?;
    let rest = &rest[..rest.len().min(SHEBANG_MAX)];
    let line = rest.split(|&c| c == b'\n').next()?.trim_ascii();
    let at = line
        .iter()
        .position(|c| c.is_ascii_whitespace())
        .unwrap_or(line.len());
    let (interp, arg) = line.split_at(at);
    let arg = arg.trim_ascii();
    (!interp.is_empty()).then(|| (interp.to_vec(), (!arg.is_empty()).then(|| arg.to_vec())))
}

impl Process {
    /// A NUL-terminated string of the program's, up to `MAX_STRING`.
    fn exec_string(&self, addr: u64) -> Result<Vec<u8>, i64> {
        let mut s = Vec::new();
        let mut at = addr;
        while s.len() <= MAX_STRING {
            let mut b = vec![0u8; (PAGE - at % PAGE) as usize];
            if !self.read(at, &mut b) {
                return Err(-EFAULT);
            }
            match b.iter().position(|&c| c == 0) {
                Some(i) => {
                    s.extend_from_slice(&b[..i]);
                    return Ok(s);
                },
                None => s.extend_from_slice(&b),
            }
            at += b.len() as u64;
        }
        Err(-E2BIG)
    }

    /// The strings of the NULL-terminated array of pointers at `addr` (argv or envp), taking
    /// their sizes from `left`.
    fn exec_strings(&self, addr: u64, left: &mut usize) -> Result<Vec<Vec<u8>>, i64> {
        let mut out = Vec::new();
        if addr == 0 {
            return Ok(out);
        }
        for i in 0.. {
            let p = self.u64_at(addr + 8 * i).ok_or(-EFAULT)?;
            if p == 0 {
                break;
            }
            let s = self.exec_string(p)?;
            *left = left.checked_sub(s.len() + 9).ok_or(-E2BIG)?;
            out.push(s);
        }
        Ok(out)
    }

    /// execve(path, argv, envp), execveat(dirfd, path, argv, envp, flags)
    pub(crate) fn sys_execve<H: Host>(
        &mut self,
        cpu: &mut Cpu<H>,
        cur: usize,
        dirfd: i32,
        [path, argv, envp, flags]: [u64; 4],
    ) -> Ret {
        let v = Ret::Val;
        if flags & !(AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW) != 0 {
            return v(-EINVAL);
        }
        let Some(board) = self.board.clone() else {
            return v(-ENOENT); // no filesystem: nothing to run
        };
        if !self.read(path, &mut [0u8; 1]) {
            return v(-EFAULT);
        }
        let mut name = self.cstr(path);
        let mut left = MAX_ARGS;
        let mut args = match self.exec_strings(argv, &mut left) {
            Ok(a) => a,
            Err(e) => return v(e),
        };
        let env = match self.exec_strings(envp, &mut left) {
            Ok(e) => e,
            Err(e) => return v(e),
        };
        if args.is_empty() {
            args.push(Vec::new()); // as Linux does: a program always has an argv[0]
        }
        let mut ino = if name.is_empty() && flags & AT_EMPTY_PATH != 0 {
            match self.files.get(dirfd).map(|d| &d.kind) {
                Some(Kind::File { ino, .. }) => {
                    name = format!("/dev/fd/{dirfd}").into_bytes();
                    *ino
                },
                Some(_) => return v(-EACCES),
                None => return v(-EBADF),
            }
        } else {
            match self.lookup(dirfd, &name, flags & AT_SYMLINK_NOFOLLOW == 0) {
                Ok(i) => i,
                Err(e) => return v(e),
            }
        };
        // What the program is named, as it is told to the program
        let exe = name.clone();
        // Where `name` is from: dirfd, then the cwd once a script's interpreter runs instead
        let mut from = dirfd;
        let mut depth = 0;
        let image = loop {
            let runs = self.fs(|fs| {
                fs.get(ino)
                    .map(|i| i.mode & S_IFMT == S_IFREG && i.mode & 0o111 != 0)
            });
            if runs != Some(Some(true)) {
                return v(-EACCES);
            }
            let image = match Image::of(&board, ino) {
                Ok(i) => i,
                Err(ExecError::Need(b)) => return Ret::Need(b),
                Err(ExecError::Fail(_)) => return v(-ENOEXEC),
            };
            let Some((interp, arg)) = shebang(image.head()) else {
                break image;
            };
            depth += 1;
            if depth > MAX_SCRIPT_DEPTH {
                return v(-ELOOP);
            }
            // The interpreter, its argument, the script's path, then what the script was given
            // after its own name
            let mut next = vec![interp.clone()];
            next.extend(arg);
            next.push(name);
            next.extend(args.drain(1..));
            args = next;
            ino = match self.lookup(AT_FDCWD, &interp, true) {
                Ok(i) => i,
                Err(e) => return v(e),
            };
            name = interp;
            from = AT_FDCWD;
        };
        // /proc/self/exe: the file that runs, symlinks resolved (/dev/fd/N stays as it is)
        let real = self
            .start_dir(from)
            .ok()
            .and_then(|start| self.fs(|fs| crate::files::canonical(fs, start, &name).ok()))
            .flatten();
        let layout = match Layout::of(&image, Some(&*board)) {
            Ok(l) => l,
            Err(ExecError::Need(b)) => return Ret::Need(b),
            Err(ExecError::Fail(_)) => return v(-ENOEXEC),
        };
        let strings = |a: &[Vec<u8>]| -> Vec<String> {
            a.iter()
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        };
        let (args, env) = (strings(&args), strings(&env));

        // The new address space is made beside the old, which stays the process's until the
        // program has loaded
        let space = match layout.space(&mut self.frames.borrow_mut()) {
            Ok(s) => s,
            Err(_) => return v(-ENOMEM),
        };
        let old = std::mem::replace(&mut self.p.mm, crate::mm::SharedMm::new(space));
        let (entry, sp) = match self.load_image(&image, &layout, &args, &env, &exe) {
            Ok(r) => r,
            Err(e) => {
                self.teardown();
                self.p.mm = old;
                return match e {
                    ExecError::Need(b) => Ret::Need(b),
                    ExecError::Fail(_) => v(-ENOEXEC),
                };
            },
        };

        // From here it is the new program. Its other threads stop (those on other CPUs before the
        // old address space goes), and the old address space goes with its last user (a vfork
        // parent still has it).
        let me = self.threads[cur].tid;
        let pid = self.p.pid;
        self.take_off_cpus(|t| t.pid == pid && t.tid != me);
        if crate::mm::SharedMm::strong_count(&old) == 1 {
            let refs = {
                let mut m = old.borrow_mut();
                m.destroy(&mut self.frames.borrow_mut());
                m.take_refs()
            };
            self.settle_refs(refs);
            self.trim_file_pages();
        }
        drop(old);
        // musl's loader finds $ORIGIN from /proc/self/exe, as for /usr/bin/java ->
        // ../lib/jvm/.../bin/java, whose libjli.so is beside the real one
        self.p.exe = real.unwrap_or(exe);
        self.p.sig.exec();
        self.p.aio.clear();
        // (Interval timers stay; POSIX timers go)
        self.timers.exec(pid);
        for (i, t) in self.threads.iter_mut().enumerate() {
            if t.pid == pid && i != cur {
                t.exited = true;
            }
        }
        let t = &mut self.threads[cur];
        t.tid = pid;
        t.clear_tid = 0;
        t.sig.altstack = AltStack::default();
        t.sig.altstack_unknown = false;
        t.sig.saved_mask = None;
        let on_exec: Vec<i32> = self
            .files
            .fds
            .iter()
            .filter(|(_, &(_, cloexec))| cloexec)
            .map(|(&fd, _)| fd)
            .collect();
        for fd in on_exec {
            self.close_fd(fd);
        }
        let mut first = Ctx::save(cpu);
        first.x = [0; 31];
        first.v = [0; 32];
        (first.fpcr, first.fpsr, first.tpidr) = (0, 0, 0);
        first.nzcv = [false; 4];
        first.sp = sp;
        first.pc = entry;
        first.load(cpu);
        self.release_vfork(pid);
        self.poke();
        v(0)
    }
}
