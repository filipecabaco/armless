//! timerfd: a descriptor that counts a timer's expirations (timerfd_create, timerfd_settime,
//! timerfd_gettime). A read gives the count since the last read, as 8 bytes, and waits for the
//! next one when there is none; poll and epoll see it readable once it has expired. GHC's runtime
//! (Supabase's PostgREST) makes one at start and stops without it; Erlang's sets one too.
//!
//! The timer is in the descriptor's description (`Kind::TimerFd`), so it goes with the
//! descriptor through dup, fork and SCM_RIGHTS, and is saved with it. Expirations are counted when
//! something looks (`settle`): the run loop wakes the calls that wait (`Process::timerfds_due`)
//! and an idle machine's host comes back at the next one (`Process::next_timerfd`).

use std::time::Duration;

use crate::errno::{EAGAIN, EBADF, EFAULT, EINVAL};
use crate::files::{Desc, Kind};
use crate::host::Instant;
use crate::ser::{R, W};
use crate::{Process, Ret};

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_BOOTTIME: u64 = 7;
const CLOCK_REALTIME_ALARM: u64 = 8;
const CLOCK_BOOTTIME_ALARM: u64 = 9;
const TFD_NONBLOCK: u64 = 0o4000;
const TFD_CLOEXEC: u64 = 0o2000000;
const TFD_TIMER_ABSTIME: u64 = 1;
const TFD_TIMER_CANCEL_ON_SET: u64 = 2;


/// A timerfd's timer: its clock, when it fires next (None: disarmed), every how long after (zero:
/// once), and the expirations not read yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timer {
    pub clock: u64,
    pub next: Option<Instant>,
    pub every: Duration,
    pub ticks: u64,
}

impl Timer {
    /// The expirations up to now counted, and the next one set.
    pub fn settle(&mut self, now: Instant) {
        let Some(at) = self.next else { return };
        if at > now {
            return;
        }
        if self.every.is_zero() {
            self.ticks += 1;
            self.next = None;
            return;
        }
        let late = now.saturating_duration_since(at).as_nanos();
        let n = (late / self.every.as_nanos()) as u64 + 1;
        self.ticks += n;
        self.next = Some(at + Duration::from_nanos((self.every.as_nanos() as u64).saturating_mul(n)));
    }

    /// Whether a read would give something
    pub fn readable(&self, now: Instant) -> bool {
        self.ticks > 0 || self.next.is_some_and(|at| at <= now)
    }

    pub fn save(&self, w: &mut W, now: Instant) {
        w.u(self.clock);
        w.u(self.next.map_or(u64::MAX, |at| at.saturating_duration_since(now).as_nanos() as u64));
        w.u(self.every.as_nanos() as u64);
        w.u(self.ticks);
    }

    pub fn load(r: &mut R) -> Result<Timer, String> {
        let now = Instant::now();
        let clock = r.u()?;
        let left = r.u()?;
        Ok(Timer {
            clock,
            next: (left != u64::MAX).then(|| now + Duration::from_nanos(left)),
            every: Duration::from_nanos(r.u()?),
            ticks: r.u()?,
        })
    }
}

impl Process {
    /// timerfd_create(clockid, flags)
    pub(crate) fn sys_timerfd_create(&mut self, clock: u64, flags: u64) -> Ret {
        if !matches!(clock, CLOCK_REALTIME | CLOCK_MONOTONIC | CLOCK_BOOTTIME | CLOCK_REALTIME_ALARM | CLOCK_BOOTTIME_ALARM)
            || flags & !(TFD_NONBLOCK | TFD_CLOEXEC) != 0
        {
            return Ret::Val(-EINVAL);
        }
        // The alarm clocks are their plain ones: nothing here suspends
        let clock = match clock {
            CLOCK_REALTIME_ALARM => CLOCK_REALTIME,
            CLOCK_BOOTTIME_ALARM => CLOCK_BOOTTIME,
            c => c,
        };
        let d = self.files.add(
            Kind::TimerFd(Timer { clock, next: None, every: Duration::ZERO, ticks: 0 }),
            2 | (flags & TFD_NONBLOCK) as u32,
        );
        Ret::Val(self.new_fd(d, 0, flags & TFD_CLOEXEC != 0))
    }

    /// timerfd_settime(fd, flags, new, old): struct itimerspec { interval, value }
    pub(crate) fn sys_timerfd_settime(&mut self, fd: i32, flags: u64, new: u64, old: u64) -> Ret {
        if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
            return Ret::Val(-EINVAL);
        }
        let Some(t) = self.timerfd(fd) else {
            return Ret::Val(if self.files.get(fd).is_some() { -EINVAL } else { -EBADF });
        };
        let (Some(every), Some(value)) = (self.timespec_checked(new), self.timespec_checked(new + 16)) else {
            return Ret::Val(-EINVAL);
        };
        if old != 0 && !self.put_timerfd_spec(old, &t) {
            return Ret::Val(-EFAULT);
        }
        let next = if value.is_zero() {
            None
        } else if flags & TFD_TIMER_ABSTIME != 0 {
            Some(self.deadline_at(t.clock, value))
        } else {
            // (One too far off to say never comes)
            Instant::now().checked_add(value)
        };
        self.set_timerfd(fd, Timer { clock: t.clock, next, every, ticks: 0 });
        self.poke();
        Ret::Val(0)
    }

    /// timerfd_gettime(fd, cur)
    pub(crate) fn sys_timerfd_gettime(&mut self, fd: i32, cur: u64) -> Ret {
        let Some(t) = self.timerfd(fd) else {
            return Ret::Val(if self.files.get(fd).is_some() { -EINVAL } else { -EBADF });
        };
        Ret::Val(if self.put_timerfd_spec(cur, &t) { 0 } else { -EFAULT })
    }

    /// A read of timerfd `fd`: its expirations since the last read, or the wait for the next.
    pub(crate) fn read_timerfd(&mut self, fd: i32, max: usize, nonblock: bool) -> Result<Vec<u8>, Ret> {
        if max < 8 {
            return Err(Ret::Val(-EINVAL));
        }
        let Some(mut t) = self.timerfd(fd) else {
            return Err(Ret::Val(-EBADF));
        };
        if t.ticks == 0 {
            return Err(if nonblock {
                Ret::Val(-EAGAIN)
            } else {
                // Woken when it fires (timerfds_due), or when it is set again
                Ret::Retry { deadline: None, on_timeout: 0 }
            });
        }
        let n = std::mem::take(&mut t.ticks);
        self.set_timerfd(fd, t);
        Ok(n.to_le_bytes().to_vec())
    }

    /// Every timerfd's expirations counted: when one has fired, what waits on it runs again.
    pub(crate) fn timerfds_due(&mut self) {
        let now = Instant::now();
        let mut fired = false;
        for d in self.files.descs.values_mut() {
            if let Kind::TimerFd(t) = &mut d.kind {
                let before = t.ticks;
                t.settle(now);
                fired |= t.ticks != before;
            }
        }
        if fired {
            self.poke();
        }
    }

    /// When the next timerfd fires, for an idle machine's host to come back then
    pub(crate) fn next_timerfd(&self) -> Option<Instant> {
        self.files
            .descs
            .values()
            .filter_map(|d| match &d.kind {
                Kind::TimerFd(t) => t.next,
                _ => None,
            })
            .min()
    }

    /// The timer of timerfd `fd`, its expirations up to now counted
    fn timerfd(&mut self, fd: i32) -> Option<Timer> {
        let now = Instant::now();
        match self.files.get_mut(fd) {
            Some(Desc { kind: Kind::TimerFd(t), .. }) => {
                t.settle(now);
                Some(*t)
            },
            _ => None,
        }
    }

    fn set_timerfd(&mut self, fd: i32, timer: Timer) {
        if let Some(Desc { kind: Kind::TimerFd(t), .. }) = self.files.get_mut(fd) {
            *t = timer;
        }
    }

    /// A struct timespec at `at`, if it is one (nanoseconds below a second, seconds not negative)
    fn timespec_checked(&self, at: u64) -> Option<Duration> {
        let (s, ns) = (self.u64_at(at)? as i64, self.u64_at(at + 8)? as i64);
        (s >= 0 && (0..1_000_000_000).contains(&ns)).then(|| Duration::new(s as u64, ns as u32))
    }

    /// struct itimerspec { interval, value } of `t` at `at` (a value of at least 1 ns while it
    /// is armed, as Linux's)
    fn put_timerfd_spec(&self, at: u64, t: &Timer) -> bool {
        let left = t.next.map_or(0, |n| {
            (n.saturating_duration_since(Instant::now()).as_nanos() as u64).max(1)
        });
        self.put_itimerspec(at, t.every.as_nanos() as u64, left)
    }
}
