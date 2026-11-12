//! The scheduler calls, answered as Linux does for a process that may do them (armless is
//! root-like, so nothing is refused for lack of rights).
//!
//! Every thread has a policy, a real-time priority and a nice value, kept per thread as Linux
//! keeps them: `sched_setscheduler`, `sched_setparam`, `sched_setattr` and `setpriority` (PRIO_PROCESS
//! with a tid, PRIO_PGRP and PRIO_USER for every matching thread) set them, `sched_getscheduler`,
//! `sched_getparam`, `sched_getattr`, `getpriority` and `sched_rr_get_interval` read them back, and
//! `sched_get_priority_max` and `_min` give the policies' ranges. A new thread or process
//! inherits its creator's (`Sched::fork`: with SCHED_RESET_ON_FORK it starts as a normal thread
//! with nice 0 or its parent's nonnegative nice, and the flag is cleared), exec keeps them.
//!
//! SCHED_OTHER, SCHED_BATCH and SCHED_IDLE take priority 0, SCHED_FIFO and SCHED_RR 1 to 99.
//! The values are only kept and reported: the one CPU is shared by round robin whatever they
//! are. SCHED_DEADLINE is not available: `sched_setscheduler` refuses it as Linux does without
//! deadline parameters (EINVAL) and `sched_setattr` with EPERM, the error for a policy the
//! caller may not use. `sched_rr_get_interval` says 100 ms for SCHED_RR and 0 for the rest.

use crate::ser::{R, W};
use crate::{Process, Ret};

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const E2BIG: i64 = 7;

pub(crate) const SCHED_OTHER: u32 = 0;
const SCHED_FIFO: u32 = 1;
const SCHED_RR: u32 = 2;
const SCHED_BATCH: u32 = 3;
const SCHED_IDLE: u32 = 5;
const SCHED_DEADLINE: u32 = 6;
const RESET_ON_FORK: u32 = 0x4000_0000;

const FLAG_RESET_ON_FORK: u64 = 0x01;
const FLAG_KEEP_POLICY: u64 = 0x08;
const FLAG_KEEP_PARAMS: u64 = 0x10;
const FLAG_UTIL_CLAMP_MIN: u64 = 0x20;
const FLAG_UTIL_CLAMP_MAX: u64 = 0x40;
/// SCHED_FLAG_ALL: reset on fork, reclaim, dl overrun, keep policy, keep params, util clamps
const FLAGS_ALL: u64 = 0x7f;

/// struct sched_attr: its first version (up to sched_period) and with the utilization clamps
const ATTR_VER0: usize = 48;
const ATTR_SIZE: usize = 56;
const PAGE: u32 = 4096;

const PRIO_PROCESS: u64 = 0;
const PRIO_PGRP: u64 = 1;
const PRIO_USER: u64 = 2;

/// A thread's scheduling attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sched {
    pub policy: u32,
    /// 1 to 99 for SCHED_FIFO and SCHED_RR, else 0
    pub prio: u32,
    /// -20 to 19
    pub nice: i32,
    pub reset_on_fork: bool,
}

impl Default for Sched {
    fn default() -> Sched {
        Sched {
            policy: SCHED_OTHER,
            prio: 0,
            nice: 0,
            reset_on_fork: false,
        }
    }
}

fn is_rt(policy: u32) -> bool {
    policy == SCHED_FIFO || policy == SCHED_RR
}

impl Sched {
    /// What a thread or process a clone makes starts with.
    pub fn fork(&self) -> Sched {
        if !self.reset_on_fork {
            return *self;
        }
        Sched {
            policy: if is_rt(self.policy) { SCHED_OTHER } else { self.policy },
            prio: 0,
            nice: self.nice.max(0),
            reset_on_fork: false,
        }
    }

    pub fn save(&self, w: &mut W) {
        w.u(self.policy as u64);
        w.u(self.prio as u64);
        w.u(self.nice as i64 as u64);
        w.u(self.reset_on_fork as u64);
    }

    pub fn load(r: &mut R) -> Result<Sched, String> {
        let policy = r.u()? as u32;
        let prio = r.u()? as u32;
        let nice = r.u()? as i64;
        let reset_on_fork = r.u()? != 0;
        if !(0..=6).contains(&policy) || prio > 99 || !(-20..=19).contains(&nice) {
            return Err("a thread's scheduling is out of range".into());
        }
        Ok(Sched {
            policy,
            prio,
            nice: nice as i32,
            reset_on_fork,
        })
    }
}

/// The priorities a policy takes: (min, max).
fn range(policy: u32) -> Option<(u32, u32)> {
    match policy {
        SCHED_FIFO | SCHED_RR => Some((1, 99)),
        SCHED_OTHER | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Some((0, 0)),
        _ => None,
    }
}

impl Process {
    /// The thread a pid argument names: 0 is the caller.
    fn sched_thread(&self, cur: usize, tid: i32) -> Result<usize, i64> {
        if tid < 0 {
            return Err(-EINVAL);
        }
        if tid == 0 {
            return Ok(cur);
        }
        self.threads
            .iter()
            .position(|t| t.tid == tid as i64 && !t.exited)
            .ok_or(-ESRCH)
    }

    /// The threads setpriority and getpriority act on.
    fn priority_threads(&self, cur: usize, which: u64, who: i32) -> Result<Vec<usize>, i64> {
        let found: Vec<usize> = match which {
            PRIO_PROCESS => {
                if who < 0 {
                    return Err(-ESRCH);
                }
                self.sched_thread(cur, who).ok().into_iter().collect()
            },
            PRIO_PGRP => {
                let pgid =
                    if who == 0 { self.proc_pgid(self.threads[cur].pid) } else { Some(who as i64) };
                (0..self.threads.len())
                    .filter(|&i| {
                        let t = &self.threads[i];
                        !t.exited && self.proc_pgid(t.pid) == pgid
                    })
                    .collect()
            },
            // Everything runs as user 0
            PRIO_USER => {
                if who != 0 {
                    Vec::new()
                } else {
                    (0..self.threads.len())
                        .filter(|&i| !self.threads[i].exited)
                        .collect()
                }
            },
            _ => return Err(-EINVAL),
        };
        if found.is_empty() {
            Err(-ESRCH)
        } else {
            Ok(found)
        }
    }

    fn proc_pgid(&self, pid: i64) -> Option<i64> {
        self.proc_ref(pid).map(|p| p.pgid)
    }

    /// Check and apply a policy and priority to a thread: `reset` is its new SCHED_RESET_ON_FORK.
    fn set_policy(&mut self, t: usize, policy: u32, prio: u32, reset: bool) -> i64 {
        let Some(_) = range(policy) else {
            return -EINVAL;
        };
        if prio > 99 || is_rt(policy) != (prio != 0) {
            return -EINVAL;
        }
        if policy == SCHED_DEADLINE {
            return -EINVAL;
        }
        let s = &mut self.threads[t].sched;
        s.policy = policy;
        s.prio = prio;
        s.reset_on_fork = reset;
        0
    }

    /// The scheduler calls (aarch64 numbers 118 to 121, 125 to 127, 140, 141, 274 and 275).
    pub(crate) fn sys_sched(&mut self, nr: u64, cur: usize, a: &[u64; 6]) -> Ret {
        Ret::Val(match self.sched_call(nr, cur, a) {
            Ok(v) => v,
            Err(e) => e,
        })
    }

    fn sched_call(&mut self, nr: u64, cur: usize, a: &[u64; 6]) -> Result<i64, i64> {
        let pid = a[0] as i32;
        match nr {
            // sched_setparam(pid, param)
            118 => {
                if a[1] == 0 || pid < 0 {
                    return Err(-EINVAL);
                }
                let prio = self.u32_at(a[1]).ok_or(-EFAULT)?;
                let t = self.sched_thread(cur, pid)?;
                let s = self.threads[t].sched;
                match self.set_policy(t, s.policy, prio, s.reset_on_fork) {
                    0 => Ok(0),
                    e => Err(e),
                }
            },
            // sched_setscheduler(pid, policy, param)
            119 => {
                let policy = a[1] as u32;
                if a[2] == 0 || pid < 0 || (policy as i32) < 0 {
                    return Err(-EINVAL);
                }
                let prio = self.u32_at(a[2]).ok_or(-EFAULT)?;
                let t = self.sched_thread(cur, pid)?;
                match self.set_policy(
                    t,
                    policy & !RESET_ON_FORK,
                    prio,
                    policy & RESET_ON_FORK != 0,
                ) {
                    0 => Ok(0),
                    e => Err(e),
                }
            },
            // sched_getscheduler(pid)
            120 => {
                let s = self.threads[self.sched_thread(cur, pid)?].sched;
                Ok((s.policy | if s.reset_on_fork { RESET_ON_FORK } else { 0 }) as i64)
            },
            // sched_getparam(pid, param)
            121 => {
                if a[1] == 0 || pid < 0 {
                    return Err(-EINVAL);
                }
                let s = self.threads[self.sched_thread(cur, pid)?].sched;
                if !self.put_u32(a[1], s.prio) {
                    return Err(-EFAULT);
                }
                Ok(0)
            },
            // sched_get_priority_max(policy), sched_get_priority_min(policy)
            125 | 126 => match range(a[0] as u32) {
                Some((min, max)) => Ok(if nr == 125 { max } else { min } as i64),
                None => Err(-EINVAL),
            },
            // sched_rr_get_interval(pid, timespec)
            127 => {
                if pid < 0 {
                    return Err(-EINVAL);
                }
                let s = self.threads[self.sched_thread(cur, pid)?].sched;
                let ns: u64 = if s.policy == SCHED_RR { 100_000_000 } else { 0 };
                let mut ts = [0u8; 16];
                ts[..8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
                ts[8..].copy_from_slice(&(ns % 1_000_000_000).to_le_bytes());
                if !self.write(a[1], &ts) {
                    return Err(-EFAULT);
                }
                Ok(0)
            },
            // setpriority(which, who, prio)
            140 => {
                let who = a[1] as i32;
                let nice = (a[2] as i32).clamp(-20, 19);
                for t in self.priority_threads(cur, a[0], who)? {
                    self.threads[t].sched.nice = nice;
                }
                Ok(0)
            },
            // getpriority(which, who): the raw call gives 20 - nice of the best one
            141 => {
                let who = a[1] as i32;
                let best = self
                    .priority_threads(cur, a[0], who)?
                    .into_iter()
                    .map(|t| self.threads[t].sched.nice)
                    .min()
                    .unwrap_or(0);
                Ok(20 - best as i64)
            },
            // sched_setattr(pid, attr, flags)
            274 => self.sched_setattr(cur, a),
            // sched_getattr(pid, attr, size, flags)
            275 => self.sched_getattr(cur, a),
            _ => Err(-EINVAL),
        }
    }

    fn sched_setattr(&mut self, cur: usize, a: &[u64; 6]) -> Result<i64, i64> {
        let pid = a[0] as i32;
        if a[1] == 0 || pid < 0 || a[2] != 0 {
            return Err(-EINVAL);
        }
        let mut size = self.u32_at(a[1]).ok_or(-EFAULT)?;
        if size == 0 {
            size = ATTR_VER0 as u32;
        }
        let too_big = |p: &Process| {
            // Linux says what size it knows
            p.put_u32(a[1], ATTR_SIZE as u32);
            -E2BIG
        };
        if size > PAGE || (size as usize) < ATTR_VER0 {
            return Err(too_big(self));
        }
        let mut raw = self.bytes_at(a[1], size as usize).ok_or(-EFAULT)?;
        if raw.len() > ATTR_SIZE && raw[ATTR_SIZE..].iter().any(|&b| b != 0) {
            return Err(too_big(self));
        }
        raw.resize(ATTR_SIZE, 0);
        let u32_of = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
        let policy = u32_of(4);
        let flags = u64::from_le_bytes(raw[8..16].try_into().unwrap());
        let nice = (u32_of(16) as i32).clamp(-20, 19);
        let prio = u32_of(20);
        let (util_min, util_max) = (u32_of(48), u32_of(52));
        if flags & !FLAGS_ALL != 0 {
            return Err(-EINVAL);
        }
        let t = self.sched_thread(cur, pid)?;
        let old = self.threads[t].sched;
        let policy = if flags & FLAG_KEEP_POLICY != 0 { old.policy } else { policy };
        if policy == SCHED_DEADLINE {
            return Err(-EPERM);
        }
        if flags & (FLAG_UTIL_CLAMP_MIN | FLAG_UTIL_CLAMP_MAX) != 0 {
            let (lo, hi) = (
                if flags & FLAG_UTIL_CLAMP_MIN != 0 { util_min } else { 0 },
                if flags & FLAG_UTIL_CLAMP_MAX != 0 { util_max } else { 1024 },
            );
            if lo > hi || hi > 1024 {
                return Err(-EINVAL);
            }
        }
        let prio = if flags & FLAG_KEEP_PARAMS != 0 && is_rt(policy) == is_rt(old.policy) {
            old.prio
        } else {
            prio
        };
        let reset = flags & FLAG_RESET_ON_FORK != 0;
        match self.set_policy(t, policy, prio, reset) {
            0 => {},
            e => return Err(e),
        }
        if flags & FLAG_KEEP_PARAMS == 0 && (policy == SCHED_OTHER || policy == SCHED_BATCH) {
            self.threads[t].sched.nice = nice;
        }
        Ok(0)
    }

    fn sched_getattr(&mut self, cur: usize, a: &[u64; 6]) -> Result<i64, i64> {
        let pid = a[0] as i32;
        let usize_ = a[2] as u32;
        if a[1] == 0 || pid < 0 || a[3] != 0 || (usize_ as usize) < ATTR_VER0 || usize_ > PAGE {
            return Err(-EINVAL);
        }
        let s = self.threads[self.sched_thread(cur, pid)?].sched;
        let size = (usize_ as usize).min(ATTR_SIZE);
        let mut k = [0u8; ATTR_SIZE];
        k[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        k[4..8].copy_from_slice(&s.policy.to_le_bytes());
        k[8..16].copy_from_slice(&(s.reset_on_fork as u64 * FLAG_RESET_ON_FORK).to_le_bytes());
        if is_rt(s.policy) {
            k[20..24].copy_from_slice(&s.prio.to_le_bytes());
        } else {
            k[16..20].copy_from_slice(&s.nice.to_le_bytes());
        }
        // The utilization clamps nobody set
        k[52..56].copy_from_slice(&1024u32.to_le_bytes());
        if !self.write(a[1], &k[..size]) {
            return Err(-EFAULT);
        }
        Ok(0)
    }
}
