//! /proc and the parts of /sys programs read, made here as Linux gives them.
//!
//! /proc/<pid>, for every process there is (an ended one nobody waited for too), `self` and
//! `thread-self` being the one that asks: `cmdline`, `comm`, `stat`, `statm`, `status`, `io`,
//! `environ`, `auxv`, `maps`, `limits`, `cgroup`, `mounts`, `mountinfo`, `oom_score`,
//! `oom_score_adj` and `wchan`; the links `exe`, `cwd` and `root`; and the directories `fd`,
//! `fdinfo` and `task` (a directory per thread, with its own `stat`, `status`, `comm` and
//! `cmdline`). /proc itself lists the processes (getdents64) after what the filesystem's /proc
//! directory holds and `self`: busybox's ps, top, pgrep and pkill read /proc so, and so does a
//! shell's `for f in /proc/[0-9]*/cmdline`.
//!
//! The machine's own: `stat` (each CPU's time, from the time it ran threads: loadavg.rs),
//! `uptime`, `loadavg`, `meminfo`, `version`, `filesystems`, and what /proc/sys and /sys say of
//! the CPUs, memory limits and cgroup that language runtimes size themselves by (`sys_file`).
//!
//! Times are in clock ticks of 100 Hz (USER_HZ). A process's CPU time is the host time its
//! threads held a CPU (timer.rs), all of it user time: there is no split between user and system
//! time here.
//!
//! A descriptor of /proc counts its position as any directory's does through the filesystem's
//! entries and `self`; from `PID_POS` on it is `PID_POS` plus the pid of the last process given,
//! so a listing taken in pieces goes on after that one whichever processes came or went since.
//! The other directories made here (`Kind::VDir`) list their entries, `.` and `..` first, by
//! index.

use armless::dev::fs::{Ino, ROOT};

use crate::errno::{EBADF, ENOTDIR};
use crate::files::{self, encode_dirents};
use crate::mm::{PROT_EXEC, PROT_READ, PROT_WRITE};
use crate::{Proc, Process, PAGE};

/// Where the processes' entries start in a descriptor of /proc's position
const PID_POS: u64 = 1 << 32;
/// The inode numbers /proc's entries of its own have, above any of the filesystem's
const PID_INO: u64 = 1 << 48;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;
/// How long a comm is at most (TASK_COMM_LEN, less its NUL)
const COMM_LEN: usize = 15;
/// Clock ticks a second (USER_HZ), and ns in one
const HZ: u64 = 100;
const TICK_NS: u64 = 1_000_000_000 / HZ;
/// The release user mode says it is (uname, /proc/sys/kernel/osrelease)
pub(crate) const RELEASE: &str = "6.6.0-armless-user";
/// What /proc/version and /proc/sys/kernel/version say
const VERSION: &str = "#1 SMP armless";

/// The files of /proc/<pid>/, as its directory lists them
const PID_FILES: [&[u8]; 16] = [
    b"auxv",
    b"cgroup",
    b"cmdline",
    b"comm",
    b"environ",
    b"io",
    b"limits",
    b"maps",
    b"mountinfo",
    b"mounts",
    b"oom_score",
    b"oom_score_adj",
    b"stat",
    b"statm",
    b"status",
    b"wchan",
];
/// Its links and directories
const PID_LINKS: [&[u8]; 3] = [b"cwd", b"exe", b"root"];
const PID_DIRS: [&[u8]; 3] = [b"fd", b"fdinfo", b"task"];
/// The files of /proc/<pid>/task/<tid>/
const TASK_FILES: [&[u8]; 4] = [b"cmdline", b"comm", b"stat", b"status"];

/// The resource limits /proc/<pid>/limits lists: name, soft and hard (None: unlimited), units
const LIMITS: [(&str, Option<u64>, Option<u64>, &str); 16] = [
    ("Max cpu time", None, None, "seconds"),
    ("Max file size", None, None, "bytes"),
    ("Max data size", None, None, "bytes"),
    ("Max stack size", Some(crate::STACK_SIZE), None, "bytes"),
    ("Max core file size", Some(0), None, "bytes"),
    ("Max resident set", None, None, "bytes"),
    ("Max processes", None, None, "processes"),
    ("Max open files", Some(files::NOFILE), Some(files::NOFILE), "files"),
    ("Max locked memory", None, None, "bytes"),
    ("Max address space", None, None, "bytes"),
    ("Max file locks", None, None, "locks"),
    ("Max pending signals", None, None, "signals"),
    ("Max msgqueue size", None, None, "bytes"),
    ("Max nice priority", Some(0), Some(0), ""),
    ("Max realtime priority", Some(0), Some(0), ""),
    ("Max realtime timeout", None, None, "us"),
];

/// What /proc/<pid>/{mounts,mountinfo} and /proc/mounts say is mounted
const MOUNTS: &str = "host9p / 9p rw 0 0\nproc /proc proc rw 0 0\n";
const MOUNTINFO: &str =
    "1 1 0:1 / / rw - 9p host9p rw\n2 1 0:2 / /proc rw,nosuid,nodev,noexec - proc proc rw\n";

/// The ticks in `ns`
fn ticks(ns: u64) -> u64 {
    ns / TICK_NS
}

/// What a path under /proc/<pid> names, its process's name (`self`, a pid) taken off
enum PidPath<'a> {
    /// /proc/<pid>
    Dir,
    /// /proc/<pid>/<rest>
    Under(&'a [u8]),
}

/// /proc/<name>[/<rest>], with `name` a pid, `self` or `thread-self`
fn pid_path(path: &[u8]) -> Option<(&[u8], PidPath<'_>)> {
    let rest = path.strip_prefix(b"/proc/")?;
    let rest = trim_slashes(rest);
    let (name, under) = match rest.iter().position(|&b| b == b'/') {
        Some(i) => (&rest[..i], PidPath::Under(&rest[i + 1..])),
        None => (rest, PidPath::Dir),
    };
    (matches!(name, b"self" | b"thread-self") || is_pid(name)).then_some((name, under))
}

/// `path` without the slashes it ends with
pub(crate) fn trim_slashes(path: &[u8]) -> &[u8] {
    let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    if end == 0 && !path.is_empty() {
        &path[..1]
    } else {
        &path[..end]
    }
}

/// The number a name is, if it is a number (a pid, a tid, a descriptor)
fn number<T: std::str::FromStr>(name: &[u8]) -> Option<T> {
    if !is_pid(name) {
        return None;
    }
    std::str::from_utf8(name).ok()?.parse().ok()
}

impl Process {
    /// The process a /proc path's first component names: `self` and `thread-self` the one that
    /// asks, a number one that is there.
    fn proc_named(&self, name: &[u8]) -> Option<&Proc> {
        match name {
            b"self" | b"thread-self" => Some(&self.p),
            _ => self.proc_ref(number(name)?),
        }
    }

    /// The descriptors of process `p`: the current one's are in `files`, the others' their own
    fn fds_of<'a>(&'a self, p: &'a Proc) -> &'a std::collections::BTreeMap<i32, (u32, bool)> {
        if p.pid == self.p.pid {
            &self.files.fds
        } else {
            &p.fds
        }
    }

    /// Whether thread `tid` is process `p`'s and hasn't ended
    fn has_thread(&self, p: &Proc, tid: i64) -> bool {
        self.threads
            .iter()
            .any(|t| t.tid == tid && t.pid == p.pid && !t.exited)
    }

    /// A file of /proc/<pid>/ made here (None for a path that isn't one)
    pub(crate) fn pid_file(&self, path: &[u8]) -> Option<Vec<u8>> {
        let (name, PidPath::Under(rest)) = pid_path(path)? else {
            return None;
        };
        let p = self.proc_named(name)?;
        // /proc/<pid>/task/<tid>/<file>
        if let Some(task) = rest.strip_prefix(b"task/") {
            let slash = task.iter().position(|&b| b == b'/')?;
            let tid: i64 = number(&task[..slash])?;
            if !self.has_thread(p, tid) {
                return None;
            }
            return Some(match &task[slash + 1..] {
                b"cmdline" => cmdline(p),
                b"comm" => comm(p).into_iter().chain(*b"\n").collect(),
                b"stat" => self.stat_line(p, Some(tid)).into_bytes(),
                b"status" => self.status_text(p, Some(tid)).into_bytes(),
                _ => return None,
            });
        }
        Some(match rest {
            b"cmdline" => cmdline(p),
            b"comm" => comm(p).into_iter().chain(*b"\n").collect(),
            b"stat" => self.stat_line(p, None).into_bytes(),
            b"status" => self.status_text(p, None).into_bytes(),
            b"statm" => self.statm(p).into_bytes(),
            b"environ" if p.zombie.is_none() => p.environ.clone(),
            b"auxv" if p.zombie.is_none() => p.auxv.clone(),
            b"environ" | b"auxv" => Vec::new(),
            b"maps" => self.maps(p).into_bytes(),
            b"io" => b"rchar: 0\nwchar: 0\nsyscr: 0\nsyscw: 0\nread_bytes: 0\nwrite_bytes: 0\ncancelled_write_bytes: 0\n".to_vec(),
            b"limits" => limits().into_bytes(),
            b"cgroup" => b"0::/\n".to_vec(),
            b"mounts" => MOUNTS.as_bytes().to_vec(),
            b"mountinfo" => MOUNTINFO.as_bytes().to_vec(),
            b"oom_score" | b"oom_score_adj" | b"wchan" => b"0\n".to_vec(),
            _ => return None,
        })
    }

    /// Where a link of /proc/<pid>/ (`exe`, `cwd`, `root`) points (None for a path that isn't one)
    pub(crate) fn pid_link(&self, path: &[u8]) -> Option<Vec<u8>> {
        let (name, PidPath::Under(rest)) = pid_path(path)? else {
            return None;
        };
        let p = self.proc_named(name)?;
        match rest {
            b"exe" => Some(p.exe.clone()),
            // (With no filesystem there is only the root)
            b"cwd" => Some(self.fs(|fs| files::path_of(fs, p.cwd)).unwrap_or_else(|| b"/".to_vec())),
            b"root" => Some(b"/".to_vec()),
            _ => None,
        }
    }

    /// The descriptor N a path /proc/<pid>/<dir>N names (`dir` "fd/" or "fdinfo/"), when it is
    /// the calling process's own (its own pid, `self` or `thread-self`)
    pub(crate) fn own_fd(&self, path: &[u8], dir: &[u8]) -> Option<i32> {
        let (name, PidPath::Under(rest)) = pid_path(path)? else {
            return None;
        };
        if self.proc_named(name)?.pid != self.p.pid {
            return None;
        }
        number(rest.strip_prefix(dir)?)
    }

    /// Whether `path` is /proc/<pid> (or self, or thread-self): Some(there is such a process),
    /// None for any other path
    pub(crate) fn pid_dir(&self, path: &[u8]) -> Option<bool> {
        match pid_path(path)? {
            (name, PidPath::Dir) => Some(self.proc_named(name).is_some()),
            _ => None,
        }
    }

    /// The entries of a directory made here (`Kind::VDir`), with their types, `.` and `..`
    /// first: /proc/<pid> and its `fd`, `fdinfo`, `task` and `task/<tid>`, and
    /// /sys/devices/system/cpu. None for a path that is no such directory.
    pub(crate) fn vdir(&self, path: &[u8]) -> Option<Vec<(Vec<u8>, u8)>> {
        let path = trim_slashes(path);
        let names = |ns: &[&[u8]], t: u8| -> Vec<(Vec<u8>, u8)> {
            ns.iter().map(|n| (n.to_vec(), t)).collect()
        };
        let mut out = vec![(b".".to_vec(), DT_DIR), (b"..".to_vec(), DT_DIR)];
        if path == b"/sys/devices/system/cpu" {
            out.extend(names(&[b"kernel_max", b"online", b"possible", b"present"], DT_REG));
            return Some(out);
        }
        let (name, under) = pid_path(path)?;
        let p = self.proc_named(name)?;
        match under {
            PidPath::Dir => {
                out.extend(names(&PID_FILES, DT_REG));
                out.extend(names(&PID_LINKS, DT_LNK));
                out.extend(names(&PID_DIRS, DT_DIR));
            },
            PidPath::Under(b"fd") => {
                let fds = self.fds_of(p).keys();
                out.extend(fds.map(|fd| (fd.to_string().into_bytes(), DT_LNK)));
            },
            PidPath::Under(b"fdinfo") => {
                let fds = self.fds_of(p).keys();
                out.extend(fds.map(|fd| (fd.to_string().into_bytes(), DT_REG)));
            },
            PidPath::Under(b"task") => {
                let tids = self.threads.iter().filter(|t| t.pid == p.pid && !t.exited);
                let mut tids: Vec<i64> = tids.map(|t| t.tid).collect();
                tids.sort_unstable();
                out.extend(tids.into_iter().map(|t| (t.to_string().into_bytes(), DT_DIR)));
            },
            PidPath::Under(rest) => {
                let tid: i64 = number(rest.strip_prefix(b"task/")?)?;
                if !self.has_thread(p, tid) {
                    return None;
                }
                out.extend(names(&TASK_FILES, DT_REG));
            },
        }
        Some(out)
    }

    /// getdents64 of a directory made here, `entries` (`vdir`), from `pos`
    pub(crate) fn vdir_dirents(
        entries: Vec<(Vec<u8>, u8)>,
        pos: u64,
        max: usize,
    ) -> Result<(Vec<u8>, u64), i64> {
        let all = entries
            .into_iter()
            .enumerate()
            .map(|(i, (name, t))| (PID_INO + (1 << 40) + i as u64, i as u64 + 1, t, name));
        encode_dirents(all.filter(|e| e.1 > pos), pos, max)
    }

    /// Whether `ino` is the filesystem's /proc, which getdents64 lists the processes in
    pub(crate) fn is_proc_dir(&self, ino: Ino) -> bool {
        self.fs(|fs| fs.lookup(ROOT, b"proc").ok() == Some(ino))
            .unwrap_or(false)
    }

    /// /proc made in the filesystem if it has none, for a program to open and list it
    pub(crate) fn make_proc_dir(&self) {
        self.fs(|fs| {
            if fs.lookup(ROOT, b"proc").is_err() {
                let _ = fs.create(ROOT, b"proc", armless::dev::fs::S_IFDIR | 0o555, 0, 0, 0);
            }
        });
    }

    /// getdents64 of /proc (`ino`) from `pos`: its entries in the filesystem, `self`, then a
    /// directory for each process
    pub(crate) fn proc_dirents(
        &self,
        ino: Ino,
        pos: u64,
        max: usize,
    ) -> Result<(Vec<u8>, u64), i64> {
        let own = self
            .fs(|fs| -> Result<Vec<_>, i64> {
                let entries = fs.list(ino).map_err(|_| -ENOTDIR)?;
                let n = entries.len() as u64;
                let mut own: Vec<_> = entries
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, ino))| {
                        (
                            ino,
                            i as u64 + 1,
                            fs.get(ino).map_or(0, |i| files::dtype(i.mode)),
                            name,
                        )
                    })
                    .collect();
                own.push((PID_INO, n + 1, DT_LNK, b"self".to_vec()));
                Ok(own)
            })
            .ok_or(-EBADF)??;
        let mut pids: Vec<i64> = self.proc_list().map(|p| p.pid).collect();
        pids.sort_unstable();
        let procs = pids.into_iter().map(|pid| {
            (
                PID_INO + pid as u64,
                PID_POS + pid as u64,
                DT_DIR,
                pid.to_string().into_bytes(),
            )
        });
        let after = own.into_iter().chain(procs).filter(|e| e.1 > pos);
        encode_dirents(after, pos, max)
    }

    /// The threads that run or may (not parked in a call, not ended): what the load averages
    /// and /proc/stat's procs_running count
    pub(crate) fn runnable(&self) -> usize {
        self.threads
            .iter()
            .filter(|t| !t.exited && t.wait.is_none())
            .count()
    }

    /// R for a process (or one of its threads, `tid`) that runs or may (the one that asks, or one
    /// with a thread that isn't parked in a call), S for one whose threads all wait, Z for one
    /// that ended
    fn state(&self, p: &Proc, tid: Option<i64>) -> (char, &'static str) {
        if p.zombie.is_some() {
            return ('Z', "zombie");
        }
        let current = self.threads.get(self.cur).map(|t| t.tid);
        let runnable = self.threads.iter().any(|t| {
            t.pid == p.pid
                && tid.is_none_or(|x| x == t.tid)
                && !t.exited
                && (t.wait.is_none() || current == Some(t.tid))
        });
        if (tid.is_none() && p.pid == self.p.pid) || runnable {
            ('R', "running")
        } else {
            ('S', "sleeping")
        }
    }

    /// Its threads that haven't ended (one for a process that has, as Linux says)
    fn thread_count(&self, p: &Proc) -> usize {
        let n = self
            .threads
            .iter()
            .filter(|t| t.pid == p.pid && !t.exited)
            .count();
        n.max(1)
    }

    /// The CPU a thread of `p` (thread `tid`, or its first) is on, or last was: 0 when none is
    fn processor(&self, p: &Proc, tid: Option<i64>) -> usize {
        self.on_cpu
            .iter()
            .position(|on| {
                on.and_then(|i| self.threads.get(i)).is_some_and(|t| {
                    t.pid == p.pid && tid.is_none_or(|x| x == t.tid)
                })
            })
            .unwrap_or(0)
    }

    /// /proc/<pid>/stat (or a thread's, `tid`): its 52 fields, zero where nothing is known
    /// (faults, signals, addresses)
    fn stat_line(&self, p: &Proc, tid: Option<i64>) -> String {
        let (state, _) = self.state(p, tid);
        let comm = String::from_utf8_lossy(&comm(p)).into_owned();
        let threads = self.thread_count(p);
        let (st, rss, _) = self.mem_of(p);
        let cpu = match tid {
            Some(t) => self.timers.thread_cpu_ns(t),
            None => self.timers.proc_cpu_ns(p.pid),
        };
        let nice = self
            .threads
            .iter()
            .find(|t| t.pid == p.pid && tid.is_none_or(|x| x == t.tid))
            .map_or(0, |t| t.sched.nice);
        // pid (comm) state ppid pgrp session tty_nr tpgid flags; minflt cminflt majflt cmajflt;
        // utime stime cutime cstime; priority nice num_threads itrealvalue starttime; vsize
        // (bytes) rss (pages) rsslim
        let mut s = format!(
            "{} ({comm}) {state} {} {} {} 0 -1 0 0 0 0 0 {} 0 {} 0 {} {nice} {threads} 0 {} {} {rss} {}",
            tid.unwrap_or(p.pid),
            p.ppid,
            p.pgid,
            p.sid,
            ticks(cpu),
            ticks(p.child_cpu),
            20 + nice,
            ticks(p.started),
            st.size,
            u64::MAX,
        );
        // startcode to cnswap (12), exit_signal, processor, then rt_priority to exit_code (13)
        s += &" 0".repeat(12);
        s += &format!(" {} {}", p.exit_signal, self.processor(p, tid));
        s += &" 0".repeat(13);
        s.push('\n');
        s
    }

    /// /proc/<pid>/statm: size resident shared text lib data dt, in pages
    fn statm(&self, p: &Proc) -> String {
        let (st, rss, _) = self.mem_of(p);
        let pages = |b: u64| b / crate::PAGE;
        format!(
            "{} {rss} {} {} 0 {} 0\n",
            pages(st.size),
            st.file + st.shmem,
            pages(st.exe),
            pages(st.data + st.stack)
        )
    }

    /// /proc/<pid>/status (or a thread's, `tid`): the lines busybox, procps and htop read
    fn status_text(&self, p: &Proc, tid: Option<i64>) -> String {
        let (state, word) = self.state(p, tid);
        let (st, rss, hwm) = self.mem_of(p);
        let kb = |b: u64| b / 1024;
        let pk = |n: u64| n * (crate::PAGE / 1024);
        // (A process that ended has no memory: Linux leaves these lines out)
        let vm = if p.zombie.is_some() {
            String::new()
        } else {
            format!(
                "VmPeak:\t{} kB\nVmSize:\t{} kB\nVmLck:\t0 kB\nVmPin:\t0 kB\nVmHWM:\t{} kB\nVmRSS:\t{} kB\nRssAnon:\t{} kB\nRssFile:\t{} kB\nRssShmem:\t{} kB\nVmData:\t{} kB\nVmStk:\t{} kB\nVmExe:\t{} kB\nVmLib:\t0 kB\nVmPTE:\t0 kB\nVmSwap:\t0 kB\n",
                kb(st.peak),
                kb(st.size),
                pk(hwm),
                pk(rss),
                pk(st.anon),
                pk(st.file),
                pk(st.shmem),
                kb(st.data),
                kb(st.stack),
                kb(st.exe),
            )
        };
        let ids = |v: [u32; 3]| format!("{}\t{}\t{}\t{}", v[0], v[1], v[2], v[1]);
        let groups: String = p.groups.iter().map(|g| format!("{g} ")).collect();
        let cpus = self.ncpus_reported();
        let mask = if cpus >= 64 { u64::MAX } else { (1u64 << cpus) - 1 };
        format!(
            "Name:\t{}\nUmask:\t{:04o}\nState:\t{state} ({word})\nTgid:\t{pid}\nNgid:\t0\nPid:\t{}\nPPid:\t{}\nTracerPid:\t0\nUid:\t{}\nGid:\t{}\nFDSize:\t64\nGroups:\t{groups}\nNStgid:\t{pid}\nNSpid:\t{pid}\nNSpgid:\t{}\nNSsid:\t{}\n{vm}Threads:\t{}\nNoNewPrivs:\t0\nSeccomp:\t0\nCpus_allowed:\t{mask:x}\nCpus_allowed_list:\t0-{}\nMems_allowed:\t1\nMems_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n",
            String::from_utf8_lossy(&comm(p)),
            p.umask,
            tid.unwrap_or(p.pid),
            p.ppid,
            ids(p.creds.uid),
            ids(p.creds.gid),
            p.pgid,
            p.sid,
            self.thread_count(p),
            cpus - 1,
            pid = p.pid,
        )
    }

    /// /proc/<pid>/maps: its areas, with the file each maps and `[heap]` and `[stack]`
    fn maps(&self, p: &Proc) -> String {
        if p.zombie.is_some() {
            return String::new();
        }
        let m = p.mm.borrow();
        let stack = (
            crate::mm::VA_END - PAGE - crate::STACK_SIZE,
            crate::mm::VA_END - PAGE,
        );
        let mut s = String::new();
        for (&start, v) in &m.vmas {
            let perms = format!(
                "{}{}{}{}",
                if v.prot & PROT_READ != 0 { 'r' } else { '-' },
                if v.prot & PROT_WRITE != 0 { 'w' } else { '-' },
                if v.prot & PROT_EXEC != 0 { 'x' } else { '-' },
                if v.shared { 's' } else { 'p' },
            );
            let (off, ino, name) = match &v.file {
                Some(f) => {
                    let path = self.fs(|fs| files::find_path(fs, f.ino)).flatten();
                    (f.off, f.ino, path.map(|p| String::from_utf8_lossy(&p).into_owned()))
                },
                None if start < stack.1 && v.end > stack.0 => (0, 0, Some("[stack]".into())),
                None if start >= m.brk_start && start < m.brk.max(m.brk_start + 1) => {
                    (0, 0, Some("[heap]".into()))
                },
                None => (0, 0, None),
            };
            let line = format!("{start:08x}-{:08x} {perms} {off:08x} 00:00 {ino}", v.end);
            match name {
                Some(n) => s += &format!("{line:<73}{n}\n"),
                None => s += &format!("{line}\n"),
            }
        }
        s
    }

    /// A file of /proc or /sys about the machine (None for a path that isn't one)
    pub(crate) fn sys_file(&self, path: &[u8]) -> Option<Vec<u8>> {
        let cpus = self.ncpus_reported();
        let range = format!("0-{}\n", cpus - 1);
        Some(match path {
            b"/proc/stat" => self.proc_stat().into_bytes(),
            b"/proc/uptime" => {
                let up = self.now_ns(1);
                let idle: u64 = (0..cpus as usize).map(|c| up.saturating_sub(self.timers.sys.busy(c))).sum();
                format!("{}.{:02} {}.{:02}\n", up / 1_000_000_000, ticks(up) % HZ, idle / 1_000_000_000, ticks(idle) % HZ).into_bytes()
            },
            b"/proc/loadavg" => {
                let n = self.runnable();
                let [a, b, c] = self.timers.sys.load(crate::host::Instant::now(), n);
                let total = self.threads.iter().filter(|t| !t.exited).count();
                format!("{a:.2} {b:.2} {c:.2} {n}/{total} {}\n", self.next_id - 1).into_bytes()
            },
            b"/proc/meminfo" => self.meminfo().into_bytes(),
            b"/proc/version" => format!("Linux version {RELEASE} (armless) {VERSION}\n").into_bytes(),
            b"/proc/filesystems" => b"nodev\tproc\nnodev\ttmpfs\nnodev\t9p\n".to_vec(),
            b"/proc/mounts" => MOUNTS.as_bytes().to_vec(),
            b"/proc/sys/kernel/ostype" => b"Linux\n".to_vec(),
            b"/proc/sys/kernel/osrelease" => format!("{RELEASE}\n").into_bytes(),
            b"/proc/sys/kernel/version" => format!("{VERSION}\n").into_bytes(),
            b"/proc/sys/kernel/hostname" => format!("{}\n", self.hostname).into_bytes(),
            b"/proc/sys/kernel/pid_max" => b"4194304\n".to_vec(),
            b"/proc/sys/kernel/threads-max" => b"32768\n".to_vec(),
            b"/proc/sys/kernel/random/boot_id" | b"/proc/sys/kernel/random/uuid" => uuid().into_bytes(),
            b"/proc/sys/kernel/random/entropy_avail" => b"256\n".to_vec(),
            b"/proc/sys/vm/overcommit_memory" => b"0\n".to_vec(),
            b"/proc/sys/vm/max_map_count" => b"65530\n".to_vec(),
            b"/proc/sys/fs/file-max" | b"/proc/sys/fs/nr_open" => format!("{}\n", files::NOFILE).into_bytes(),
            b"/sys/devices/system/cpu/online" | b"/sys/devices/system/cpu/possible" | b"/sys/devices/system/cpu/present" => range.into_bytes(),
            b"/sys/devices/system/cpu/kernel_max" => format!("{}\n", cpus - 1).into_bytes(),
            b"/sys/kernel/mm/transparent_hugepage/enabled" => b"always madvise [never]\n".to_vec(),
            b"/sys/kernel/mm/transparent_hugepage/hpage_pmd_size" => b"2097152\n".to_vec(),
            b"/sys/fs/cgroup/cpu.max" => b"max 100000\n".to_vec(),
            b"/sys/fs/cgroup/memory.max" | b"/sys/fs/cgroup/memory.high" | b"/sys/fs/cgroup/memory.swap.max" | b"/sys/fs/cgroup/pids.max" => b"max\n".to_vec(),
            b"/sys/fs/cgroup/memory.current" => format!("{}\n", self.ram_size - self.ram_free()).into_bytes(),
            b"/sys/fs/cgroup/cgroup.controllers" => b"cpu memory pids\n".to_vec(),
            _ => return None,
        })
    }

    /// /proc/stat: each CPU's time (user: the time it ran threads; idle: the rest of the time
    /// since the machine started), and the counts top and vmstat read
    fn proc_stat(&self) -> String {
        let up = ticks(self.now_ns(1));
        let line = |name: &str, user: u64, idle: u64| {
            format!("{name} {user} 0 0 {idle} 0 0 0 0 0 0\n")
        };
        let per: Vec<(u64, u64)> = (0..self.ncpus_reported() as usize)
            .map(|c| {
                let user = ticks(self.timers.sys.busy(c)).min(up);
                (user, up - user)
            })
            .collect();
        let (user, idle) = per.iter().fold((0, 0), |(u, i), &(a, b)| (u + a, i + b));
        let mut s = line("cpu ", user, idle);
        for (c, &(u, i)) in per.iter().enumerate() {
            s += &line(&format!("cpu{c}"), u, i);
        }
        let btime = self.now_secs().saturating_sub(up / HZ);
        s += &format!(
            "intr 0\nctxt {}\nbtime {btime}\nprocesses {}\nprocs_running {}\nprocs_blocked 0\nsoftirq 0 0 0 0 0 0 0 0 0 0 0\n",
            self.stats.switches,
            self.next_id - 1,
            self.runnable().max(1),
        );
        s
    }

    /// /proc/meminfo: the machine's RAM, what of it holds files' pages (Cached) and what
    /// programs' own (AnonPages); there is no swap
    fn meminfo(&self) -> String {
        let kb = |b: u64| b / 1024;
        let total = self.ram_size;
        let free = self.ram_free();
        let cached = (self.file_pages.borrow().len() + self.page_cache.borrow().len()) as u64 * PAGE;
        let used = total.saturating_sub(free);
        let anon = used.saturating_sub(cached);
        let lines: [(&str, u64); 30] = [
            ("MemTotal", kb(total)),
            ("MemFree", kb(free)),
            ("MemAvailable", kb(free)),
            ("Buffers", 0),
            ("Cached", kb(cached)),
            ("SwapCached", 0),
            ("Active", kb(used)),
            ("Inactive", 0),
            ("Active(anon)", kb(anon)),
            ("Inactive(anon)", 0),
            ("Active(file)", kb(cached)),
            ("Inactive(file)", 0),
            ("Unevictable", 0),
            ("Mlocked", 0),
            ("SwapTotal", 0),
            ("SwapFree", 0),
            ("Dirty", 0),
            ("Writeback", 0),
            ("AnonPages", kb(anon)),
            ("Mapped", kb(cached)),
            ("Shmem", 0),
            ("KReclaimable", 0),
            ("Slab", 0),
            ("SReclaimable", 0),
            ("SUnreclaim", 0),
            ("KernelStack", 0),
            ("PageTables", 0),
            ("CommitLimit", kb(total)),
            ("Committed_AS", kb(used)),
            ("Hugepagesize", 2048),
        ];
        lines
            .iter()
            .map(|(name, v)| format!("{:<16}{v:>8} kB\n", format!("{name}:")))
            .collect()
    }
}

/// /proc/<pid>/limits: the limits getrlimit gives (sys/misc.rs), as Linux lists them
fn limits() -> String {
    let v = |l: Option<u64>| l.map_or("unlimited".to_string(), |n| n.to_string());
    let mut s = format!("{:<26}{:<21}{:<21}{:<10}\n", "Limit", "Soft Limit", "Hard Limit", "Units");
    for (name, soft, hard, units) in LIMITS {
        s += &format!("{name:<26}{:<21}{:<21}{units:<10}\n", v(soft), v(hard));
    }
    s
}

/// A random UUID (version 4), as /proc/sys/kernel/random/uuid gives a new one each read
fn uuid() -> String {
    let mut b = [0u8; 16];
    b.copy_from_slice(&crate::host::random_bytes(16));
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}\n", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Whether a name is a number, as a process's directory is named
fn is_pid(name: &[u8]) -> bool {
    !name.is_empty() && name.iter().all(u8::is_ascii_digit)
}

/// /proc/<pid>/cmdline: its arguments, each with its NUL; nothing once it has ended, and its
/// executable's path when no arguments were kept (a state saved before they were)
fn cmdline(p: &Proc) -> Vec<u8> {
    if p.zombie.is_some() {
        Vec::new()
    } else if p.cmdline.is_empty() {
        p.exe.iter().copied().chain([0]).collect()
    } else {
        p.cmdline.clone()
    }
}

/// Its name: the last component of argv[0] (of the executable's path if it has none), cut to
/// 15 bytes. (`sleep` for busybox's applet, which Linux too names by what was run.)
fn comm(p: &Proc) -> Vec<u8> {
    comm_of(&p.cmdline, &p.exe)
}

fn comm_of(cmdline: &[u8], exe: &[u8]) -> Vec<u8> {
    let arg0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
    let path = if arg0.is_empty() { exe } else { arg0 };
    let base = path.rsplit(|&b| b == b'/').next().unwrap_or_default();
    base[..base.len().min(COMM_LEN)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comm_is_argv0s_last_component_cut_to_15_bytes() {
        assert_eq!(comm_of(b"sleep\x004167\0", b"/bin/busybox"), b"sleep");
        assert_eq!(comm_of(b"/usr/bin/prog\0x\0", b"/x"), b"prog");
        assert_eq!(
            comm_of(b"", b"/usr/lib/postgresql/bin/postgres"),
            b"postgres"
        );
        assert_eq!(
            comm_of(b"/a/a-very-long-program-name\0", b""),
            b"a-very-long-pro"
        );
    }

    #[test]
    fn pid_paths_name_a_process_and_what_is_under_it() {
        assert!(matches!(pid_path(b"/proc/self"), Some((b"self", PidPath::Dir))));
        assert!(matches!(pid_path(b"/proc/12/"), Some((b"12", PidPath::Dir))));
        assert!(matches!(
            pid_path(b"/proc/12/task/13/stat"),
            Some((b"12", PidPath::Under(b"task/13/stat")))
        ));
        assert!(matches!(
            pid_path(b"/proc/thread-self/fd//"),
            Some((b"thread-self", PidPath::Under(b"fd")))
        ));
        assert!(pid_path(b"/proc/stat").is_none());
        assert!(pid_path(b"/proc/").is_none());
        assert!(pid_path(b"/sys/12").is_none());
    }

    #[test]
    fn slashes_at_the_end_go_but_root_stays() {
        assert_eq!(trim_slashes(b"/proc//"), b"/proc");
        assert_eq!(trim_slashes(b"/"), b"/");
        assert_eq!(trim_slashes(b"//"), b"/");
        assert_eq!(trim_slashes(b""), b"");
        assert_eq!(trim_slashes(b"a"), b"a");
    }

    #[test]
    fn limits_are_a_table_of_16_with_a_header() {
        let l = limits();
        let lines: Vec<&str> = l.lines().collect();
        assert_eq!(lines.len(), 17);
        assert!(lines[0].starts_with("Limit                     Soft Limit"));
        assert!(l.contains("Max open files            4096                 4096                 files"));
        assert!(l.contains("Max stack size            8388608              unlimited            bytes"));
    }

    #[test]
    fn a_uuid_is_version_4() {
        let u = uuid();
        assert_eq!(u.len(), 37);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
        assert_ne!(u, uuid());
    }
}
