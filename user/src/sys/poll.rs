//! Waiting on descriptors: what each is ready for, ppoll, pselect6 and epoll.

use std::time::Duration;

use crate::errno::{EBADF, EFAULT, EINVAL};
use crate::files::{Desc, Kind, O_CLOEXEC, PIPE_BUF_MAX};
use crate::host::Instant;
use crate::{Process, Ret};

use super::Ready;


impl Process {
    pub(super) fn ready(&self, fd: i32) -> Option<Ready> {
        let d = self.files.get(fd)?;
        Some(match &d.kind {
            Kind::Tty => (self.tty.readable(), true, self.tty.closed),
            Kind::PipeRead(p) => {
                let p = self.files.pipes.get(p);
                (
                    p.is_none_or(|p| !p.buf.is_empty() || p.writers == 0),
                    false,
                    p.is_none_or(|p| p.writers == 0),
                )
            },
            Kind::PipeWrite(p) => {
                let p = self.files.pipes.get(p);
                (
                    false,
                    p.is_none_or(|p| p.buf.len() < PIPE_BUF_MAX || p.readers == 0),
                    p.is_none_or(|p| p.readers == 0),
                )
            },
            Kind::Event { count } => (*count > 0, true, false),
            Kind::Signal { mask } => (self.signal_pending(*mask), false, false),
            // Readable once its process has ended; hung up too once it was waited for
            Kind::Pidfd { pid } => (
                self.proc_ref(*pid).is_none_or(|p| p.zombie.is_some()),
                false,
                self.proc_ref(*pid).is_none(),
            ),
            Kind::TimerFd(t) => (t.readable(Instant::now()), false, false),
            Kind::Sock(id) => self.sock_ready(*id),
            Kind::Epoll { watch } => (
                watch.iter().any(|(&f, w)| {
                    self.ready(f)
                        .is_some_and(|r| r.0 && w.events & 1 != 0 || r.1 && w.events & 4 != 0)
                }),
                false,
                false,
            ),
            _ => (true, true, false),
        })
    }

    /// ppoll: struct pollfd { int fd; short events; short revents }
    pub(super) fn ppoll(&mut self, fds: u64, n: u64, timeout: u64) -> Ret {
        let mut ready = 0;
        // The array in one read, and back in one write (not two reads and a write a descriptor)
        let n = n.min(4096) as usize;
        let mut pfds = if n == 0 {
            Vec::new()
        } else {
            match self.bytes_at(fds, 8 * n) {
                Some(b) => b,
                None => return Ret::Val(-EFAULT),
            }
        };
        for pfd in pfds.as_chunks_mut::<8>().0 {
            let fd = i32::from_le_bytes(pfd[..4].try_into().unwrap());
            let events = u16::from_le_bytes(pfd[4..6].try_into().unwrap());
            let revents: u16 = if fd < 0 {
                0
            } else {
                match self.ready(fd) {
                    None => 0x20, // POLLNVAL
                    Some((r, w, hup)) => {
                        let mut e = 0;
                        if r {
                            e |= events & 1;
                        }
                        if w {
                            e |= events & 4;
                        }
                        if hup {
                            e |= 0x10;
                        }
                        e
                    },
                }
            };
            pfd[6..].copy_from_slice(&revents.to_le_bytes());
            if revents != 0 {
                ready += 1;
            }
        }
        if n > 0 {
            self.write(fds, &pfds);
        }
        if ready > 0 {
            return Ret::Val(ready);
        }
        let deadline = match (self.resume_deadline, timeout) {
            (Some(d), _) => Some(d),
            (None, 0) => None,
            // (A timeout too long to say is none)
            (None, t) => match self.timespec(t) {
                Ok(d) if d.is_zero() => return Ret::Val(0),
                Ok(d) => Instant::now().checked_add(d),
                Err(e) => return Ret::Val(e),
            },
        };
        Ret::Retry {
            deadline,
            on_timeout: 0,
        }
    }

    /// pselect6: fd_set bitmaps of n descriptors, and a timespec
    pub(super) fn pselect(&mut self, n: u64, rset: u64, wset: u64, eset: u64, timeout: u64) -> Ret {
        let wait = match timeout {
            0 => None,
            t => match self.timespec(t) {
                Ok(d) => Some(d),
                Err(e) => return Ret::Val(e),
            },
        };
        let words = n.min(1024).div_ceil(64);
        let load = |p: &Self, at: u64| -> Vec<u64> {
            if at == 0 {
                vec![0; words as usize]
            } else {
                (0..words)
                    .map(|i| p.u64_at(at.wrapping_add(8 * i)).unwrap_or(0))
                    .collect()
            }
        };
        let (r, w) = (load(self, rset), load(self, wset));
        let (mut ro, mut wo) = (vec![0u64; words as usize], vec![0u64; words as usize]);
        let mut count = 0;
        for fd in 0..n.min(1024) {
            let (i, bit) = ((fd / 64) as usize, 1u64 << (fd % 64));
            if r[i] & bit == 0 && w[i] & bit == 0 {
                continue;
            }
            let Some((rd, wr, hup)) = self.ready(fd as i32) else {
                return Ret::Val(-EBADF);
            };
            if r[i] & bit != 0 && (rd || hup) {
                ro[i] |= bit;
                count += 1;
            }
            if w[i] & bit != 0 && wr {
                wo[i] |= bit;
                count += 1;
            }
        }
        if count > 0 || wait.is_some_and(|d| d.is_zero()) {
            for (at, set) in [
                (rset, &ro),
                (wset, &wo),
                (eset, &vec![0u64; words as usize]),
            ] {
                if at != 0 {
                    for (i, v) in set.iter().enumerate() {
                        self.put_u64(at.wrapping_add(8 * i as u64), *v);
                    }
                }
            }
            return Ret::Val(count);
        }
        let deadline = self
            .resume_deadline
            .or_else(|| wait.and_then(|d| Instant::now().checked_add(d)));
        // At the deadline: no descriptor ready, the sets cleared
        for at in [rset, wset, eset] {
            if at != 0 && deadline.is_some_and(|d| d <= Instant::now()) {
                for i in 0..words {
                    self.put_u64(at.wrapping_add(8 * i), 0);
                }
            }
        }
        Ret::Retry {
            deadline,
            on_timeout: 0,
        }
    }

    /// The events an epoll instance reports now, at most `max` (`take`: as epoll_wait does, which
    /// marks edge-triggered watches as reported and disarms one-shot ones; else only a look).
    pub(super) fn epoll_ready(&mut self, ep: i32, max: usize, take: bool) -> Vec<(u32, u64)> {
        const EPOLLONESHOT: u32 = 1 << 30;
        const EPOLLET: u32 = 1 << 31;
        let Some(Desc {
            kind: Kind::Epoll { watch },
            ..
        }) = self.files.get(ep)
        else {
            return Vec::new();
        };
        let epoch = self.epoch;
        let mut now: Vec<(i32, u32)> = Vec::new();
        for (&fd, w) in watch {
            let Some((r, wr, hup)) = self.ready(fd) else {
                continue;
            };
            let mut got = 0;
            if r {
                got |= w.events & 1; // EPOLLIN
            }
            if wr {
                got |= w.events & 4; // EPOLLOUT
            }
            if hup {
                got |= 0x10 | (w.events & 0x2000); // EPOLLHUP, EPOLLRDHUP
            }
            now.push((fd, got));
        }
        let Some(Desc {
            kind: Kind::Epoll { watch },
            ..
        }) = self.files.get_mut(ep)
        else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        for (fd, got) in now {
            let w = watch.get_mut(&fd).unwrap();
            // Edge-triggered: what it became ready for since it was last reported, and more input
            // in a descriptor that was never drained (a write to an eventfd that nobody reads is
            // an edge each time, as the kernel counts every wakeup of the file)
            let report = if w.events & EPOLLET != 0 {
                got & !w.last | if w.epoch != epoch { got & 1 } else { 0 }
            } else {
                got
            };
            if take {
                // What it is no longer ready for may be reported again once it is
                w.last &= got;
                w.epoch = epoch;
            }
            if report == 0 || w.events & 0x3fff == 0 || ready.len() >= max {
                continue;
            }
            if take {
                w.last |= report;
                if w.events & EPOLLONESHOT != 0 {
                    w.events &= !0x3fff;
                }
            }
            ready.push((report, w.data));
        }
        ready
    }

    /// epoll_create1
    pub(super) fn sys_epoll_create1(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let d = self.files.add(
            Kind::Epoll {
                watch: Default::default(),
            },
            2,
        );
        v(self.new_fd(d, 0, a[0] as u32 & O_CLOEXEC != 0))
    }

    /// epoll_ctl
    pub(super) fn sys_epoll_ctl(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let (ev, data) = (
            self.u32_at(a[3]).unwrap_or(0),
            self.u64_at(a[3].wrapping_add(8)).unwrap_or(0),
        );
        if self.files.get(a[2] as i32).is_none() {
            return v(-EBADF);
        }
        let Some(Desc {
            kind: Kind::Epoll { watch },
            ..
        }) = self.files.get_mut(fd0)
        else {
            return v(-EBADF);
        };
        match a[1] {
            1 | 3 => {
                watch.insert(
                    a[2] as i32,
                    crate::files::Watch {
                        events: ev,
                        data,
                        last: 0,
                        epoch: self.epoch,
                    },
                );
            },
            2 => {
                watch.remove(&(a[2] as i32));
            },
            _ => return v(-EINVAL),
        }
        v(0)
    }

    /// epoll_pwait, epoll_pwait2
    pub(super) fn sys_epoll_pwait(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let ready = self.epoll_ready(fd0, a[2] as usize, true);
        if !ready.is_empty() {
            let n = ready.len();
            for (i, &(ev, data)) in ready[..n].iter().enumerate() {
                // struct epoll_event on arm64: u32 events, 4 bytes of padding, u64 data
                let at = a[1] + 16 * i as u64;
                self.put_u32(at, ev);
                self.put_u64(at.wrapping_add(8), data);
            }
            return v(n as i64);
        }
        let deadline = if nr == 441 {
            if a[3] == 0 {
                None
            } else {
                match self.timespec(a[3]) {
                    Ok(d) => Instant::now().checked_add(d),
                    Err(e) => return v(e),
                }
            }
        } else {
            let ms = a[3] as i32;
            if ms == 0 {
                return v(0);
            }
            (ms > 0)
                .then(|| Instant::now().checked_add(Duration::from_millis(ms as u64)))
                .flatten()
        };
        Ret::Retry {
            deadline: self.resume_deadline.or(deadline),
            on_timeout: 0,
        }
    }
}
