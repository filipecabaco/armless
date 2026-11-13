//! Futexes, the CPUs, time, limits and what the system says of itself.

use crate::errno::{EAGAIN, EFAULT, EINVAL, ENOSYS, EPERM};
use crate::files;
use crate::host::{self, Instant};
use crate::{Process, Ret, Wait};

use super::NCPUS_REPORTED;


impl Process {
    /// The CPUs a program is told it has (`NCPUS_REPORTED`)
    pub(crate) fn ncpus_reported(&self) -> u64 {
        NCPUS_REPORTED.max(self.on_cpu.len() as u64)
    }

    /// FUTEX_WAKE_OP: applies the operation `enc` (the kernel's encoded `op`, `oparg`, `cmp` and
    /// `cmparg`) to the word at `addr2`, wakes up to `nwake` threads waiting on `addr`, and, if
    /// the old value of the word satisfies the comparison, up to `nwake2` waiting on `addr2`.
    pub(super) fn futex_wake_op(
        &mut self,
        addr: u64,
        nwake: u64,
        addr2: u64,
        nwake2: u64,
        enc: u32,
        private: bool,
    ) -> i64 {
        let sign = |v: u32| ((v << 20) as i32) >> 20;
        let (mut op, cmp) = ((enc >> 28) & 0xf, (enc >> 24) & 0xf);
        let (mut oparg, cmparg) = (sign((enc >> 12) & 0xfff), sign(enc & 0xfff));
        if op & 8 != 0 {
            op &= 7;
            oparg = 1i32.wrapping_shl(oparg as u32 & 31);
        }
        let Some(old) = self.u32_at(addr2) else {
            return -EFAULT;
        };
        let old = old as i32;
        let new = match op {
            0 => oparg,
            1 => old.wrapping_add(oparg),
            2 => old | oparg,
            3 => old & !oparg,
            4 => old ^ oparg,
            _ => return -ENOSYS,
        };
        let hit = match cmp {
            0 => old == cmparg,
            1 => old != cmparg,
            2 => old < cmparg,
            3 => old <= cmparg,
            4 => old > cmparg,
            5 => old >= cmparg,
            _ => return -ENOSYS,
        };
        if !self.put_u32(addr2, new as u32) {
            return -EFAULT;
        }
        let (key, key2) = (self.futex_key(addr, private), self.futex_key(addr2, private));
        let mut woken = self.futex_wake(key, nwake, u32::MAX);
        if hit {
            woken += self.futex_wake(key2, nwake2, u32::MAX);
        }
        woken
    }

    /// futex
    pub(super) fn sys_futex(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let (addr, op, val) = (a[0], a[1] as u32, a[2]);
        let realtime = op & 256 != 0;
        // FUTEX_PRIVATE_FLAG: the process's own; without it, a futex in shared memory is
        // the one every process mapping it meets at (futex_key)
        let private = op & 128 != 0;
        match op & 0x7f {
            0 | 9 => {
                // The timeout first, as Linux reads it: relative for FUTEX_WAIT, on a
                // clock for FUTEX_WAIT_BITSET (too far to say: none)
                let deadline = match (a[3], self.timespec(a[3])) {
                    (0, _) => None,
                    (_, Err(e)) => return v(e),
                    (_, Ok(d)) if op & 0x7f == 0 => Instant::now().checked_add(d),
                    (_, Ok(t)) => Some(self.deadline_at(if realtime { 0 } else { 1 }, t)),
                };
                let Some(cur_val) = self.u32_at(addr) else {
                    return v(-EFAULT);
                };
                if cur_val != val as u32 {
                    return v(-EAGAIN);
                }
                let mask = if op & 0x7f == 9 { a[5] as u32 } else { u32::MAX };
                let addr = self.futex_key(addr, private);
                Ret::Block(Wait::Futex { addr, mask }, deadline)
            },
            1 => v(self.futex_wake(self.futex_key(addr, private), val, u32::MAX)),
            10 => v(self.futex_wake(self.futex_key(addr, private), val, a[5] as u32)),
            // FUTEX_REQUEUE and FUTEX_CMP_REQUEUE (a[5] is the value to compare with):
            // musl's condition variables broadcast with them.
            3 | 4 => {
                if op & 0x7f == 4 && self.u32_at(addr) != Some(a[5] as u32) {
                    return v(-EAGAIN);
                }
                let (key, key2) =
                    (self.futex_key(addr, private), self.futex_key(a[4], private));
                let (woken, moved) = self.futex_requeue(key, val, key2, a[3]);
                v((woken + moved) as i64)
            },
            5 => v(self.futex_wake_op(addr, val, a[4], a[3], a[5] as u32, private)),
            _ => v(-ENOSYS),
        }
    }

    /// sched_getaffinity
    pub(super) fn sys_sched_getaffinity(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        if a[1] < 8 {
            return v(-EINVAL);
        }
        let mask = (1u64 << self.ncpus_reported()) - 1;
        v(if self.put_u64(a[2], mask) { 8 } else { -EFAULT })
    }

    /// getcpu
    pub(super) fn sys_getcpu(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        // (The CPU this thread runs on; one node)
        for (at, n) in [(a[0], self.cpu as u32), (a[1], 0)] {
            if at != 0 {
                self.put_u32(at, n);
            }
        }
        v(0)
    }

    /// capget: no capabilities
    pub(super) fn sys_capget(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        if a[1] != 0 {
            self.write(a[1], &[0u8; 24]);
        }
        v(0)
    }

    pub(super) fn sys_clock_gettime(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let ns = self.now_ns(a[0]);
        let ok = self.put_u64(a[1], ns / 1_000_000_000)
            && self.put_u64(a[1].wrapping_add(8), ns % 1_000_000_000);
        v(if ok { 0 } else { -EFAULT })
    }

    /// gettimeofday: V8 reads its Date.now() with it, and keeps what was there when it fails
    pub(super) fn sys_gettimeofday(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let ns = self.now_ns(0);
        let ok = (a[0] == 0
            || self.put_u64(a[0], ns / 1_000_000_000)
                && self.put_u64(a[0].wrapping_add(8), ns % 1_000_000_000 / 1000))
            && (a[1] == 0 || self.put_u64(a[1], 0));
        v(if ok { 0 } else { -EFAULT })
    }

    pub(super) fn sys_clock_getres(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        if a[1] != 0 {
            self.put_u64(a[1], 0);
            self.put_u64(a[1].wrapping_add(8), 1);
        }
        v(0)
    }

    /// nanosleep, clock_nanosleep (TIMER_ABSTIME: until the clock reads it); a sleep too
    /// long to say sleeps until a signal
    pub(super) fn sys_nanosleep(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.timespec(a[0]) {
            Ok(d) => Ret::Block(Wait::Sleep, Instant::now().checked_add(d)),
            Err(e) => v(e),
        }
    }

    pub(super) fn sys_clock_nanosleep(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.timespec(a[2]) {
            Ok(d) if a[1] & 1 != 0 => {
                Ret::Block(Wait::Sleep, Some(self.deadline_at(a[0], d)))
            },
            Ok(d) => Ret::Block(Wait::Sleep, Instant::now().checked_add(d)),
            Err(e) => v(e),
        }
    }

    /// getrandom(buf, count, flags): GRND_NONBLOCK, GRND_RANDOM and GRND_INSECURE ask
    /// for what the host's source gives anyway
    pub(super) fn sys_getrandom(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        if a[2] as u32 & !7 != 0 {
            return v(-EINVAL);
        }
        let bytes = host::random_bytes(self.rw_count(a[1]));
        v(if self.write(a[0], &bytes) { bytes.len() as i64 } else { -EFAULT })
    }

    /// prlimit64(pid, resource, new, old), getrlimit(resource, old), setrlimit(resource,
    /// new). A new limit isn't kept, but one past the descriptors a process may have is
    /// refused (EPERM, as for a process that may not raise it), for what getrlimit says to
    /// be what holds.
    pub(super) fn sys_rlimit(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let (res, new, old) = match nr {
            261 => (a[1], a[2], a[3]),
            163 => (a[0], 0, a[1]),
            _ => (a[0], a[1], 0),
        };
        // RLIM_NLIMITS
        if res >= 16 {
            return v(-EINVAL);
        }
        if nr == 163 && old == 0 || nr == 164 && new == 0 {
            return v(-EFAULT);
        }
        if new != 0 {
            let (Some(c), Some(m)) = (self.u64_at(new), self.u64_at(new.wrapping_add(8))) else {
                return v(-EFAULT);
            };
            if c > m {
                return v(-EINVAL);
            }
            if res == 7 && m > files::NOFILE {
                return v(-EPERM);
            }
        }
        if old != 0 {
            let (c, m) = match res {
                3 => (crate::STACK_SIZE, u64::MAX),
                7 => (files::NOFILE, files::NOFILE),
                _ => (u64::MAX, u64::MAX),
            };
            if !(self.put_u64(old, c) && self.put_u64(old.wrapping_add(8), m)) {
                return v(-EFAULT);
            }
        }
        v(0)
    }

    /// getrusage: the time it has run, as user time
    pub(super) fn sys_getrusage(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let mut b = [0u8; 144];
        let el = self.start.elapsed();
        b[0..8].copy_from_slice(&el.as_secs().to_le_bytes());
        b[8..16].copy_from_slice(&(el.subsec_micros() as u64).to_le_bytes());
        v(if self.write(a[1], &b) { 0 } else { -EFAULT })
    }

    /// sysinfo
    pub(super) fn sys_sysinfo(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let mut b = [0u8; 112];
        b[0..8].copy_from_slice(&self.start.elapsed().as_secs().to_le_bytes());
        b[32..40].copy_from_slice(&self.ram_size.to_le_bytes());
        b[40..48].copy_from_slice(&(self.ram_size / 2).to_le_bytes());
        b[80..82].copy_from_slice(&(self.processes() as u16).to_le_bytes());
        b[104..108].copy_from_slice(&1u32.to_le_bytes());
        v(if self.write(a[0], &b) { 0 } else { -EFAULT })
    }

    /// uname
    pub(super) fn sys_uname(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let mut u = [0u8; 65 * 6];
        for (i, s) in [
            "Linux",
            "armless",
            "6.6.0-armless-user",
            "#1",
            "aarch64",
            "",
        ]
        .iter()
        .enumerate()
        {
            u[65 * i..65 * i + s.len()].copy_from_slice(s.as_bytes());
        }
        v(if self.write(a[0], &u) { 0 } else { -EFAULT })
    }
}
