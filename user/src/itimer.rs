//! A process's real-time interval timer (setitimer, getitimer: ITIMER_REAL), which raises
//! SIGALRM. Postgres arms its statement, lock and idle timeouts with it, and refuses a connection
//! when it can't. The virtual and profiling timers (ITIMER_VIRTUAL, ITIMER_PROF) are taken and
//! never fire.

use std::time::Duration;

use crate::host::Instant;
use crate::{Process, Ret};

const ITIMER_REAL: u64 = 0;
const ITIMER_PROF: u64 = 2;
const SIGALRM: u32 = 14;
/// si_code of a signal the kernel sends
const SI_KERNEL: i32 = 0x80;
const EINVAL: i64 = 22;
const EFAULT: i64 = 14;

/// An armed timer: when it fires next, and every how long after (zero: once)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Alarm {
    pub at: Instant,
    pub every: Duration,
}

impl Process {
    /// setitimer(which, new, old)
    pub(crate) fn sys_setitimer(&mut self, which: u64, new: u64, old: u64) -> Ret {
        if which > ITIMER_PROF {
            return Ret::Val(-EINVAL);
        }
        let set = if new == 0 {
            None
        } else {
            match self.itimerval_at(new) {
                Some(v) => Some(v),
                None => return Ret::Val(-EFAULT),
            }
        };
        if old != 0 && !self.put_itimerval(old, if which == ITIMER_REAL { self.p.alarm } else { None }) {
            return Ret::Val(-EFAULT);
        }
        if which == ITIMER_REAL {
            self.p.alarm = set.and_then(|(every, value)| {
                (!value.is_zero()).then(|| Alarm {
                    at: Instant::now() + value,
                    every,
                })
            });
        }
        Ret::Val(0)
    }

    /// getitimer(which, cur)
    pub(crate) fn sys_getitimer(&mut self, which: u64, cur: u64) -> Ret {
        if which > ITIMER_PROF {
            return Ret::Val(-EINVAL);
        }
        let alarm = if which == ITIMER_REAL { self.p.alarm } else { None };
        Ret::Val(if self.put_itimerval(cur, alarm) { 0 } else { -EFAULT })
    }

    /// SIGALRM for every process whose timer is due, which is armed again if it repeats.
    pub(crate) fn fire_alarms(&mut self) {
        let now = Instant::now();
        let mut due = Vec::new();
        for p in std::iter::once(&mut self.p).chain(self.procs.values_mut()) {
            let Some(a) = p.alarm else { continue };
            if a.at > now || p.zombie.is_some() {
                continue;
            }
            due.push(p.pid);
            p.alarm = (!a.every.is_zero()).then(|| {
                // Expirations missed while nothing ran are one signal, as Linux's
                let mut at = a.at + a.every;
                while at <= now {
                    // (The page's Instant has + but not +=)
                    #[allow(clippy::assign_op_pattern)]
                    {
                        at = at + a.every;
                    }
                }
                Alarm { at, every: a.every }
            });
        }
        for pid in due {
            self.raise_in(pid, SIGALRM, None, SI_KERNEL);
        }
    }

    /// When the next timer fires, for an idle machine's host to come back then
    pub(crate) fn next_alarm(&self) -> Option<Instant> {
        std::iter::once(&self.p)
            .chain(self.procs.values())
            .filter(|p| p.zombie.is_none())
            .filter_map(|p| p.alarm.map(|a| a.at))
            .min()
    }

    /// struct itimerval at `at`: (interval, value)
    fn itimerval_at(&self, at: u64) -> Option<(Duration, Duration)> {
        let tv = |o: u64| -> Option<Duration> {
            let (s, us) = (self.u64_at(at + o)? as i64, self.u64_at(at + o + 8)? as i64);
            (s >= 0 && (0..1_000_000).contains(&us))
                .then(|| Duration::from_secs(s as u64) + Duration::from_micros(us as u64))
        };
        Some((tv(0)?, tv(16)?))
    }

    fn put_itimerval(&self, at: u64, alarm: Option<Alarm>) -> bool {
        let (every, left) = alarm.map_or((Duration::ZERO, Duration::ZERO), |a| {
            (a.every, a.at.saturating_duration_since(Instant::now()).max(Duration::from_micros(1)))
        });
        [every, left].iter().enumerate().all(|(i, d)| {
            let o = at + 16 * i as u64;
            self.put_u64(o, d.as_secs()) && self.put_u64(o + 8, d.subsec_micros() as u64)
        })
    }
}
