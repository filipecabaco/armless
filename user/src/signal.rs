//! Signals, as Linux delivers them on arm64: actions per process, a mask, a pending set and an
//! alternate stack per thread, and a pending set for each process. A signal for a process goes
//! to its first thread that doesn't block it (the main thread first, as Linux picks); one with a
//! handler is delivered when its thread next runs, on a `rt_sigframe` built on its stack (or its
//! alternate stack, SA_ONSTACK) exactly as the kernel lays it out, and `rt_sigreturn` takes the
//! thread back from that frame, mask and FP/SIMD registers too.
//!
//! A thread parked in a blocking call is woken by a signal it has a handler for, and the call
//! ends as Linux ends it (`interrupted`): reads and writes of terminals and pipes, and futex
//! waits with no timeout, start again after the handler when it has SA_RESTART and return EINTR
//! otherwise; poll, select, epoll, sleeps, rt_sigsuspend and rt_sigtimedwait return EINTR
//! whatever the flags. A call waiting on the filesystem's contents is not interrupted (Linux's
//! uninterruptible disk wait): its signal is delivered once it is done.
//!
//! Default actions: SIGCHLD, SIGCONT, SIGURG and SIGWINCH are ignored, and so are the stop signals
//! (no job control); every other one ends the process (the first one's exit code is 128 + the
//! signal; another's wait status says which signal killed it). A process's end raises its exit
//! signal (SIGCHLD) in its parent. `kill` takes a pid, a process group, or everything.
//! Not yet: queued real-time signals (one of each pending, as for the standard ones), the
//! signal masks of ppoll, pselect6 and epoll_pwait, signals for faults (a fault ends the process
//! as SIGSEGV would, whatever its handler), and the sender's pid in a handler's siginfo (the
//! first process's, 1000, for kill, tkill and the like; none for SIGCHLD).

use crate::host::Instant;
use std::collections::BTreeMap;

use armless::cpu::Cpu;
use armless::Host;

use crate::proc::End;
use crate::ser::{R, W};
use crate::{Ctx, Process, Ret, Wait, INIT};

pub(crate) const SIGINT: u32 = 2;
pub(crate) const SIGQUIT: u32 = 3;
pub(crate) const SIGKILL: u32 = 9;
pub(crate) const SIGCHLD: u32 = 17;
pub(crate) const SIGCONT: u32 = 18;
pub(crate) const SIGSTOP: u32 = 19;
pub(crate) const SIGTSTP: u32 = 20;
pub(crate) const SIGTTIN: u32 = 21;
pub(crate) const SIGTTOU: u32 = 22;
pub(crate) const SIGURG: u32 = 23;
pub(crate) const SIGWINCH: u32 = 28;
const NSIG: u32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

const SA_NOCLDWAIT: u64 = 2;
const SA_SIGINFO: u64 = 4;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;

const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;
const SS_AUTODISARM: u32 = 1 << 31;
const MINSIGSTKSZ: u64 = 5120;
/// The alternate stack a thread of an old state gets (`ThreadSig::altstack_unknown`)
const LEGACY_ALTSTACK: u64 = 64 << 10;

/// si_code: sent by kill, by tkill or tgkill, by the kernel (the terminal, the host)
pub(crate) const SI_USER: i32 = 0;
pub(crate) const SI_TKILL: i32 = -6;
pub(crate) const SI_KERNEL: i32 = 0x80;

const EINTR: i64 = 4;
const EINVAL: i64 = 22;
const EFAULT: i64 = 14;
const ENOMEM: i64 = 12;
const EPERM: i64 = 1;
const EAGAIN: i64 = 11;
const ESRCH: i64 = 3;

// The frame (arch/arm64/kernel/signal.c): siginfo, then the ucontext, whose sigcontext is at 176
// and its __reserved records at 288 of that; a frame record (fp, lr) above it all.
const SIGINFO_SIZE: u64 = 128;
const UC_STACK: u64 = 16;
const UC_SIGMASK: u64 = 40;
const UC_MCONTEXT: u64 = 176;
const MC_REGS: u64 = 8;
const MC_SP: u64 = 256;
const MC_PC: u64 = 264;
const MC_PSTATE: u64 = 272;
const MC_RESERVED: u64 = 288;
const RESERVED_SIZE: u64 = 4096;
const FRAME_SIZE: u64 = SIGINFO_SIZE + UC_MCONTEXT + MC_RESERVED + RESERVED_SIZE;
const FPSIMD_MAGIC: u32 = 0x4650_8001;
const FPSIMD_SIZE: u32 = 528;

pub(crate) fn bit(sig: u32) -> u64 {
    1 << (sig - 1)
}

/// What can't be blocked
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

/// A signal's action: the kernel's struct sigaction on arm64.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(crate) struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

impl Action {
    fn to_bytes(self) -> [u8; 32] {
        let mut b = [0u8; 32];
        for (i, v) in [self.handler, self.flags, self.restorer, self.mask]
            .into_iter()
            .enumerate()
        {
            b[8 * i..8 * i + 8].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn from_bytes(b: &[u8; 32]) -> Action {
        let u = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
        Action {
            handler: u(0),
            flags: u(1),
            restorer: u(2),
            mask: u(3),
        }
    }
}

/// What delivering a signal does.
#[derive(PartialEq, Debug)]
enum Disposition {
    Ignore,
    Terminate,
    Handle(Action),
}

fn default_ignored(sig: u32) -> bool {
    matches!(
        sig,
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH | SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU
    )
}

/// A thread's alternate signal stack (sigaltstack); size 0 is none.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(crate) struct AltStack {
    pub sp: u64,
    pub size: u64,
    pub autodisarm: bool,
}

impl AltStack {
    fn on(&self, sp: u64) -> bool {
        !self.autodisarm && sp > self.sp && sp - self.sp <= self.size
    }

    /// ss_flags as sigaltstack reports them for a thread at `sp`
    fn flags(&self, sp: u64) -> u32 {
        let base = if self.size == 0 {
            SS_DISABLE
        } else if self.on(sp) {
            SS_ONSTACK
        } else {
            0
        };
        base | if self.autodisarm { SS_AUTODISARM } else { 0 }
    }
}

/// A thread's signal state.
#[derive(Clone, Default, Debug)]
pub(crate) struct ThreadSig {
    pub mask: u64,
    /// Signals for this thread, with their si_code
    pub pending: BTreeMap<u32, i32>,
    pub altstack: AltStack,
    /// The mask to go back to once rt_sigsuspend's own ends
    pub saved_mask: Option<u64>,
    /// Restored from a state of before signals were delivered, which didn't keep alternate
    /// stacks: the thread may have set one (Go's threads all have), so its first SA_ONSTACK
    /// handler gets one of its own
    pub altstack_unknown: bool,
}

impl ThreadSig {
    /// A new thread's: its creator's mask, nothing pending, no alternate stack (clone)
    pub fn inherit(&self) -> ThreadSig {
        ThreadSig {
            mask: self.mask,
            ..Default::default()
        }
    }
}

/// The process's signal state.
pub(crate) struct Signals {
    actions: [Action; NSIG as usize + 1],
    /// Signals for the process not yet given to a thread, with their si_code
    pub pending: BTreeMap<u32, i32>,
    /// Where the sigreturn trampoline is, once made (0: not yet)
    pub trampoline: u64,
}

impl Default for Signals {
    fn default() -> Signals {
        Signals {
            actions: [Action::default(); NSIG as usize + 1],
            pending: BTreeMap::new(),
            trampoline: 0,
        }
    }
}

impl Signals {
    /// A child process's (fork): the same actions, nothing pending
    pub fn fork(&self) -> Signals {
        Signals {
            actions: self.actions,
            pending: BTreeMap::new(),
            trampoline: self.trampoline,
        }
    }

    /// Whether the process's children vanish when they end, with no one to wait for them
    /// (SIGCHLD ignored, or SA_NOCLDWAIT)
    pub fn children_vanish(&self) -> bool {
        let a = self.actions[SIGCHLD as usize];
        a.handler == SIG_IGN || a.flags & SA_NOCLDWAIT != 0
    }

    fn disposition(&self, sig: u32) -> Disposition {
        let a = self.actions[sig as usize];
        match a.handler {
            _ if sig == SIGKILL || sig == SIGSTOP => {
                if sig == SIGKILL {
                    Disposition::Terminate
                } else {
                    Disposition::Ignore
                }
            },
            SIG_IGN => Disposition::Ignore,
            SIG_DFL if default_ignored(sig) => Disposition::Ignore,
            SIG_DFL => Disposition::Terminate,
            _ => Disposition::Handle(a),
        }
    }

    /// The actions that aren't the default, for a snapshot (the format sigactions always had)
    pub fn save_actions(&self, w: &mut W) {
        let set: Vec<u32> = (1..=NSIG)
            .filter(|&s| self.actions[s as usize] != Action::default())
            .collect();
        w.u(set.len() as u64);
        for s in set {
            w.u(s as u64);
            w.b(&self.actions[s as usize].to_bytes());
        }
    }

    pub fn load_actions(r: &mut R) -> Result<Signals, String> {
        let mut s = Signals::default();
        for _ in 0..r.u()? {
            let sig = r.u()?;
            let act: &[u8; 32] = r.b()?.try_into().map_err(|_| "bad sigaction")?;
            if (1..=NSIG as u64).contains(&sig) {
                s.actions[sig as usize] = Action::from_bytes(act);
            }
        }
        Ok(s)
    }

    /// The actions, the trampoline and what is pending, for a snapshot
    pub fn save(&self, w: &mut W) {
        self.save_actions(w);
        w.u(self.trampoline);
        save_pending(w, &self.pending);
    }

    pub fn load(r: &mut R) -> Result<Signals, String> {
        let mut s = Signals::load_actions(r)?;
        s.trampoline = r.u()?;
        s.pending = load_pending(r)?;
        Ok(s)
    }
}

impl ThreadSig {
    pub fn save(&self, w: &mut W) {
        w.u(self.mask);
        save_pending(w, &self.pending);
        w.u(self.altstack.sp);
        w.u(self.altstack.size);
        w.u(self.altstack.autodisarm as u64);
        w.u(self.saved_mask.map_or(0, |_| 1));
        w.u(self.saved_mask.unwrap_or(0));
        w.u(self.altstack_unknown as u64);
    }

    pub fn load(r: &mut R) -> Result<ThreadSig, String> {
        let mask = r.u()?;
        let pending = load_pending(r)?;
        let altstack = AltStack {
            sp: r.u()?,
            size: r.u()?,
            autodisarm: r.u()? != 0,
        };
        let has = r.u()? != 0;
        let m = r.u()?;
        Ok(ThreadSig {
            mask,
            pending,
            altstack,
            saved_mask: has.then_some(m),
            altstack_unknown: r.u()? != 0,
        })
    }
}

fn save_pending(w: &mut W, p: &BTreeMap<u32, i32>) {
    w.u(p.len() as u64);
    for (&s, &c) in p {
        w.u(s as u64);
        w.u(c as i64 as u64);
    }
}

fn load_pending(r: &mut R) -> Result<BTreeMap<u32, i32>, String> {
    let mut p = BTreeMap::new();
    for _ in 0..r.u()? {
        let s = r.u()? as u32;
        let c = r.u()? as i64 as i32;
        if !(1..=NSIG).contains(&s) {
            return Err(format!("bad pending signal {s}"));
        }
        p.insert(s, c);
    }
    Ok(p)
}

/// How a parked call ends when a handler interrupts it.
enum Interrupted {
    /// It returns this (to x0, past its svc)
    Returns(i64),
    /// It runs again after the handler (SA_RESTART), from its svc
    Restarts,
}

impl Process {
    // ------------------------------------------------------------ raising

    /// A signal for process `pid` (`thread`: for that thread, by its index). Ignored ones are
    /// dropped now, as Linux does, unless blocked (a handler may be set before they're unblocked).
    pub(crate) fn raise_in(&mut self, pid: i64, sig: u32, thread: Option<usize>, code: i32) {
        if !(1..=NSIG).contains(&sig) {
            return;
        }
        let Some(p) = self.proc_ref(pid).filter(|p| p.zombie.is_none()) else {
            return;
        };
        let ignored = p.sig.disposition(sig) == Disposition::Ignore;
        // Whether it is blocked: by the thread it is for, or the process's first
        let t = thread.or_else(|| self.threads.iter().position(|t| t.pid == pid && !t.exited));
        let blocked = t.is_some_and(|t| self.threads[t].sig.mask & bit(sig) != 0);
        if ignored && !blocked {
            self.stats.signals_ignored += 1;
            return;
        }
        match thread {
            Some(t) => {
                self.threads[t].sig.pending.entry(sig).or_insert(code);
            },
            None => {
                if let Some(p) = self.proc_mut(pid) {
                    p.sig.pending.entry(sig).or_insert(code);
                }
            },
        }
        self.sig_dirty = true;
        // rt_sigtimedwait looks again
        self.poked = true;
    }

    /// Whether a thread has a signal it can take now, for the run loop to deliver it
    pub(crate) fn signal_ready(&self) -> bool {
        self.sig_dirty
    }

    /// Gives what the processes have pending to their threads, then delivers what each thread
    /// can take: wakes it from a call it is parked in, and builds the handler's frame (or ends
    /// its process, or drops the signal). (A handler is built in its process's address space:
    /// that process becomes the one that runs.)
    pub(crate) fn deliver_signals<H: Host>(&mut self, cpu: &mut Cpu<H>) {
        if !std::mem::take(&mut self.sig_dirty)
            && self.proc_list().all(|p| p.sig.pending.is_empty())
            && self.threads.iter().all(|t| t.sig.pending.is_empty())
        {
            return;
        }
        // The processes': to the first thread not blocking it, the main thread first
        let shared: Vec<(i64, u32, i32)> = self
            .proc_list()
            .flat_map(|p| p.sig.pending.iter().map(move |(&s, &c)| (p.pid, s, c)))
            .collect();
        for (pid, sig, code) in shared {
            let to = self
                .threads
                .iter()
                .position(|t| t.pid == pid && !t.exited && t.sig.mask & bit(sig) == 0);
            if let Some(t) = to {
                self.threads[t].sig.pending.entry(sig).or_insert(code);
                if let Some(p) = self.proc_mut(pid) {
                    p.sig.pending.remove(&sig);
                }
            }
        }
        for i in 0..self.threads.len() {
            loop {
                let t = &self.threads[i];
                if t.exited || matches!(t.wait, Some((Wait::Blobs(_) | Wait::Vfork(_), _))) {
                    break;
                }
                let mask = t.sig.mask & !UNBLOCKABLE;
                let Some((&sig, &code)) = t.sig.pending.iter().find(|(&s, _)| mask & bit(s) == 0)
                else {
                    break;
                };
                let pid = t.pid;
                self.threads[i].sig.pending.remove(&sig);
                let Some(disposition) = self.proc_ref(pid).map(|p| p.sig.disposition(sig)) else {
                    break;
                };
                match disposition {
                    Disposition::Ignore => {
                        self.stats.signals_ignored += 1;
                        continue;
                    },
                    Disposition::Terminate => {
                        self.end_process(pid, End::Signal(sig));
                        if self.exit_code.is_some() {
                            return;
                        }
                        break;
                    },
                    Disposition::Handle(act) => {
                        if self.loaded == Some(i) {
                            self.park(cpu);
                        }
                        self.switch_to(pid);
                        self.handle(i, sig, code, act);
                        self.stats.signals_delivered += 1;
                        // One frame at a time: the next once this one's thread runs again
                        self.sig_dirty = true;
                        break;
                    },
                }
            }
        }
    }

    /// Ends the call thread `i` is parked in, if any, as a handler with `act` interrupts it.
    fn interrupt(&mut self, i: usize, act: &Action) {
        let t = &mut self.threads[i];
        let Some((wait, deadline)) = t.wait.take() else {
            return;
        };
        t.resume_deadline = None;
        let nr = t.ctx.x[8];
        let restart = act.flags & SA_RESTART != 0;
        let how = match wait {
            // Reads and writes of terminals and pipes (ERESTARTSYS), the rest EINTR
            Wait::Retry { .. } => match nr {
                29 | 56 | 63..=70 | 260 | 95 if restart => Interrupted::Restarts,
                _ => Interrupted::Returns(-EINTR),
            },
            // A futex wait with no timeout restarts with SA_RESTART; with one, EINTR
            Wait::Futex { .. } if restart && deadline.is_none() => Interrupted::Restarts,
            Wait::Futex { .. } => Interrupted::Returns(-EINTR),
            // nanosleep, clock_nanosleep: EINTR, and the time left in `rem`
            Wait::Sleep => {
                let rem = match nr {
                    101 => t.ctx.x[1],
                    115 if t.ctx.x[1] & 1 == 0 => t.ctx.x[3],
                    _ => 0,
                };
                if rem != 0 {
                    let left = deadline.map_or(std::time::Duration::ZERO, |d| {
                        d.saturating_duration_since(Instant::now())
                    });
                    self.put_u64(rem, left.as_secs());
                    self.put_u64(rem + 8, left.subsec_nanos() as u64);
                }
                Interrupted::Returns(-EINTR)
            },
            Wait::Blobs(_) | Wait::Vfork(_) => unreachable!("not interrupted"),
        };
        let t = &mut self.threads[i];
        // A Retry wait has its pc on the svc; a Block one past it
        let at_svc = matches!(wait, Wait::Retry { .. });
        match how {
            Interrupted::Returns(r) => {
                if at_svc {
                    t.ctx.pc += 4;
                }
                t.ctx.x[0] = r as u64;
            },
            Interrupted::Restarts => {
                if !at_svc {
                    t.ctx.pc -= 4;
                }
            },
        }
    }

    /// Runs `act`'s handler for `sig` on thread `i` (its registers in the process).
    fn handle(&mut self, i: usize, sig: u32, code: i32, act: Action) {
        self.interrupt(i, &act);
        let t = &mut self.threads[i];
        let ctx = t.ctx.clone();
        // rt_sigsuspend's mask is the handler's; the one before it goes back at sigreturn
        let old_mask = t.sig.saved_mask.take().unwrap_or(t.sig.mask);
        if act.flags & SA_ONSTACK != 0 && t.sig.altstack_unknown && t.sig.altstack.size == 0 {
            t.sig.altstack_unknown = false;
            // Go takes a signal on any alternate stack sigaltstack reports, as its own
            if let Some(at) = self.alloc_map(
                LEGACY_ALTSTACK,
                crate::mm::PROT_READ | crate::mm::PROT_WRITE,
            ) {
                self.threads[i].sig.altstack = AltStack {
                    sp: at,
                    size: LEGACY_ALTSTACK,
                    autodisarm: false,
                };
            }
        }
        let t = &mut self.threads[i];
        let alt = t.sig.altstack;
        let mut sp = ctx.sp;
        if act.flags & SA_ONSTACK != 0 && alt.size != 0 && !alt.on(sp) {
            sp = alt.sp + alt.size;
        }
        let record = (sp - 16) & !15;
        let frame = (record - FRAME_SIZE) & !15;
        let mut f = vec![0u8; FRAME_SIZE as usize];
        let put = |f: &mut Vec<u8>, at: u64, v: u64| {
            f[at as usize..at as usize + 8].copy_from_slice(&v.to_le_bytes())
        };
        // siginfo: si_signo, si_errno, si_code, then si_pid and si_uid for a sender
        f[0..4].copy_from_slice(&(sig as i32).to_le_bytes());
        f[8..12].copy_from_slice(&code.to_le_bytes());
        if code <= 0 {
            f[16..20].copy_from_slice(&1000i32.to_le_bytes());
        }
        let uc = SIGINFO_SIZE;
        // uc_flags, uc_link: 0; uc_stack: the alternate stack as it was
        put(&mut f, uc + UC_STACK, alt.sp);
        f[(uc + UC_STACK + 8) as usize..(uc + UC_STACK + 12) as usize]
            .copy_from_slice(&alt.flags(ctx.sp).to_le_bytes());
        put(&mut f, uc + UC_STACK + 16, alt.size);
        put(&mut f, uc + UC_SIGMASK, old_mask);
        let mc = uc + UC_MCONTEXT;
        for (r, &x) in ctx.x.iter().enumerate() {
            put(&mut f, mc + MC_REGS + 8 * r as u64, x);
        }
        put(&mut f, mc + MC_SP, ctx.sp);
        put(&mut f, mc + MC_PC, ctx.pc);
        put(&mut f, mc + MC_PSTATE, pstate(&ctx));
        // fpsimd_context, then the terminating null record
        let fp = mc + MC_RESERVED;
        f[fp as usize..fp as usize + 4].copy_from_slice(&FPSIMD_MAGIC.to_le_bytes());
        f[fp as usize + 4..fp as usize + 8].copy_from_slice(&FPSIMD_SIZE.to_le_bytes());
        f[fp as usize + 8..fp as usize + 12].copy_from_slice(&(ctx.fpsr as u32).to_le_bytes());
        f[fp as usize + 12..fp as usize + 16].copy_from_slice(&(ctx.fpcr as u32).to_le_bytes());
        for (r, &v) in ctx.v.iter().enumerate() {
            let at = (fp + 16 + 16 * r as u64) as usize;
            f[at..at + 16].copy_from_slice(&v.to_le_bytes());
        }
        let ok = self.write(frame, &f)
            && self.put_u64(record, ctx.x[29])
            && self.put_u64(record + 8, ctx.x[30]);
        if !ok {
            // Linux would kill it with SIGSEGV
            self.crash(format!(
                "no room for signal {sig}'s frame at {frame:#x} (sp {:#x})",
                ctx.sp
            ));
            return;
        }
        // Without SA_RESTORER the kernel returns to its vDSO's sigreturn trampoline (Go on
        // arm64 sets none): here a page of the process's with the same two instructions
        let restorer = if act.flags & SA_RESTORER != 0 { act.restorer } else { self.trampoline() };
        let t = &mut self.threads[i];
        if act.flags & SA_ONSTACK != 0 && alt.autodisarm && sp != ctx.sp {
            t.sig.altstack = AltStack::default();
        }
        t.ctx.x[0] = sig as u64;
        if act.flags & SA_SIGINFO != 0 {
            t.ctx.x[1] = frame;
            t.ctx.x[2] = frame + uc;
        }
        t.ctx.x[29] = record;
        t.ctx.x[30] = restorer;
        t.ctx.sp = frame;
        t.ctx.pc = act.handler;
        let mut mask = t.sig.mask | act.mask;
        if act.flags & SA_NODEFER == 0 {
            mask |= bit(sig);
        }
        t.sig.mask = mask & !UNBLOCKABLE;
        if act.flags & SA_RESETHAND != 0 {
            self.p.sig.actions[sig as usize] = Action::default();
        }
    }

    /// The sigreturn trampoline (`mov x8, #139; svc #0`), made the first time it is needed.
    fn trampoline(&mut self) -> u64 {
        if self.p.sig.trampoline == 0 {
            if let Some(at) =
                self.alloc_map(crate::PAGE, crate::mm::PROT_READ | crate::mm::PROT_EXEC)
            {
                let code = [0xd280_1168u32, 0xd400_0001];
                let b: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
                self.write_any(at, &b);
                self.p.sig.trampoline = at;
            }
        }
        self.p.sig.trampoline
    }

    /// rt_sigreturn: the thread as the frame at its stack pointer has it.
    pub(crate) fn sigreturn<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        let frame = cpu.sp_el0;
        let Some(f) = (frame & 15 == 0)
            .then(|| self.bytes_at(frame, FRAME_SIZE as usize))
            .flatten()
        else {
            self.crash(format!("rt_sigreturn: bad frame at {frame:#x}"));
            return Ret::Val(-EFAULT);
        };
        let get = |at: u64| u64::from_le_bytes(f[at as usize..at as usize + 8].try_into().unwrap());
        let get32 =
            |at: u64| u32::from_le_bytes(f[at as usize..at as usize + 4].try_into().unwrap());
        let uc = SIGINFO_SIZE;
        let mc = uc + UC_MCONTEXT;
        let mut ctx = Ctx::save(cpu);
        for r in 0..31 {
            ctx.x[r] = get(mc + MC_REGS + 8 * r as u64);
        }
        ctx.sp = get(mc + MC_SP);
        ctx.pc = get(mc + MC_PC);
        let ps = get(mc + MC_PSTATE);
        ctx.nzcv = [
            ps >> 31 & 1 != 0,
            ps >> 30 & 1 != 0,
            ps >> 29 & 1 != 0,
            ps >> 28 & 1 != 0,
        ];
        // The records: an fpsimd_context is required, as Linux has it
        let mut at = mc + MC_RESERVED;
        let end = at + RESERVED_SIZE;
        let mut fpsimd = false;
        while at + 8 <= end {
            let (magic, size) = (get32(at), get32(at + 4) as u64);
            if magic == 0 {
                break;
            }
            if size < 8 || size % 16 != 0 || at + size > end {
                break;
            }
            if magic == FPSIMD_MAGIC && size == FPSIMD_SIZE as u64 {
                ctx.fpsr = get32(at + 8) as u64;
                ctx.fpcr = get32(at + 12) as u64;
                for (r, v) in ctx.v.iter_mut().enumerate() {
                    let o = (at + 16 + 16 * r as u64) as usize;
                    *v = u128::from_le_bytes(f[o..o + 16].try_into().unwrap());
                }
                fpsimd = true;
            }
            at += size;
        }
        if !fpsimd {
            self.crash(format!(
                "rt_sigreturn: no FP/SIMD record in the frame at {frame:#x}"
            ));
            return Ret::Val(-EFAULT);
        }
        ctx.load(cpu);
        let t = &mut self.threads[cur];
        t.sig.mask = get(uc + UC_SIGMASK) & !UNBLOCKABLE;
        // The alternate stack as the frame has it, unless the thread is back on it
        let (ss_sp, ss_flags, ss_size) = (
            get(uc + UC_STACK),
            get32(uc + UC_STACK + 8),
            get(uc + UC_STACK + 16),
        );
        if !t.sig.altstack.on(ctx.sp) {
            if ss_flags & SS_DISABLE != 0 {
                t.sig.altstack = AltStack::default();
            } else if ss_size >= MINSIGSTKSZ {
                t.sig.altstack = AltStack {
                    sp: ss_sp,
                    size: ss_size,
                    autodisarm: ss_flags & SS_AUTODISARM != 0,
                };
            }
        }
        self.sig_dirty = true;
        Ret::Val(ctx.x[0] as i64)
    }

    // ------------------------------------------------------------ the syscalls

    /// rt_sigaction(sig, act, oldact, sigsetsize)
    pub(crate) fn sys_sigaction(&mut self, sig: u64, act: u64, old: u64, size: u64) -> Ret {
        if !(1..=NSIG as u64).contains(&sig) || size != 8 {
            return Ret::Val(-EINVAL);
        }
        let sig = sig as u32;
        if act != 0 && (sig == SIGKILL || sig == SIGSTOP) {
            return Ret::Val(-EINVAL);
        }
        let new = if act != 0 {
            match self.bytes_at(act, 32) {
                Some(b) => Some(Action::from_bytes(&b.try_into().unwrap())),
                None => return Ret::Val(-EFAULT),
            }
        } else {
            None
        };
        if old != 0 && !self.write(old, &self.p.sig.actions[sig as usize].to_bytes()) {
            return Ret::Val(-EFAULT);
        }
        if let Some(mut a) = new {
            a.mask &= !UNBLOCKABLE;
            self.p.sig.actions[sig as usize] = a;
            // Ignored now: what is pending of it goes
            if self.p.sig.disposition(sig) == Disposition::Ignore {
                self.p.sig.pending.remove(&sig);
                let me = self.p.pid;
                for t in self.threads.iter_mut().filter(|t| t.pid == me) {
                    t.sig.pending.remove(&sig);
                }
            }
            // Children that end are no longer waited for: those that have are gone
            if sig == SIGCHLD && self.p.sig.children_vanish() {
                let me = self.p.pid;
                self.procs
                    .retain(|_, c| !(c.ppid == me && c.zombie.is_some()));
            }
        }
        Ret::Val(0)
    }

    /// rt_sigprocmask(how, set, oldset, sigsetsize)
    pub(crate) fn sys_sigprocmask(&mut self, cur: usize, a: &[u64; 6]) -> Ret {
        let (how, set, old, size) = (a[0], a[1], a[2], a[3]);
        if size != 8 {
            return Ret::Val(-EINVAL);
        }
        let mask = self.threads[cur].sig.mask;
        let new = if set != 0 {
            let Some(s) = self.u64_at(set) else {
                return Ret::Val(-EFAULT);
            };
            Some(match how {
                0 => mask | s,
                1 => mask & !s,
                2 => s,
                _ => return Ret::Val(-EINVAL),
            })
        } else {
            None
        };
        if old != 0 && !self.put_u64(old, mask) {
            return Ret::Val(-EFAULT);
        }
        if let Some(m) = new {
            self.threads[cur].sig.mask = m & !UNBLOCKABLE;
            self.sig_dirty = true;
        }
        Ret::Val(0)
    }

    /// rt_sigpending(set, sigsetsize): what is pending and blocked
    pub(crate) fn sys_sigpending(&mut self, cur: usize, set: u64) -> Ret {
        let t = &self.threads[cur].sig;
        let p = t
            .pending
            .keys()
            .chain(self.p.sig.pending.keys())
            .fold(0, |a, &s| a | bit(s));
        Ret::Val(if self.put_u64(set, p & t.mask) { 0 } else { -EFAULT })
    }

    /// sigaltstack(ss, old_ss); `sp` is the thread's stack pointer
    pub(crate) fn sys_sigaltstack(&mut self, cur: usize, ss: u64, old: u64, sp: u64) -> Ret {
        let alt = self.threads[cur].sig.altstack;
        if old != 0 {
            let mut b = [0u8; 24];
            b[0..8].copy_from_slice(&alt.sp.to_le_bytes());
            b[8..12].copy_from_slice(&alt.flags(sp).to_le_bytes());
            b[16..24].copy_from_slice(&alt.size.to_le_bytes());
            if !self.write(old, &b) {
                return Ret::Val(-EFAULT);
            }
        }
        if ss != 0 {
            let Some(b) = self.bytes_at(ss, 24) else {
                return Ret::Val(-EFAULT);
            };
            if alt.on(sp) {
                return Ret::Val(-EPERM);
            }
            let ss_sp = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
            let new = match flags & !SS_AUTODISARM {
                SS_DISABLE => AltStack::default(),
                0 | SS_ONSTACK if size < MINSIGSTKSZ => return Ret::Val(-ENOMEM),
                0 | SS_ONSTACK => AltStack {
                    sp: ss_sp,
                    size,
                    autodisarm: flags & SS_AUTODISARM != 0,
                },
                _ => return Ret::Val(-EINVAL),
            };
            self.threads[cur].sig.altstack = new;
            self.threads[cur].sig.altstack_unknown = false;
        }
        Ret::Val(0)
    }

    /// rt_sigsuspend(mask, sigsetsize): waits with `mask` until a handler has run (EINTR)
    pub(crate) fn sys_sigsuspend(&mut self, cur: usize, set: u64, size: u64) -> Ret {
        if size != 8 {
            return Ret::Val(-EINVAL);
        }
        let Some(m) = self.u64_at(set) else {
            return Ret::Val(-EFAULT);
        };
        let t = &mut self.threads[cur].sig;
        // Run again while it waits: the mask from before the first time is the one to go back to
        if t.saved_mask.is_none() {
            t.saved_mask = Some(t.mask);
        }
        t.mask = m & !UNBLOCKABLE;
        self.sig_dirty = true;
        Ret::Retry {
            deadline: None,
            on_timeout: 0,
        }
    }

    /// rt_sigtimedwait(set, info, timeout, sigsetsize): takes one of `set` that is pending
    pub(crate) fn sys_sigtimedwait(&mut self, cur: usize, a: &[u64; 6]) -> Ret {
        let (set, info, timeout, size) = (a[0], a[1], a[2], a[3]);
        if size != 8 {
            return Ret::Val(-EINVAL);
        }
        let Some(want) = self.u64_at(set) else {
            return Ret::Val(-EFAULT);
        };
        let want = want & !UNBLOCKABLE;
        let pick = |p: &BTreeMap<u32, i32>| {
            p.iter()
                .find(|(&s, _)| want & bit(s) != 0)
                .map(|(&s, &c)| (s, c))
        };
        let got = match pick(&self.threads[cur].sig.pending) {
            Some(g) => {
                self.threads[cur].sig.pending.remove(&g.0);
                Some(g)
            },
            None => pick(&self.p.sig.pending).inspect(|g| {
                self.p.sig.pending.remove(&g.0);
            }),
        };
        if let Some((sig, code)) = got {
            if info != 0 {
                let mut b = [0u8; SIGINFO_SIZE as usize];
                b[0..4].copy_from_slice(&(sig as i32).to_le_bytes());
                b[8..12].copy_from_slice(&code.to_le_bytes());
                if code <= 0 {
                    b[16..20].copy_from_slice(&1000i32.to_le_bytes());
                }
                if !self.write(info, &b) {
                    return Ret::Val(-EFAULT);
                }
            }
            return Ret::Val(sig as i64);
        }
        let deadline = match (self.resume_deadline, timeout) {
            (Some(d), _) => Some(d),
            (None, 0) => None,
            (None, t) => match self.timespec(t) {
                Some(d) if d.is_zero() => return Ret::Val(-EAGAIN),
                Some(d) => Some(Instant::now() + d),
                None => return Ret::Val(-EFAULT),
            },
        };
        Ret::Retry {
            deadline,
            on_timeout: -EAGAIN,
        }
    }

    /// kill, tkill, tgkill (nr 129, 130, 131)
    pub(crate) fn sys_kill(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let (target, sig) = match nr {
            129 | 130 => (a[0] as i32 as i64, a[1]),
            _ => (a[1] as i32 as i64, a[2]),
        };
        if sig > NSIG as u64 {
            return Ret::Val(-EINVAL);
        }
        let me = self.p.pid;
        if nr == 129 {
            // A process, a group (0: this process's), or everything but the first and this one
            let pids: Vec<i64> = self
                .proc_list()
                .filter(|p| p.zombie.is_none())
                .filter(|p| match target {
                    t if t > 0 => p.pid == t,
                    0 => p.pgid == self.p.pgid,
                    -1 => p.pid != me && p.pid != INIT,
                    t => p.pgid == -t,
                })
                .map(|p| p.pid)
                .collect();
            if pids.is_empty() {
                return Ret::Val(-ESRCH);
            }
            if sig != 0 {
                for pid in pids {
                    self.raise_in(pid, sig as u32, None, SI_USER);
                }
            }
            return Ret::Val(0);
        }
        // A thread, by its tid (tgkill: of that process)
        let Some(i) = self.threads.iter().position(|t| {
            !t.exited && t.tid == target && (nr != 131 || t.pid == a[0] as i32 as i64)
        }) else {
            return Ret::Val(-ESRCH);
        };
        if sig != 0 {
            let pid = self.threads[i].pid;
            self.raise_in(pid, sig as u32, Some(i), SI_TKILL);
        }
        Ret::Val(0)
    }
}

/// PSTATE as user mode sees it: NZCV (EL0t)
fn pstate(c: &Ctx) -> u64 {
    let [n, z, cf, v] = c.nzcv;
    (n as u64) << 31 | (z as u64) << 30 | (cf as u64) << 29 | (v as u64) << 28
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_is_laid_out_as_the_kernel_lays_it() {
        // sizeof(struct rt_sigframe) on arm64, and where Go and glibc look in it
        assert_eq!(FRAME_SIZE, 4688);
        assert_eq!(FRAME_SIZE % 16, 0);
        assert_eq!(SIGINFO_SIZE + UC_MCONTEXT, 304);
        assert_eq!((SIGINFO_SIZE + UC_MCONTEXT + MC_RESERVED) % 16, 0);
    }

    #[test]
    fn an_action_round_trips_as_struct_sigaction() {
        let a = Action {
            handler: 0x1234,
            flags: SA_SIGINFO | SA_RESTORER,
            restorer: 0x5678,
            mask: 0b110,
        };
        let b = a.to_bytes();
        assert_eq!(&b[0..8], &0x1234u64.to_le_bytes());
        assert_eq!(&b[16..24], &0x5678u64.to_le_bytes());
        assert_eq!(Action::from_bytes(&b), a);
    }

    #[test]
    fn default_actions_ignore_some_and_end_on_the_rest() {
        let s = Signals::default();
        for sig in [SIGCHLD, SIGCONT, SIGURG, SIGWINCH, SIGTSTP, SIGSTOP] {
            assert_eq!(s.disposition(sig), Disposition::Ignore, "{sig}");
        }
        for sig in [SIGINT, SIGQUIT, SIGKILL, 15, 34] {
            assert_eq!(s.disposition(sig), Disposition::Terminate, "{sig}");
        }
        let mut s = Signals::default();
        s.actions[SIGINT as usize].handler = SIG_IGN;
        s.actions[SIGKILL as usize].handler = 0x1000;
        assert_eq!(s.disposition(SIGINT), Disposition::Ignore);
        assert_eq!(
            s.disposition(SIGKILL),
            Disposition::Terminate,
            "SIGKILL can't be caught"
        );
    }

    #[test]
    fn the_alternate_stack_knows_when_a_thread_is_on_it() {
        let alt = AltStack {
            sp: 0x1000,
            size: 0x2000,
            autodisarm: false,
        };
        assert!(alt.on(0x3000));
        assert!(alt.on(0x1800));
        assert!(!alt.on(0x1000));
        assert!(!alt.on(0x3001));
        assert_eq!(alt.flags(0x1800), SS_ONSTACK);
        assert_eq!(alt.flags(0x8000), 0);
        assert_eq!(AltStack::default().flags(0), SS_DISABLE);
    }
}
