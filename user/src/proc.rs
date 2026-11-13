//! Processes: pids, the tree they make, and what ends them and waits for them.
//!
//! A process is its address space, descriptors, working directory and signal actions (`Proc`),
//! and one or more threads; the kernel's state (the descriptions the descriptors refer to, the
//! pipes, the terminal, the clock) is shared by all. `Process::p` is the process that runs, the
//! others are in `Process::procs`; `switch_to` makes another the one that runs, as the run loop
//! does for every thread it dispatches (the process's descriptors are then in `Process::files`).
//! Threads of all processes are in `Process::threads`, taking turns on the one CPU.
//!
//! A process ends (`end_process`) by `exit_group` (or its last thread's `exit`), or from a
//! signal's default action, or from a fault it didn't handle (`crash`: SIGSEGV; the first
//! process's fault stops the machine as it did when there was only one). Its descriptors are
//! closed, its address space given back unless a vfork parent still has it, its children go to
//! the first process, its parent gets its exit signal (SIGCHLD) and the wait status stays for
//! `wait4` and `waitid` (a zombie) unless the parent ignores SIGCHLD. The machine ends when the
//! first process does.
//!
//! `clone` (and `clone3`) makes a thread (CLONE_THREAD) or a process: a copy-on-write copy of
//! the address space (`Mm::fork`), or the same one (CLONE_VM, with CLONE_VFORK its parent waits
//! until the child execs or ends). The descriptor table is always copied, never shared
//! (CLONE_FILES), and so are the signal actions and the working directory.
//!
//! A pidfd (`pidfd_open`, or CLONE_PIDFD) is a description of `Kind::Pidfd`: the process by its
//! pid, which is never reused. It is readable once the process has ended, and hung up as well once
//! it is waited for; `pidfd_send_signal` signals it, `pidfd_getfd` takes a description of its
//! table, and `waitid(P_PIDFD)` waits for it (one with PIDFD_NONBLOCK never waits).

use std::cell::RefCell;
use std::rc::Rc;

use armless::cpu::Cpu;
use armless::Host;

use crate::files::Kind;
use crate::mm::Mm;
use crate::signal::{self, ThreadSig};
use crate::{Ctx, Proc, Process, Ret, Thread, Wait, INIT};

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ECHILD: i64 = 10;
const EINVAL: i64 = 22;

const SIGSEGV: u32 = 11;

pub(crate) const CLONE_VM: u64 = 0x100;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
pub(crate) const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
pub(crate) const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
/// CLONE_NEWNS, NEWCGROUP, NEWUTS, NEWIPC, NEWUSER, NEWPID, NEWNET
const CLONE_NAMESPACES: u64 =
    0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;
/// What clone ignores: CLONE_FS, FILES (the table is copied), PTRACE, SYSVSEM, UNTRACED,
/// DETACHED, IO, and CLONE_SIGHAND's other side
const CLONE_IGNORED: u64 = 0x200 | 0x400 | 0x2000 | 0x40000 | 0x40_0000 | 0x80_0000 | 0x8000_0000;

const WNOHANG: u32 = 1;
const WSTOPPED: u32 = 2;
const WEXITED: u32 = 4;
const WCONTINUED: u32 = 8;
const WNOWAIT: u32 = 0x0100_0000;
const WNOTHREAD: u32 = 0x2000_0000;
const WALL: u32 = 0x4000_0000;
const WCLONE: u32 = 0x8000_0000;

/// How a process ended.
pub(crate) enum End {
    /// exit, exit_group
    Exit(i32),
    /// Killed by this signal
    Signal(u32),
}

/// What clone and clone3 ask for.
pub(crate) struct CloneArgs {
    pub flags: u64,
    pub exit_signal: u32,
    /// The child's stack pointer, if given
    pub sp: u64,
    pub ptid: u64,
    pub ctid: u64,
    pub tls: u64,
    /// Where CLONE_PIDFD puts the pidfd (clone's is `ptid`)
    pub pidfd: u64,
}

/// Which children wait4 and waitid pick.
#[derive(Clone, Copy)]
enum Pick {
    Any,
    Pid(i64),
    Group(i64),
}

impl Process {
    // ------------------------------------------------------------ the table

    pub(crate) fn proc_ref(&self, pid: i64) -> Option<&Proc> {
        if self.p.pid == pid {
            Some(&self.p)
        } else {
            self.procs.get(&pid)
        }
    }

    pub(crate) fn proc_mut(&mut self, pid: i64) -> Option<&mut Proc> {
        if self.p.pid == pid {
            Some(&mut self.p)
        } else {
            self.procs.get_mut(&pid)
        }
    }

    /// Every process, the one that runs first (the ended ones that haven't been waited for too).
    pub(crate) fn proc_list(&self) -> impl Iterator<Item = &Proc> {
        std::iter::once(&self.p).chain(self.procs.values())
    }

    /// The process that runs is `pid`: its address space, descriptors, signal actions.
    pub(crate) fn switch_to(&mut self, pid: i64) {
        if self.p.pid == pid {
            return;
        }
        let Some(mut next) = self.procs.remove(&pid) else {
            return;
        };
        self.p.fds = std::mem::take(&mut self.files.fds);
        self.files.fds = std::mem::take(&mut next.fds);
        std::mem::swap(&mut self.p, &mut next);
        // `next` is the one that ran; nobody waits for it if it ended with its parent ignoring that
        if !next.reap {
            self.procs.insert(next.pid, next);
        }
    }

    /// Takes the threads that ended out of the list, keeping the others' places.
    pub(crate) fn compact(&mut self) {
        if !self.threads.iter().any(|t| t.exited) {
            return;
        }
        let mut moved = Vec::with_capacity(self.threads.len());
        let mut kept = 0;
        for t in &self.threads {
            moved.push((!t.exited).then(|| {
                kept += 1;
                kept - 1
            }));
        }
        let n = self.threads.len();
        // The next thread to run: the one that was, or the first of those after it
        self.cur = (0..n)
            .map(|i| (self.cur + i) % n)
            .find_map(|i| moved[i])
            .unwrap_or(0);
        self.loaded = self.loaded.and_then(|l| moved[l]);
        self.threads.retain(|t| !t.exited);
    }

    // ------------------------------------------------------------ ending

    /// A thread of the process that runs ends (`exit`): the process ends with it if it was the last.
    pub(crate) fn thread_exit(&mut self, i: usize, code: i32) {
        let pid = self.threads[i].pid;
        let ct = self.threads[i].clear_tid;
        if ct != 0 {
            self.write(ct, &0u32.to_le_bytes());
            self.futex_wake(self.futex_key(ct, false), 1, u32::MAX);
        }
        self.threads[i].exited = true;
        if self.loaded == Some(i) {
            self.loaded = None;
        }
        // What it had pending goes to the threads left
        let left = std::mem::take(&mut self.threads[i].sig.pending);
        if let Some(p) = self.proc_mut(pid) {
            for (s, c) in left {
                p.sig.pending.entry(s).or_insert(c);
            }
        }
        if !self.threads.iter().any(|t| t.pid == pid && !t.exited) {
            self.end_process(pid, End::Exit(code));
        }
    }

    /// The process that runs did what it can't: it dies of SIGSEGV. The first process's fault
    /// is the machine's (`Status::Fault`).
    pub(crate) fn crash(&mut self, why: String) {
        if self.p.pid == INIT {
            self.fault = Some(why);
            return;
        }
        self.stats.faults.push(why);
        let pid = self.p.pid;
        self.end_process(pid, End::Signal(SIGSEGV));
    }

    /// Process `pid` ends: see the module's doc. (The first process's end is the machine's.)
    pub(crate) fn end_process(&mut self, pid: i64, how: End) {
        if pid == INIT {
            self.exit_code = Some(match how {
                End::Exit(c) => c,
                End::Signal(s) => 128 + s as i32,
            });
            return;
        }
        self.switch_to(pid);
        if self.p.pid != pid || self.p.zombie.is_some() {
            return;
        }
        for (i, t) in self.threads.iter_mut().enumerate() {
            if t.pid == pid {
                t.exited = true;
                if self.loaded == Some(i) {
                    self.loaded = None;
                }
            }
        }
        let fds: Vec<i32> = self.files.fds.keys().copied().collect();
        for fd in fds {
            self.close_fd(fd);
        }
        // The address space goes with its last user (a vfork parent may still have it)
        let mm = std::mem::replace(&mut self.p.mm, Rc::new(RefCell::new(Mm::husk())));
        if Rc::strong_count(&mm) == 1 {
            let refs = {
                let mut m = mm.borrow_mut();
                m.destroy(&mut self.frames.borrow_mut());
                m.take_refs()
            };
            self.settle_refs(refs);
            self.trim_file_pages();
        }
        drop(mm);
        self.p.sig.pending.clear();
        self.release_vfork(pid);
        for q in self.procs.values_mut() {
            if q.ppid == pid {
                q.ppid = INIT;
            }
        }
        let status = match how {
            End::Exit(c) => (c & 0xff) << 8,
            End::Signal(s) => s as i32,
        };
        self.p.zombie = Some(status);
        let (ppid, exit_signal) = (self.p.ppid, self.p.exit_signal);
        if self.proc_ref(ppid).is_some_and(|q| q.sig.children_vanish()) {
            self.p.reap = true;
        }
        if exit_signal != 0 {
            let code = match how {
                End::Exit(_) => 1,   // CLD_EXITED
                End::Signal(_) => 2, // CLD_KILLED
            };
            self.raise_in(ppid, exit_signal, None, code);
        }
        // Those waiting for it look again
        self.poke();
    }

    /// The parent that is suspended until process `child` execs or ends (vfork) goes on.
    pub(crate) fn release_vfork(&mut self, child: i64) {
        for t in &mut self.threads {
            if matches!(t.wait, Some((Wait::Vfork(c), _)) if c == child) {
                t.wait = None;
                t.ctx.x[0] = child as u64;
            }
        }
    }

    /// A signal from the terminal (a key, a resize) goes to its foreground process group.
    pub(crate) fn signal_foreground(&mut self, sig: u32) {
        let group = self.tty.pgrp;
        let pids: Vec<i64> = self
            .proc_list()
            .filter(|p| p.zombie.is_none() && p.pgid == group)
            .map(|p| p.pid)
            .collect();
        for pid in pids {
            self.raise_in(pid, sig, None, signal::SI_KERNEL);
        }
    }

    // ------------------------------------------------------------ waiting

    /// The children `pick` selects, and the first of them that has ended: (pid, wait status).
    /// None when there are no such children.
    fn children(&self, pick: Pick) -> Option<Option<(i64, i32)>> {
        let me = self.p.pid;
        let mut any = false;
        for c in self.proc_list().filter(|c| c.ppid == me && c.pid != me) {
            let wanted = match pick {
                Pick::Any => true,
                Pick::Pid(p) => c.pid == p,
                Pick::Group(g) => c.pgid == g,
            };
            if !wanted {
                continue;
            }
            any = true;
            if let Some(status) = c.zombie {
                return Some(Some((c.pid, status)));
            }
        }
        any.then_some(None)
    }

    /// wait4(pid, status, options, rusage)
    pub(crate) fn sys_wait4(&mut self, a: &[u64; 6]) -> Ret {
        let (pid, status, options, rusage) = (a[0] as i32 as i64, a[1], a[2] as u32, a[3]);
        if options & !(WNOHANG | WSTOPPED | WCONTINUED | WNOTHREAD | WALL | WCLONE) != 0 {
            return Ret::Val(-EINVAL);
        }
        let pick = match pid {
            -1 => Pick::Any,
            0 => Pick::Group(self.p.pgid),
            p if p > 0 => Pick::Pid(p),
            p => Pick::Group(-p),
        };
        match self.children(pick) {
            None => Ret::Val(-ECHILD),
            Some(None) if options & WNOHANG != 0 => Ret::Val(0),
            Some(None) => Ret::Retry {
                deadline: None,
                on_timeout: 0,
            },
            Some(Some((child, st))) => {
                if status != 0 && !self.put_u32(status, st as u32) {
                    return Ret::Val(-EFAULT);
                }
                if rusage != 0 && !self.write(rusage, &[0u8; 144]) {
                    return Ret::Val(-EFAULT);
                }
                self.procs.remove(&child);
                Ret::Val(child)
            },
        }
    }

    /// pidfd_open(pid, flags): a descriptor of process `pid`, readable once it has ended (the
    /// Supabase CLI watches its stack's owner so)
    pub(crate) fn sys_pidfd_open(&mut self, pid: i64, flags: u64) -> Ret {
        const PIDFD_NONBLOCK: u64 = 0o4000;
        if flags & !PIDFD_NONBLOCK != 0 || pid <= 0 {
            return Ret::Val(-EINVAL);
        }
        if self.proc_ref(pid).is_none_or(|p| p.zombie.is_some()) {
            return Ret::Val(-3); // ESRCH
        }
        let d = self
            .files
            .add(crate::files::Kind::Pidfd { pid }, flags as u32 & PIDFD_NONBLOCK as u32);
        Ret::Val(self.files.install(d, 0, true) as i64)
    }

    /// pidfd_send_signal(pidfd, sig, info, flags): kill for the process a pidfd is of
    pub(crate) fn sys_pidfd_send_signal(&mut self, fd: i32, sig: u32) -> Ret {
        let pid = match self.files.get(fd).map(|d| &d.kind) {
            Some(crate::files::Kind::Pidfd { pid }) => *pid,
            Some(_) => return Ret::Val(-EINVAL),
            None => return Ret::Val(-9), // EBADF
        };
        if self.proc_ref(pid).is_none_or(|p| p.zombie.is_some()) {
            return Ret::Val(-3); // ESRCH
        }
        if sig > 64 {
            return Ret::Val(-EINVAL);
        }
        if sig != 0 {
            self.raise_in(pid, sig, None, 0); // SI_USER
        }
        Ret::Val(0)
    }

    /// pidfd_getfd(pidfd, targetfd, flags): a descriptor here, close-on-exec, of the description
    /// the pidfd's process has at `targetfd` (as ptrace allows: root, or the same user)
    pub(crate) fn sys_pidfd_getfd(&mut self, fd: i32, target: i32, flags: u64) -> Ret {
        if flags != 0 {
            return Ret::Val(-EINVAL);
        }
        let pid = match self.files.get(fd).map(|d| &d.kind) {
            Some(crate::files::Kind::Pidfd { pid }) => *pid,
            Some(_) => return Ret::Val(-EINVAL),
            None => return Ret::Val(-EBADF),
        };
        let Some(p) = self.proc_ref(pid).filter(|p| p.zombie.is_none()) else {
            return Ret::Val(-ESRCH);
        };
        let me = self.p.creds;
        if me.euid() != 0 && me.euid() != p.creds.uid[0] {
            return Ret::Val(-EPERM);
        }
        // The process that runs has its descriptors in `files`, the others in their Proc
        let desc = if pid == self.p.pid {
            self.files.desc_of(target)
        } else {
            p.fds.get(&target).map(|&(d, _)| d)
        };
        let Some(d) = desc else {
            return Ret::Val(-EBADF);
        };
        Ret::Val(self.files.install(d, 0, true) as i64)
    }

    /// waitid(idtype, id, infop, options, rusage)
    pub(crate) fn sys_waitid(&mut self, a: &[u64; 6]) -> Ret {
        let (kind, id, info, options, rusage) = (a[0], a[1] as i32 as i64, a[2], a[3] as u32, a[4]);
        if options
            & !(WNOHANG | WSTOPPED | WEXITED | WCONTINUED | WNOWAIT | WNOTHREAD | WALL | WCLONE)
            != 0
            || options & WEXITED == 0
        {
            return Ret::Val(-EINVAL);
        }
        let pick = match kind {
            0 => Pick::Any,
            1 => Pick::Pid(id),
            2 => Pick::Group(if id == 0 { self.p.pgid } else { id }),
            // P_PIDFD: the process a pidfd is of
            3 => match self.files.get(id as i32).map(|d| &d.kind) {
                Some(crate::files::Kind::Pidfd { pid }) => Pick::Pid(*pid),
                Some(_) => return Ret::Val(-EINVAL),
                None => return Ret::Val(-EBADF),
            },
            _ => return Ret::Val(-EINVAL),
        };
        // A pidfd opened with PIDFD_NONBLOCK never waits for its process (EAGAIN)
        let nonblock = kind == 3 && self.nonblock(id as i32);
        match self.children(pick) {
            None => Ret::Val(-ECHILD),
            Some(None) if nonblock && options & WNOHANG == 0 => Ret::Val(-EAGAIN),
            Some(None) if options & WNOHANG != 0 => {
                // Nothing yet: its siginfo says so (si_signo and si_pid zero)
                if info != 0 && !self.write(info, &[0u8; 128]) {
                    return Ret::Val(-EFAULT);
                }
                Ret::Val(0)
            },
            Some(None) => Ret::Retry {
                deadline: None,
                on_timeout: 0,
            },
            Some(Some((child, st))) => {
                if info != 0 {
                    let mut b = [0u8; 128];
                    b[0..4].copy_from_slice(&(signal::SIGCHLD as i32).to_le_bytes());
                    let (code, value): (i32, i32) = if st & 0x7f == 0 {
                        (1, st >> 8) // CLD_EXITED, the exit code
                    } else {
                        (2, st & 0x7f) // CLD_KILLED, the signal
                    };
                    b[8..12].copy_from_slice(&code.to_le_bytes());
                    b[16..20].copy_from_slice(&(child as i32).to_le_bytes());
                    b[24..28].copy_from_slice(&value.to_le_bytes());
                    if !self.write(info, &b) {
                        return Ret::Val(-EFAULT);
                    }
                }
                if rusage != 0 && !self.write(rusage, &[0u8; 144]) {
                    return Ret::Val(-EFAULT);
                }
                if options & WNOWAIT == 0 {
                    self.procs.remove(&child);
                }
                Ret::Val(0)
            },
        }
    }

    // ------------------------------------------------------------ groups and sessions

    /// getpgid (155) and getsid (156) of `pid` (0: this process): a field of it
    pub(crate) fn sys_getpgid(&self, nr: u64, pid: i64) -> Ret {
        let pid = if pid == 0 { self.p.pid } else { pid };
        match self.proc_ref(pid).filter(|p| p.zombie.is_none()) {
            Some(p) => Ret::Val(if nr == 155 { p.pgid } else { p.sid }),
            None => Ret::Val(-ESRCH),
        }
    }

    /// setsid: a new session and group of this process (not if it leads a group already)
    pub(crate) fn sys_setsid(&mut self) -> Ret {
        let me = self.p.pid;
        if self.proc_list().any(|p| p.zombie.is_none() && p.pgid == me) {
            return Ret::Val(-EPERM);
        }
        self.p.pgid = me;
        self.p.sid = me;
        Ret::Val(me)
    }

    /// setpgid(pid, pgid): this process or a child of it into a group of its session
    pub(crate) fn sys_setpgid(&mut self, pid: i64, pgid: i64) -> Ret {
        let me = self.p.pid;
        let pid = if pid == 0 { me } else { pid };
        let Some(target) = self
            .proc_ref(pid)
            .filter(|p| p.zombie.is_none() && (p.pid == me || p.ppid == me))
        else {
            return Ret::Val(-ESRCH);
        };
        let pgid = if pgid == 0 { pid } else { pgid };
        if pgid < 0 || target.sid == target.pid {
            return Ret::Val(if pgid < 0 { -EINVAL } else { -EPERM });
        }
        let sid = target.sid;
        // Its own group, or one of its session that exists
        if pgid != pid
            && !self
                .proc_list()
                .any(|p| p.zombie.is_none() && p.pgid == pgid && p.sid == sid)
        {
            return Ret::Val(-EPERM);
        }
        if let Some(p) = self.proc_mut(pid) {
            p.pgid = pgid;
        }
        Ret::Val(0)
    }

    // ------------------------------------------------------------ clone

    /// clone: a thread of this process (CLONE_THREAD), or a process. The child returns 0 from it
    /// (with its own stack if one is given); the parent gets its pid.
    pub(crate) fn sys_clone<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize, c: CloneArgs) -> Ret {
        let (v, f) = (Ret::Val, c.flags);
        if f & CLONE_THREAD != 0 && f & (CLONE_SIGHAND | CLONE_VM) != (CLONE_SIGHAND | CLONE_VM)
            || f & CLONE_SIGHAND != 0 && f & CLONE_VM == 0
            || f & CLONE_PIDFD != 0 && f & CLONE_THREAD != 0
            // clone puts the pidfd where ptid points, so it can't have the tid there too
            || f & CLONE_PIDFD != 0 && f & CLONE_PARENT_SETTID != 0 && c.pidfd == c.ptid
            || f & CLONE_VFORK != 0 && f & CLONE_THREAD != 0
            || f & CLONE_PARENT != 0 && self.p.pid == INIT
        {
            return v(-EINVAL);
        }
        if f & CLONE_NAMESPACES != 0 {
            return v(-EPERM);
        }
        let known = CLONE_VM
            | CLONE_SIGHAND
            | CLONE_VFORK
            | CLONE_PARENT
            | CLONE_THREAD
            | CLONE_SETTLS
            | CLONE_PARENT_SETTID
            | CLONE_CHILD_CLEARTID
            | CLONE_CHILD_SETTID
            | CLONE_PIDFD
            | CLONE_IGNORED;
        if f & !known != 0 {
            return v(-EINVAL);
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut ctx = Ctx::save(cpu);
        ctx.x[0] = 0;
        if c.sp != 0 {
            ctx.sp = c.sp;
        }
        if f & CLONE_SETTLS != 0 {
            ctx.tpidr = c.tls;
        }
        let clear_tid = if f & CLONE_CHILD_CLEARTID != 0 { c.ctid } else { 0 };
        if f & CLONE_THREAD != 0 {
            if f & CLONE_PARENT_SETTID != 0 {
                self.put_u32(c.ptid, id as u32);
            }
            if f & CLONE_CHILD_SETTID != 0 {
                self.put_u32(c.ctid, id as u32);
            }
            self.threads.push(Thread {
                tid: id,
                pid: self.p.pid,
                ctx,
                clear_tid,
                wait: None,
                resume_deadline: None,
                exited: false,
                sig: self.threads[cur].sig.inherit(),
                sched: self.threads[cur].sched.fork(),
            });
            return v(id);
        }
        // A process
        let mm = if f & CLONE_VM != 0 {
            Rc::clone(&self.p.mm)
        } else {
            let child = self.p.mm.borrow_mut().fork(&mut self.frames.borrow_mut());
            let Some(mut child) = child else {
                return v(-ENOMEM);
            };
            let refs = child.take_refs();
            self.settle_refs(refs);
            Rc::new(RefCell::new(child))
        };
        let parent = &self.threads[cur].sig;
        // A copy of the address space has its alternate stack; one that is shared (except for
        // vfork's, whose parent waits) is not for two to use
        let keeps = f & CLONE_VM == 0 || f & CLONE_VFORK != 0;
        let sig = ThreadSig {
            mask: parent.mask,
            altstack: if keeps { parent.altstack } else { Default::default() },
            ..Default::default()
        };
        let fds = self.files.dup_table();
        let child = Proc {
            pid: id,
            ppid: if f & CLONE_PARENT != 0 { self.p.ppid } else { self.p.pid },
            pgid: self.p.pgid,
            sid: self.p.sid,
            mm,
            fds,
            exit_signal: c.exit_signal,
            zombie: None,
            reap: false,
            cwd: self.p.cwd,
            umask: self.p.umask,
            exe: self.p.exe.clone(),
            sig: self.p.sig.fork(),
            creds: self.p.creds,
            alarm: None,
        };
        self.procs.insert(id, child);
        self.threads.push(Thread {
            tid: id,
            pid: id,
            ctx,
            clear_tid,
            wait: None,
            resume_deadline: None,
            exited: false,
            sig,
            sched: self.threads[cur].sched.fork(),
        });
        if f & CLONE_PIDFD != 0 {
            // A pidfd of the child, close-on-exec, in the parent's table
            let d = self.files.add(Kind::Pidfd { pid: id }, 2);
            let fd = self.files.install(d, 0, true);
            self.put_u32(c.pidfd, fd as u32);
        }
        if f & CLONE_PARENT_SETTID != 0 {
            self.put_u32(c.ptid, id as u32);
        }
        if f & CLONE_CHILD_SETTID != 0 {
            if f & CLONE_VM != 0 {
                self.put_u32(c.ctid, id as u32);
            } else {
                // In the child's copy of the memory
                let me = self.p.pid;
                self.switch_to(id);
                self.put_u32(c.ctid, id as u32);
                self.switch_to(me);
            }
        }
        if f & CLONE_VFORK != 0 {
            return Ret::Block(Wait::Vfork(id), None);
        }
        v(id)
    }

    /// clone3(args, size)
    pub(crate) fn sys_clone3<H: Host>(
        &mut self,
        cpu: &mut Cpu<H>,
        cur: usize,
        args: u64,
        size: u64,
    ) -> Ret {
        // struct clone_args: the 64 bytes of its first version, then set_tid, set_tid_size, cgroup
        if !(64..=4096).contains(&size) {
            return Ret::Val(-EINVAL);
        }
        let Some(b) = self.bytes_at(args, size.min(88) as usize) else {
            return Ret::Val(-EFAULT);
        };
        let field = |i: usize| {
            b.get(8 * i..8 * i + 8)
                .map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap()))
        };
        let (flags, ctid, ptid, exit_signal) = (field(0), field(2), field(3), field(4));
        let (stack, stack_size, tls, set_tid_size) = (field(5), field(6), field(7), field(9));
        if flags >> 32 != 0 || flags & 0xff != 0 || exit_signal > 0xff || set_tid_size != 0 {
            return Ret::Val(-EINVAL);
        }
        let sp = if stack != 0 { stack + stack_size } else { 0 };
        self.sys_clone(
            cpu,
            cur,
            CloneArgs {
                flags,
                exit_signal: if flags & CLONE_THREAD != 0 { 0 } else { exit_signal as u32 },
                sp,
                ptid,
                ctid,
                tls,
                pidfd: field(1),
            },
        )
    }
}
