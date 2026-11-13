//! Processes the host starts in the machine (`Process::spawn`), as a machine with a kernel runs a
//! job in a shell of its own: a program of the filesystem, found as the first one is (PATH, the
//! working directory, `#!` scripts), in a new address space, a session of its own and with no
//! parent, its stdin, stdout and stderr pipes whose other ends the host holds. The host writes its
//! input (`spawn_input`, and closes it for the end), takes what it printed (`spawn_poll`), signals
//! its process group (`spawn_kill`: a job past its time limit, and what it started), and once it
//! has ended lets go of it (`spawn_release`: its pipes, and the process nobody else waits for).
//!
//! With no parent nothing reaps it but the host: a shell that waits for every child (the first
//! process's) doesn't take its status. What it leaves running (`cmd &`) goes on, as a job's does.
//! The host's ends of the pipes are not in a snapshot: take one with no job running.

use std::collections::BTreeMap;

use armless::dev::fs::ROOT;

use crate::files::{self, Kind};
use crate::signal::{Signals, ThreadSig};
use crate::{mm, Creds, Ctx, ExecError, Layout, Proc, Process, Thread};

/// What the host holds of a process it started: its pipes' other ends (descriptions)
pub(crate) struct Spawned {
    stdin: Option<u32>,
    stdout: u32,
    stderr: u32,
}

/// What a started process did since the host last looked (`Process::spawn_poll`)
#[derive(Debug, Default)]
pub struct SpawnPoll {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Once it has ended: its exit status (0 to 255), or 128 plus the signal that ended it
    pub exit: Option<i32>,
}

impl Process {
    /// Starts `args[0]` (by PATH in `env` when it has no slash, from `cwd`) as a process of its
    /// own, as `creds` (root by default): its pid. Its pages come from the filesystem as it runs;
    /// `ExecError::Need` says its program's blobs are on their way (call again once they are in).
    pub fn spawn(
        &mut self,
        args: &[String],
        env: &[String],
        cwd: &str,
        creds: Option<(u32, u32)>,
    ) -> Result<i64, ExecError> {
        let fail = |s: String| ExecError::Fail(s);
        let board = self
            .board
            .clone()
            .ok_or_else(|| fail("the machine has no filesystem".into()))?;
        let mut args = args.to_vec();
        if args.is_empty() {
            return Err(fail("no program".into()));
        }
        let path_var = env
            .iter()
            .find_map(|e| e.strip_prefix("PATH="))
            .unwrap_or("/usr/local/bin:/usr/bin:/bin");
        let cwd_ino = board
            .with_fs(|fs| files::resolve(fs, ROOT, cwd.as_bytes(), true).unwrap_or(ROOT))
            .unwrap_or(ROOT);
        // The program, and a script's interpreter in its place
        let mut depth = 0;
        let (path, image) = loop {
            let name = args[0].clone();
            let candidates: Vec<String> = if name.contains('/') {
                vec![name.clone()]
            } else {
                path_var.split(':').map(|d| format!("{d}/{name}")).collect()
            };
            let found = board.with_fs(|fs| {
                candidates.iter().find_map(|p| {
                    let ino = files::resolve(fs, cwd_ino, p.as_bytes(), true).ok()?;
                    fs.get(ino)
                        .filter(|i| !i.is_dir())
                        .map(|_| (p.clone(), ino))
                })
            });
            let (path, ino) = found
                .flatten()
                .ok_or_else(|| fail(format!("{name}: not found")))?;
            let image = mm::Image::of(&board, ino)?;
            let Some((interp, arg)) = crate::exec::shebang(image.head()) else {
                break (path, image);
            };
            depth += 1;
            if depth > crate::exec::MAX_SCRIPT_DEPTH {
                return Err(fail(format!("{name}: too many levels of #! interpreters")));
            }
            let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            let mut next = vec![s(&interp)];
            next.extend(arg.as_deref().map(s));
            next.push(path);
            next.extend(args.drain(1..));
            args = next;
        };
        let layout = Layout::of(&image, Some(&board))?;
        let exe = board
            .with_fs(|fs| files::canonical(fs, cwd_ino, path.as_bytes()).ok())
            .flatten()
            .unwrap_or_else(|| path.clone().into_bytes());
        let space = layout.space(&mut self.frames.borrow_mut())?;

        let pid = self.next_id;
        self.next_id += 1;
        let creds = creds.map_or_else(Creds::default, |(uid, gid)| Creds {
            uid: [uid; 3],
            gid: [gid; 3],
        });
        self.procs.insert(
            pid,
            Proc {
                pid,
                // No parent: the host takes its status
                ppid: 0,
                pgid: pid,
                sid: pid,
                mm: mm::SharedMm::new(space),
                fds: BTreeMap::new(),
                exit_signal: 0,
                zombie: None,
                reap: false,
                cwd: cwd_ino,
                umask: 0o022,
                exe: exe.clone(),
                sig: Signals::default(),
                creds,
                groups: Vec::new(),
                aio: BTreeMap::new(),
            },
        );
        // Its program loaded into its address space, which is the one the process that runs has
        let before = self.p.pid;
        self.switch_to(pid);
        let loaded = self.load_image(&image, &layout, &args, env, &exe);
        let (entry, sp) = match loaded {
            Ok(r) => r,
            Err(e) => {
                self.teardown();
                self.switch_to(before);
                self.procs.remove(&pid);
                return Err(e);
            },
        };
        // Its stdin, stdout and stderr: pipes, the other ends the host's
        let (input, output, errors) = (
            self.files.new_pipe(),
            self.files.new_pipe(),
            self.files.new_pipe(),
        );
        let stdin = self.files.add(Kind::PipeRead(input), 0);
        let stdout = self.files.add(Kind::PipeWrite(output), 1);
        let stderr = self.files.add(Kind::PipeWrite(errors), 1);
        for (fd, d) in [(0, stdin), (1, stdout), (2, stderr)] {
            self.files.put(fd, d, false);
        }
        let host = Spawned {
            stdin: Some(self.files.add(Kind::PipeWrite(input), 1)),
            stdout: self.files.add(Kind::PipeRead(output), 0),
            stderr: self.files.add(Kind::PipeRead(errors), 0),
        };
        for d in [host.stdin.unwrap_or_default(), host.stdout, host.stderr] {
            self.files.hold(d);
        }
        self.switch_to(before);
        self.spawned.insert(pid, host);
        self.threads.push(Thread {
            tid: pid,
            pid,
            ctx: Ctx {
                x: [0; 31],
                sp,
                pc: entry,
                nzcv: [false; 4],
                v: [0; 32],
                fpcr: 0,
                fpsr: 0,
                tpidr: 0,
            },
            clear_tid: 0,
            wait: None,
            resume_deadline: None,
            exited: false,
            tty_wait: false,
            rerun: None,
            sig: ThreadSig::default(),
            sched: Default::default(),
        });
        self.poke();
        Ok(pid)
    }

    /// Bytes for the stdin of process `pid` (`spawn`'s): false if it has none any more
    pub fn spawn_input(&mut self, pid: i64, data: &[u8]) -> bool {
        let Some(d) = self.spawned.get(&pid).and_then(|s| s.stdin) else {
            return false;
        };
        let Some(Kind::PipeWrite(p)) = self.files.descs.get(&d).map(|d| d.kind.clone()) else {
            return false;
        };
        let Some(pipe) = self.files.pipes.get_mut(&p) else {
            return false;
        };
        pipe.buf.extend(data);
        self.poke();
        true
    }

    /// The end of the stdin of process `pid`: a read past what is left gives 0
    pub fn spawn_close_input(&mut self, pid: i64) {
        if let Some(d) = self.spawned.get_mut(&pid).and_then(|s| s.stdin.take()) {
            self.release_desc(d);
            self.poke();
        }
    }

    /// What process `pid` printed since the last call, and its status once it has ended (None
    /// for a pid that isn't one `spawn` started, or was released).
    pub fn spawn_poll(&mut self, pid: i64) -> Option<SpawnPoll> {
        let s = self.spawned.get(&pid)?;
        let mut take = |d: u32| match self.files.descs.get(&d).map(|d| d.kind.clone()) {
            Some(Kind::PipeRead(p)) => self
                .files
                .pipes
                .get_mut(&p)
                .map(|pipe| pipe.buf.drain(..).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let (stdout, stderr) = (take(s.stdout), take(s.stderr));
        if !stdout.is_empty() || !stderr.is_empty() {
            // (A writer waiting on a full pipe goes on)
            self.poke();
        }
        let exit = match self.proc_ref(pid) {
            Some(p) => p
                .zombie
                .map(|w| if w & 0x7f == 0 { (w >> 8) & 0xff } else { 128 + (w & 0x7f) }),
            // Gone (an exit nobody could wait for): ended
            None => Some(0),
        };
        Some(SpawnPoll {
            stdout,
            stderr,
            exit,
        })
    }

    /// `sig` for the process group of process `pid` (`spawn`'s): it and what it started that is
    /// still in its group
    pub fn spawn_kill(&mut self, pid: i64, sig: u32) {
        if !self.spawned.contains_key(&pid) {
            return;
        }
        let group: Vec<i64> = self
            .proc_list()
            .filter(|p| p.pgid == pid && p.zombie.is_none())
            .map(|p| p.pid)
            .collect();
        for p in group {
            self.raise_in(p, sig, None, 0);
        }
    }

    /// Done with process `pid` (`spawn`'s): its pipes' host ends let go, and the process, once it
    /// has ended, taken away (as its parent's wait would)
    pub fn spawn_release(&mut self, pid: i64) {
        let Some(s) = self.spawned.remove(&pid) else {
            return;
        };
        for d in s.stdin.into_iter().chain([s.stdout, s.stderr]) {
            self.release_desc(d);
        }
        if self.proc_ref(pid).is_some_and(|p| p.zombie.is_some()) {
            if self.p.pid == pid {
                // (The process that runs is never one that ended: switch_to moves off it)
                self.p.reap = true;
            } else {
                self.procs.remove(&pid);
            }
        }
        self.poke();
    }

    /// The processes `spawn` started that haven't been released
    pub fn spawned(&self) -> Vec<i64> {
        self.spawned.keys().copied().collect()
    }
}
