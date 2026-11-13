//! Timers that deliver signals: the interval timers (`setitimer`, `getitimer`: ITIMER_REAL,
//! ITIMER_VIRTUAL, ITIMER_PROF) and the POSIX per-process timers (`timer_create`,
//! `timer_settime`, `timer_gettime`, `timer_getoverrun`, `timer_delete`). aarch64 has no `alarm`:
//! glibc's is `setitimer`. Postgres arms its statement, lock and idle timeouts with ITIMER_REAL,
//! and refuses a connection when it can't; a busybox shell sets them as it starts.
//!
//! A timer is armed with a due time and an interval to reload it with. The run loop looks at them
//! every time around (`fire_timers`, before signals are delivered, so a timer that is due
//! interrupts the call a thread is parked in as any other signal does), and when every thread
//! waits `Status::Idle` carries the first deadline of an armed timer, so a host wakes the
//! machine when it is due (`timer_wake`).
//!
//! Wall-clock timers (ITIMER_REAL, CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME, CLOCK_TAI
//! and the alarm clocks) are due at a host `Instant`, whichever clock they are on; an absolute
//! time on CLOCK_REALTIME is turned into one when it is set (the wall clock is not adjusted
//! while the machine runs, so it doesn't move after). CPU-time timers (ITIMER_VIRTUAL,
//! ITIMER_PROF, CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID) are due at a reading of the
//! process's or the thread's CPU time, which is the host time it held the CPU (`charge_cpu`:
//! the run loop counts the time of each slice to the thread that ran it, its system calls
//! included). That is an approximation: there is no split between user and system time, so
//! ITIMER_VIRTUAL runs as ITIMER_PROF does, and the time is the host's, not a count of
//! instructions. They are looked at at the end of each slice, so one can fire up to a slice late.
//!
//! Signals: the interval timers send SIGALRM, SIGVTALRM and SIGPROF to the process (si_code
//! SI_KERNEL). A POSIX timer sends what its sigevent says: SIGEV_SIGNAL (and SIGEV_THREAD, which
//! the kernel treats alike) to the process, SIGEV_THREAD_ID to one of its threads, SIGEV_NONE
//! nothing; the siginfo has si_code SI_TIMER, si_timerid, si_overrun and si_value (and so has a
//! signalfd's signalfd_siginfo: ssi_tid, ssi_overrun, ssi_int and ssi_ptr). Like every
//! signal here (no queued real-time signals) one that is still pending when the timer expires
//! again is not queued twice: the timer counts the expiry as an overrun, as Linux does for a
//! signal not yet delivered, and `timer_getoverrun` and si_overrun report the count of the
//! signal last delivered. When several timers send the same signal and it coalesces, the
//! siginfo is the first one's.
//!
//! Rules of fork and exec: a child has no timers; interval timers stay across exec and POSIX
//! timers are deleted by it; a process's end deletes both. A snapshot keeps the armed timers as
//! the time left, and the CPU time counted so far: ITIMER_REAL in its process's record (where
//! ARMLUSR6 has it), the rest in a section of their own at the end (ARMLUSR7).

use std::collections::BTreeMap;
use std::time::Duration;

use crate::errno::{EAGAIN, EFAULT, EINVAL, EOPNOTSUPP};
use crate::host::Instant;
use crate::ser::{R, W};
use crate::signal::{SI_KERNEL, SI_TIMER};
use crate::{Process, Ret};


const NS: u64 = 1_000_000_000;
/// The longest a timer is set for (an `Instant` can't go much further): about 68 years
const FAR: u64 = (1 << 31) * NS;
/// DELAYTIMER_MAX
const MAX_OVERRUN: u64 = i32::MAX as u64;
const MAX_TIMERS: usize = 1024;

const SIGALRM: u32 = 14;
const SIGVTALRM: u32 = 26;
const SIGPROF: u32 = 27;
const SIGRTMAX: i32 = 64;

const ITIMER_REAL: u64 = 0;
const ITIMER_PROF: u64 = 2;

const SIGEV_SIGNAL: i32 = 0;
const SIGEV_NONE: i32 = 1;
const SIGEV_THREAD: i32 = 2;
const SIGEV_THREAD_ID: i32 = 4;

const TIMER_ABSTIME: u64 = 1;

/// What a timer is measured on.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Scope {
    /// The host's clock
    Wall,
    /// The CPU time of a process (its pid) or a thread (its tid)
    Proc(i64),
    Thread(i64),
}

/// When a timer is due.
#[derive(Clone, Copy, Debug)]
enum Due {
    At(Instant),
    /// A reading of the CPU time of its scope, in ns
    Cpu(u64),
}

#[derive(Clone, Copy, Debug)]
struct Arm {
    due: Due,
    /// ns to reload it with once it is due (0: it ends)
    interval: u64,
}

impl Arm {
    #[allow(clippy::assign_op_pattern)] // (the wasm host's Instant has no `+=`)
    fn advance(&mut self, ns: u64) {
        match &mut self.due {
            Due::At(t) => *t = *t + Duration::from_nanos(ns),
            Due::Cpu(c) => *c = c.saturating_add(ns),
        }
    }
}

#[derive(Clone, Copy)]
struct ITimer {
    pid: i64,
    which: u32,
    arm: Arm,
}

impl ITimer {
    fn scope(&self) -> Scope {
        if self.which == ITIMER_REAL as u32 {
            Scope::Wall
        } else {
            Scope::Proc(self.pid)
        }
    }
}

#[derive(Clone)]
struct Posix {
    pid: i64,
    id: i32,
    /// 0 or 1 for a wall-clock timer (CLOCK_REALTIME or the monotonic clocks), 2 or 3 for CPU time
    clock: u32,
    scope: Scope,
    /// The signal (0: SIGEV_NONE), its thread (SIGEV_THREAD_ID) and its sigval
    sig: u32,
    thread: Option<i64>,
    value: u64,
    arm: Option<Arm>,
    /// Its signal is pending, not yet delivered
    queued: bool,
    /// Expiries counted while it was pending, and the count of the signal last delivered
    overrun: u64,
    last: i32,
}

/// What a POSIX timer's siginfo has beyond the signal and its code.
pub(crate) struct TimerInfo {
    id: i32,
    overrun: i32,
    value: u64,
}

impl TimerInfo {
    /// si_timerid, si_overrun and si_value of a siginfo
    pub(crate) fn put(&self, b: &mut [u8]) {
        b[16..20].copy_from_slice(&self.id.to_le_bytes());
        b[20..24].copy_from_slice(&self.overrun.to_le_bytes());
        b[24..32].copy_from_slice(&self.value.to_le_bytes());
    }

    /// ssi_tid, ssi_overrun, ssi_int and ssi_ptr of a signalfd_siginfo
    pub(crate) fn put_signalfd(&self, b: &mut [u8]) {
        b[24..28].copy_from_slice(&self.id.to_le_bytes());
        b[32..36].copy_from_slice(&self.overrun.to_le_bytes());
        b[44..48].copy_from_slice(&(self.value as u32).to_le_bytes());
        b[48..56].copy_from_slice(&self.value.to_le_bytes());
    }
}

/// The timers of every process, and the CPU time they and the clocks count.
#[derive(Default)]
pub(crate) struct Timers {
    itimers: Vec<ITimer>,
    posix: Vec<Posix>,
    /// The id the next POSIX timer of a process gets
    next_id: BTreeMap<i64, i32>,
    cpu_proc: BTreeMap<i64, u64>,
    cpu_thread: BTreeMap<i64, u64>,
    /// The thread whose slice is running, and since when (counted by `charge_cpu`)
    running: Vec<Option<(i64, i64, Instant)>>,
}

/// How many times a timer `left` ns from its due time (negative: past it) with `interval` has
/// expired: once, and again for each whole interval since.
fn expiries(left: i128, interval: u64) -> u64 {
    // (No interval: once)
    ((-left) as u64)
        .checked_div(interval)
        .map_or(1, |q| q.saturating_add(1))
}

fn clamp(ns: u128) -> u64 {
    ns.min(FAR as u128) as u64
}

impl Timers {
    fn cpu(&self, scope: Scope, now: Instant) -> u64 {
        // What the slices running now (one per CPU) have had so far
        let extra = |mine: &dyn Fn(&(i64, i64, Instant)) -> bool| -> u64 {
            self.running
                .iter()
                .flatten()
                .filter(|r| mine(r))
                .map(|r| now.saturating_duration_since(r.2).as_nanos() as u64)
                .sum()
        };
        match scope {
            Scope::Wall => 0,
            Scope::Proc(pid) => {
                self.cpu_proc.get(&pid).copied().unwrap_or(0) + extra(&|r| r.0 == pid)
            },
            Scope::Thread(tid) => {
                self.cpu_thread.get(&tid).copied().unwrap_or(0) + extra(&|r| r.1 == tid)
            },
        }
    }

    /// CPU time of a process, for CLOCK_PROCESS_CPUTIME_ID
    pub(crate) fn proc_cpu_ns(&self, pid: i64) -> u64 {
        self.cpu(Scope::Proc(pid), Instant::now())
    }

    /// CPU time of a thread, for CLOCK_THREAD_CPUTIME_ID
    pub(crate) fn thread_cpu_ns(&self, tid: i64) -> u64 {
        self.cpu(Scope::Thread(tid), Instant::now())
    }

    /// ns until `arm` is due (negative: it is past)
    fn left(&self, arm: &Arm, scope: Scope, now: Instant) -> i128 {
        match arm.due {
            Due::At(t) if t >= now => t.saturating_duration_since(now).as_nanos() as i128,
            Due::At(t) => -(now.saturating_duration_since(t).as_nanos() as i128),
            Due::Cpu(c) => c as i128 - self.cpu(scope, now) as i128,
        }
    }

    /// An arm that is `ns` from now (relative), or due at the clock's reading `abs`
    fn arm(&self, scope: Scope, ns: u64, interval: u64, now: Instant) -> Arm {
        let due = match scope {
            Scope::Wall => Due::At(now + Duration::from_nanos(ns)),
            _ => Due::Cpu(self.cpu(scope, now).saturating_add(ns)),
        };
        Arm { due, interval }
    }

    /// What of process `pid`'s timers execve ends
    pub(crate) fn exec(&mut self, pid: i64) {
        self.posix.retain(|t| t.pid != pid);
        self.next_id.remove(&pid);
    }

    /// Process `pid`, with threads `tids`, has ended
    pub(crate) fn end(&mut self, pid: i64, tids: &[i64]) {
        self.itimers.retain(|t| t.pid != pid);
        self.exec(pid);
        self.cpu_proc.remove(&pid);
        for t in tids {
            self.cpu_thread.remove(t);
        }
    }

    /// The signal of a POSIX timer was taken (delivered, or waited for): what its siginfo has.
    pub(crate) fn dequeue(&mut self, pid: i64, sig: u32, tid: i64) -> Option<TimerInfo> {
        let mine = |t: &Posix| t.pid == pid && t.queued && t.sig == sig;
        let first = self
            .posix
            .iter()
            .position(|t| mine(t) && t.thread.is_none_or(|x| x == tid))
            .or_else(|| self.posix.iter().position(mine))?;
        let info = {
            let t = &self.posix[first];
            TimerInfo {
                id: t.id,
                overrun: t.overrun.min(MAX_OVERRUN) as i32,
                value: t.value,
            }
        };
        // (The timers that expired into the same signal have it delivered too)
        for t in self.posix.iter_mut().filter(|t| mine(t)) {
            t.queued = false;
            t.last = t.overrun.min(MAX_OVERRUN) as i32;
            t.overrun = 0;
        }
        Some(info)
    }

    fn itimer_get(&self, pid: i64, which: u32, now: Instant) -> (u64, u64) {
        self.itimers
            .iter()
            .find(|t| t.pid == pid && t.which == which)
            .map_or((0, 0), |t| {
                // (Armed, so not zero: a timer that is due and not yet signalled has 1 us left)
                let left = self.left(&t.arm, t.scope(), now).max(1_000) as u64;
                (left, t.arm.interval)
            })
    }

    /// The timers' section of a snapshot: the CPU time counted, the next ids, and the armed
    /// timers but ITIMER_REAL
    pub(crate) fn save(&self, w: &mut W, now: Instant) {
        w.u(self.cpu_proc.len() as u64);
        for (&pid, &ns) in &self.cpu_proc {
            w.u(pid as u64);
            w.u(ns);
        }
        w.u(self.cpu_thread.len() as u64);
        for (&tid, &ns) in &self.cpu_thread {
            w.u(tid as u64);
            w.u(ns);
        }
        w.u(self.next_id.len() as u64);
        for (&pid, &id) in &self.next_id {
            w.u(pid as u64);
            w.u(id as u64);
        }
        let arm = |w: &mut W, a: Option<Arm>| match a {
            None => w.u(0),
            Some(a) => {
                match a.due {
                    Due::At(t) => {
                        w.u(1);
                        w.u(t.saturating_duration_since(now).as_nanos() as u64);
                    },
                    Due::Cpu(c) => {
                        w.u(2);
                        w.u(c);
                    },
                }
                w.u(a.interval);
            },
        };
        // (ITIMER_REAL is in its process's record: `save_real`)
        let itimers: Vec<&ITimer> = self
            .itimers
            .iter()
            .filter(|t| t.which != ITIMER_REAL as u32)
            .collect();
        w.u(itimers.len() as u64);
        for t in itimers {
            w.u(t.pid as u64);
            w.u(t.which as u64);
            arm(w, Some(t.arm));
        }
        w.u(self.posix.len() as u64);
        for t in &self.posix {
            w.u(t.pid as u64);
            w.u(t.id as u64);
            w.u(t.clock as u64);
            match t.scope {
                Scope::Wall => w.u(0),
                Scope::Proc(p) => {
                    w.u(1);
                    w.u(p as u64);
                },
                Scope::Thread(p) => {
                    w.u(2);
                    w.u(p as u64);
                },
            }
            w.u(t.sig as u64);
            w.u(t.thread.map_or(u64::MAX, |x| x as u64));
            w.u(t.value);
            arm(w, t.arm);
            w.u(t.queued as u64);
            w.u(t.overrun);
            w.u(t.last as u64);
        }
    }

    /// Process `pid`'s ITIMER_REAL, for its record in a snapshot: the time left and the
    /// interval, or u64::MAX when it is not armed
    pub(crate) fn save_real(&self, w: &mut W, pid: i64, now: Instant) {
        match self
            .itimers
            .iter()
            .find(|t| t.pid == pid && t.which == ITIMER_REAL as u32)
        {
            Some(t) => {
                w.u(self.left(&t.arm, Scope::Wall, now).max(0) as u64);
                w.u(t.arm.interval);
            },
            None => w.u(u64::MAX),
        }
    }

    /// Process `pid`'s ITIMER_REAL from its record (`save_real`)
    pub(crate) fn load_real(&mut self, r: &mut R, pid: i64, now: Instant) -> Result<(), String> {
        let left = r.u()?;
        if left != u64::MAX {
            let arm = Arm {
                due: Due::At(now + Duration::from_nanos(clamp(left as u128))),
                interval: r.u()?,
            };
            self.itimers.push(ITimer {
                pid,
                which: ITIMER_REAL as u32,
                arm,
            });
        }
        Ok(())
    }

    /// The timers' section of a snapshot (`save`), into what `load_real` has read
    pub(crate) fn load(&mut self, r: &mut R, now: Instant) -> Result<(), String> {
        let s = self;
        for _ in 0..r.u()? {
            let (pid, ns) = (r.u()? as i64, r.u()?);
            s.cpu_proc.insert(pid, ns);
        }
        for _ in 0..r.u()? {
            let (tid, ns) = (r.u()? as i64, r.u()?);
            s.cpu_thread.insert(tid, ns);
        }
        for _ in 0..r.u()? {
            let (pid, id) = (r.u()? as i64, r.u()? as i32);
            s.next_id.insert(pid, id);
        }
        let arm = |r: &mut R| -> Result<Option<Arm>, String> {
            let due = match r.u()? {
                0 => return Ok(None),
                1 => Due::At(now + Duration::from_nanos(clamp(r.u()? as u128))),
                2 => Due::Cpu(r.u()?),
                k => return Err(format!("unknown timer due {k}")),
            };
            Ok(Some(Arm {
                due,
                interval: r.u()?,
            }))
        };
        for _ in 0..r.u()? {
            let (pid, which) = (r.u()? as i64, r.u()? as u32);
            let arm = arm(r)?.ok_or("an interval timer that is not armed")?;
            if which > ITIMER_PROF as u32 {
                return Err(format!("unknown interval timer {which}"));
            }
            s.itimers.push(ITimer { pid, which, arm });
        }
        for _ in 0..r.u()? {
            let (pid, id, clock) = (r.u()? as i64, r.u()? as i32, r.u()? as u32);
            let scope = match r.u()? {
                0 => Scope::Wall,
                1 => Scope::Proc(r.u()? as i64),
                2 => Scope::Thread(r.u()? as i64),
                k => return Err(format!("unknown timer scope {k}")),
            };
            let sig = r.u()? as u32;
            let thread = match r.u()? {
                u64::MAX => None,
                t => Some(t as i64),
            };
            let value = r.u()?;
            let a = arm(r)?;
            s.posix.push(Posix {
                pid,
                id,
                clock,
                scope,
                sig,
                thread,
                value,
                arm: a,
                queued: r.u()? != 0,
                overrun: r.u()?,
                last: r.u()? as i32,
            });
        }
        Ok(())
    }
}

/// The nanoseconds of a timeval of the program's, or None when it isn't valid.
fn timeval_ns(sec: u64, usec: u64) -> Option<u64> {
    let (s, u) = (sec as i64, usec as i64);
    (s >= 0 && (0..1_000_000).contains(&u))
        .then(|| clamp(s as u128 * NS as u128 + u as u128 * 1000))
}

/// The nanoseconds of a timespec of the program's, or None when it isn't valid.
fn timespec_ns(sec: u64, nsec: u64) -> Option<u64> {
    let (s, n) = (sec as i64, nsec as i64);
    (s >= 0 && (0..NS as i64).contains(&n)).then(|| clamp(s as u128 * NS as u128 + n as u128))
}

impl Process {
    // ------------------------------------------------------------ running

    /// Counts the time since the thread that runs began its slice to it and its process.
    pub(crate) fn charge_cpu(&mut self) {
        let Some((pid, tid, at)) = self.timers.running.get_mut(self.cpu).and_then(Option::take)
        else {
            return;
        };
        if self.proc_ref(pid).is_none() {
            return;
        }
        let ns = at.elapsed().as_nanos() as u64;
        *self.timers.cpu_proc.entry(pid).or_default() += ns;
        *self.timers.cpu_thread.entry(tid).or_default() += ns;
        // (Threads that ended leave their counts: the process's own have them)
        if self.timers.cpu_thread.len() > self.threads.len() + 16 {
            let threads = &self.threads;
            self.timers
                .cpu_thread
                .retain(|tid, _| threads.iter().any(|t| t.tid == *tid));
        }
    }

    /// Thread `tid` of process `pid` is about to run a slice.
    pub(crate) fn begin_slice(&mut self, pid: i64, tid: i64) {
        let cpu = self.cpu;
        if self.timers.running.len() <= cpu {
            self.timers.running.resize(cpu + 1, None);
        }
        self.timers.running[cpu] = Some((pid, tid, Instant::now()));
    }

    /// The first time an armed timer that signals is due, to wake the machine at.
    pub(crate) fn timer_wake(&self) -> Option<Instant> {
        let it = self.timers.itimers.iter().map(|t| t.arm);
        let px = self
            .timers
            .posix
            .iter()
            .filter(|t| t.sig != 0)
            .filter_map(|t| t.arm);
        it.chain(px)
            .filter_map(|a| match a.due {
                Due::At(t) => Some(t),
                Due::Cpu(_) => None,
            })
            .min()
    }

    /// Signals what is due, and sets its timers to go on or to end.
    pub(crate) fn fire_timers(&mut self) {
        if self.timers.itimers.is_empty() && self.timers.posix.is_empty() {
            return;
        }
        let now = Instant::now();
        let itimers = std::mem::take(&mut self.timers.itimers);
        for mut t in itimers {
            let left = self.timers.left(&t.arm, t.scope(), now);
            if left <= 0 {
                let n = expiries(left, t.arm.interval);
                let sig = match t.which {
                    0 => SIGALRM,
                    1 => SIGVTALRM,
                    _ => SIGPROF,
                };
                self.raise_in(t.pid, sig, None, SI_KERNEL);
                t.arm.advance(n.saturating_mul(t.arm.interval));
                if t.arm.interval == 0 {
                    continue;
                }
            }
            self.timers.itimers.push(t);
        }
        let mut posix = std::mem::take(&mut self.timers.posix);
        for t in &mut posix {
            // (A signal that was ignored or cleared is not pending any more)
            let target = self.timer_target(t);
            t.queued = t.queued && self.timer_signal_pending(t.pid, t.sig, target.flatten());
            let Some(arm) = t.arm else {
                continue;
            };
            let left = self.timers.left(&arm, t.scope, now);
            if left > 0 {
                continue;
            }
            let n = expiries(left, arm.interval);
            t.arm = (arm.interval != 0).then(|| {
                let mut a = arm;
                a.advance(n.saturating_mul(arm.interval));
                a
            });
            // (No signal, or its thread is gone: nobody to signal)
            let Some(target) = target.filter(|_| t.sig != 0) else {
                continue;
            };
            if t.queued {
                // Its signal is not delivered yet: this expiry is counted, not queued
                t.overrun = (t.overrun + n).min(MAX_OVERRUN);
                continue;
            }
            t.overrun = n - 1;
            self.raise_in(t.pid, t.sig, target, SI_TIMER);
            t.queued = self.timer_signal_pending(t.pid, t.sig, target);
        }
        self.timers.posix = posix;
    }

    /// The thread a timer's signal is for: Some(None) the process, Some(Some(i)) thread `i`,
    /// None when that thread is gone.
    fn timer_target(&self, t: &Posix) -> Option<Option<usize>> {
        match t.thread {
            None => Some(None),
            Some(tid) => self
                .threads
                .iter()
                .position(|x| x.tid == tid && x.pid == t.pid && !x.exited)
                .map(Some),
        }
    }

    /// Whether a timer's signal is pending for the process (or its thread `target`).
    fn timer_signal_pending(&self, pid: i64, sig: u32, target: Option<usize>) -> bool {
        let timer = Some(&SI_TIMER);
        match target {
            Some(t) => self.threads[t].sig.pending.get(&sig) == timer,
            None => {
                self.proc_ref(pid)
                    .is_some_and(|p| p.sig.pending.get(&sig) == timer)
                    || self
                        .threads
                        .iter()
                        .any(|t| t.pid == pid && t.sig.pending.get(&sig) == timer)
            },
        }
    }

    /// The signal of a POSIX timer of process `pid` was taken by thread `tid`.
    pub(crate) fn timer_dequeue(&mut self, pid: i64, sig: u32, tid: i64) -> Option<TimerInfo> {
        self.timers.dequeue(pid, sig, tid)
    }

    // ------------------------------------------------------------ the syscalls

    /// getitimer, setitimer, timer_create, timer_gettime, timer_getoverrun, timer_settime and
    /// timer_delete (nr 102, 103, 107 to 111)
    pub(crate) fn sys_timer(&mut self, nr: u64, cur: usize, a: &[u64; 6]) -> Ret {
        // What is due has expired before a call looks at a timer
        self.fire_timers();
        let r = match nr {
            102 => self.getitimer(a[0], a[1]),
            103 => self.setitimer(a[0], a[1], a[2]),
            107 => self.timer_create(cur, a[0], a[1], a[2]),
            108 => self.timer_gettime(a[0], a[1]),
            109 => self.timer_getoverrun(a[0]),
            110 => self.timer_settime(a[0], a[1], a[2], a[3]),
            _ => self.timer_delete(a[0]),
        };
        Ret::Val(r.unwrap_or_else(|e| -e))
    }

    fn put_itimerval(&self, at: u64, interval: u64, value: u64) -> bool {
        let mut b = [0u8; 32];
        for (i, ns) in [interval, value].into_iter().enumerate() {
            b[16 * i..16 * i + 8].copy_from_slice(&(ns / NS).to_le_bytes());
            b[16 * i + 8..16 * i + 16].copy_from_slice(&(ns % NS / 1000).to_le_bytes());
        }
        self.write(at, &b)
    }

    pub(crate) fn put_itimerspec(&self, at: u64, interval: u64, value: u64) -> bool {
        let mut b = [0u8; 32];
        for (i, ns) in [interval, value].into_iter().enumerate() {
            b[16 * i..16 * i + 8].copy_from_slice(&(ns / NS).to_le_bytes());
            b[16 * i + 8..16 * i + 16].copy_from_slice(&(ns % NS).to_le_bytes());
        }
        self.write(at, &b)
    }

    fn getitimer(&mut self, which: u64, cur: u64) -> Result<i64, i64> {
        if which > ITIMER_PROF {
            return Err(EINVAL);
        }
        let (value, interval) = self
            .timers
            .itimer_get(self.p.pid, which as u32, Instant::now());
        self.put_itimerval(cur, interval, value)
            .then_some(0)
            .ok_or(EFAULT)
    }

    fn setitimer(&mut self, which: u64, new: u64, old: u64) -> Result<i64, i64> {
        if which > ITIMER_PROF {
            return Err(EINVAL);
        }
        // (Linux takes no new value as a zero one)
        let (interval, value) = if new == 0 {
            (0, 0)
        } else {
            let b = self.bytes_at(new, 32).ok_or(EFAULT)?;
            let u = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
            (
                timeval_ns(u(0), u(1)).ok_or(EINVAL)?,
                timeval_ns(u(2), u(3)).ok_or(EINVAL)?,
            )
        };
        let (pid, which) = (self.p.pid, which as u32);
        let now = Instant::now();
        let (left, was) = self.timers.itimer_get(pid, which, now);
        self.timers
            .itimers
            .retain(|t| !(t.pid == pid && t.which == which));
        if value != 0 {
            let mut t = ITimer {
                pid,
                which,
                arm: Arm {
                    due: Due::Cpu(0),
                    interval,
                },
            };
            t.arm = self.timers.arm(t.scope(), value, interval, now);
            self.timers.itimers.push(t);
        }
        if old != 0 && !self.put_itimerval(old, was, left) {
            return Err(EFAULT);
        }
        Ok(0)
    }

    fn timer_create(&mut self, cur: usize, clock: u64, sev: u64, out: u64) -> Result<i64, i64> {
        let pid = self.p.pid;
        // The clock: the realtime ones (and TAI, with no offset) and the monotonic ones (and
        // boot time, with no suspend) are wall clocks; the alarm clocks too (this is root)
        let (clock, scope) = match clock as i64 {
            0 | 8 | 11 => (0, Scope::Wall),
            1 | 7 | 9 => (1, Scope::Wall),
            2 => (2, Scope::Proc(pid)),
            3 => (3, Scope::Thread(self.threads[cur].tid)),
            // CLOCK_MONOTONIC_RAW and the coarse clocks only tell the time
            4..=6 => return Err(EOPNOTSUPP),
            _ => return Err(EINVAL),
        };
        let id = self.timers.next_id.get(&pid).copied().unwrap_or(0);
        let (sig, thread, value) = if sev == 0 {
            (SIGALRM, None, id as u64)
        } else {
            let b = self.bytes_at(sev, 64).ok_or(EFAULT)?;
            let i = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().unwrap());
            let value = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let (signo, notify) = (i(8), i(12));
            let in_range = (1..=SIGRTMAX).contains(&signo);
            match notify {
                SIGEV_NONE => (0, None, value),
                SIGEV_SIGNAL | SIGEV_THREAD if in_range => (signo as u32, None, value),
                SIGEV_THREAD_ID if in_range => {
                    let tid = i(16) as i64;
                    let mine = self
                        .threads
                        .iter()
                        .any(|t| t.tid == tid && t.pid == pid && !t.exited);
                    if !mine {
                        return Err(EINVAL);
                    }
                    (signo as u32, Some(tid), value)
                },
                _ => return Err(EINVAL),
            }
        };
        if self.timers.posix.iter().filter(|t| t.pid == pid).count() >= MAX_TIMERS {
            return Err(EAGAIN);
        }
        if !self.put_u32(out, id as u32) {
            return Err(EFAULT);
        }
        self.timers.next_id.insert(pid, id.wrapping_add(1).max(0));
        self.timers.posix.push(Posix {
            pid,
            id,
            clock,
            scope,
            sig,
            thread,
            value,
            arm: None,
            queued: false,
            overrun: 0,
            last: 0,
        });
        Ok(0)
    }

    fn posix_index(&self, id: u64) -> Result<usize, i64> {
        let pid = self.p.pid;
        self.timers
            .posix
            .iter()
            .position(|t| t.pid == pid && t.id == id as i32 && id <= i32::MAX as u64)
            .ok_or(EINVAL)
    }

    fn timer_gettime(&mut self, id: u64, cur: u64) -> Result<i64, i64> {
        let k = self.posix_index(id)?;
        let t = &self.timers.posix[k];
        let (value, interval) = t.arm.map_or((0, 0), |a| {
            let left = self.timers.left(&a, t.scope, Instant::now());
            (left.max(1) as u64, a.interval)
        });
        self.put_itimerspec(cur, interval, value)
            .then_some(0)
            .ok_or(EFAULT)
    }

    fn timer_getoverrun(&mut self, id: u64) -> Result<i64, i64> {
        let k = self.posix_index(id)?;
        Ok(self.timers.posix[k].last as i64)
    }

    fn timer_settime(&mut self, id: u64, flags: u64, new: u64, old: u64) -> Result<i64, i64> {
        let k = self.posix_index(id)?;
        if flags & !TIMER_ABSTIME != 0 {
            return Err(EINVAL);
        }
        let b = self.bytes_at(new, 32).ok_or(EFAULT)?;
        let u = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().unwrap());
        let (interval, value) = (
            timespec_ns(u(0), u(1)).ok_or(EINVAL)?,
            timespec_ns(u(2), u(3)).ok_or(EINVAL)?,
        );
        let now = Instant::now();
        let t = &self.timers.posix[k];
        let (scope, clock) = (t.scope, t.clock);
        let was = t.arm.map_or((0, 0), |a| {
            (self.timers.left(&a, scope, now).max(1) as u64, a.interval)
        });
        let arm = (value != 0).then(|| match (scope, flags & TIMER_ABSTIME != 0) {
            (Scope::Wall, true) => Arm {
                due: Due::At(self.deadline_at(clock as u64, Duration::from_nanos(value))),
                interval,
            },
            (_, true) => Arm {
                due: Due::Cpu(value),
                interval,
            },
            (_, false) => self.timers.arm(scope, value, interval, now),
        });
        let t = &mut self.timers.posix[k];
        t.arm = arm;
        t.overrun = 0;
        t.last = 0;
        if old != 0 && !self.put_itimerspec(old, was.1, was.0) {
            return Err(EFAULT);
        }
        Ok(0)
    }

    fn timer_delete(&mut self, id: u64) -> Result<i64, i64> {
        let k = self.posix_index(id)?;
        let t = self.timers.posix.remove(k);
        let shared = self
            .timers
            .posix
            .iter()
            .any(|o| o.pid == t.pid && o.sig == t.sig && o.queued);
        if t.queued && !shared {
            // Its signal is not delivered once it is gone
            let pid = t.pid;
            let from = |p: &mut BTreeMap<u32, i32>| {
                if p.get(&t.sig) == Some(&SI_TIMER) {
                    p.remove(&t.sig);
                }
            };
            if let Some(p) = self.proc_mut(pid) {
                from(&mut p.sig.pending);
            }
            for th in self.threads.iter_mut().filter(|x| x.pid == pid) {
                from(&mut th.sig.pending);
            }
        }
        Ok(0)
    }
}
