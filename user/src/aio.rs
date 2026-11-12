//! Linux native AIO (io_setup, io_submit, io_getevents and the rest), done synchronously: a
//! submitted request runs as it is submitted and its completion waits in the context's queue for
//! io_getevents. Also the read and write calls that take RWF_* flags (preadv2, pwritev2), which
//! requests with a flags field share.
//!
//! A context is named by the address of one zeroed page of the program's memory (a program that
//! looks for the ring's header, as libaio does, finds no magic number and asks the kernel). A
//! context belongs to the process that made it and ends with its address space; a child doesn't
//! inherit it.

use std::collections::VecDeque;

use crate::files::{Kind, EBADF, EINVAL};
use crate::host::Instant;
use crate::{Process, Ret, PAGE, RETRY_AT_DEADLINE};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EOPNOTSUPP: i64 = 95;

/// aio-max-nr
const AIO_MAX_NR: u64 = 65536;

const RWF_APPEND: u64 = 0x10;
const RWF_NOWAIT: u64 = 8;
/// A write goes where it is told, whatever the description's O_APPEND says
const RWF_NOAPPEND: u64 = 0x20;
/// HIPRI, DSYNC, SYNC, NOWAIT, APPEND, NOAPPEND, DONTCACHE (RWF_ATOMIC, 0x40, is for file
/// systems that can write atomically: none of this one's can)
const RWF_SUPPORTED: u64 = 0x1f | RWF_NOAPPEND | 0x80;

const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
const IOCB_FLAG_RESFD: u32 = 1;
const IOCB_FLAG_IOPRIO: u32 = 2;

/// One AIO context: how many completions it holds at most, and those not yet reaped.
pub(crate) struct Aio {
    pub cap: u64,
    /// io_event: data, obj, res, res2
    pub events: VecDeque<[u64; 4]>,
}

impl Process {
    /// Reads into or writes from the program's `iov` (address, length pairs) on `fd`, at `at` or
    /// at the descriptor's position, with the RWF_* `flags`: the bytes moved, or what the call
    /// must do instead.
    pub(crate) fn rw_iov(
        &mut self,
        fd: i32,
        write: bool,
        iov: &[(u64, u64)],
        at: Option<u64>,
        flags: u64,
    ) -> Result<usize, Ret> {
        let v = |e: i64| Err(Ret::Val(e));
        if flags & !RWF_SUPPORTED != 0 || !write && flags & RWF_APPEND != 0 {
            return v(-EOPNOTSUPP);
        }
        let mut at = at;
        let r = if write {
            let mut all = Vec::new();
            for &(base, len) in iov {
                let Some(b) = self.bytes_at(base, len as usize) else {
                    return v(-EFAULT);
                };
                all.extend_from_slice(&b);
            }
            // At the descriptor's position and not the end, though it appends: the position is
            // given to the write, and moved on by what it wrote
            let mut moves = None;
            if flags & RWF_NOAPPEND != 0 {
                if let (None, Some(Kind::File { pos, .. })) =
                    (at, self.files.get(fd).map(|d| d.kind.clone()))
                {
                    at = Some(pos);
                    moves = Some(pos);
                }
            } else if flags & RWF_APPEND != 0 {
                if let Some(Kind::File { ino, .. }) = self.files.get(fd).map(|d| d.kind.clone()) {
                    let size = self.fs(|fs| fs.get(ino).map_or(0, |i| i.size)).unwrap_or(0);
                    match at {
                        Some(_) => at = Some(size),
                        None => {
                            if let Some(crate::files::Desc {
                                kind: Kind::File { pos, .. },
                                ..
                            }) = self.files.get_mut(fd)
                            {
                                *pos = size;
                            }
                        },
                    }
                }
            }
            let r = self.write_fd(fd, &all, at);
            if let (Some(from), Ok(n), Some(Kind::File { pos, .. })) =
                (moves, &r, self.files.get_mut(fd).map(|d| &mut d.kind))
            {
                *pos = from + *n as u64;
            }
            r
        } else {
            let total: u64 = iov.iter().map(|&(_, l)| l).sum();
            self.read_fd(fd, total as usize, at).map(|b| {
                let mut off = 0;
                for &(base, len) in iov {
                    let n = (len as usize).min(b.len() - off);
                    self.write(base, &b[off..off + n]);
                    off += n;
                }
                b.len()
            })
        };
        match r {
            // RWF_NOWAIT: a call that would wait for something to happen says so (a file's
            // blobs still come)
            Err(Ret::Retry { .. }) if flags & RWF_NOWAIT != 0 => v(-EAGAIN),
            r => r,
        }
    }

    /// preadv2 and pwritev2: `a` is (fd, iov, iovcnt, pos_l, pos_h, flags); a position of -1 is
    /// the descriptor's own.
    pub(crate) fn sys_rwv2(&mut self, write: bool, a: &[u64; 6]) -> Ret {
        let Some(iov) = self.iovecs(a[1], a[2]) else {
            return Ret::Val(-EFAULT);
        };
        let at = match a[3] as i64 {
            -1 => None,
            p if p < 0 => return Ret::Val(-EINVAL),
            p => Some(p as u64),
        };
        self.rw_iov(a[0] as i32, write, &iov, at, a[5])
            .map_or_else(|r| r, |n| Ret::Val(n as i64))
    }

    // ------------------------------------------------------------ contexts

    pub(crate) fn sys_io_setup(&mut self, nr: u64, ctxp: u64) -> i64 {
        match self.u64_at(ctxp) {
            None => return -EFAULT,
            Some(0) => {},
            Some(_) => return -EINVAL,
        }
        let nr = nr as u32;
        if nr == 0 || nr > i32::MAX as u32 {
            return -EINVAL;
        }
        if nr as u64 > AIO_MAX_NR {
            return -EAGAIN;
        }
        let Some(id) = self.alloc_map(PAGE, 3) else {
            return -EAGAIN;
        };
        if !self.put_u64(ctxp, id) {
            self.sys_munmap(id, PAGE);
            return -EFAULT;
        }
        self.p.aio.insert(
            id,
            Aio {
                cap: (nr as u64).max(4),
                events: VecDeque::new(),
            },
        );
        0
    }

    pub(crate) fn sys_io_destroy(&mut self, id: u64) -> i64 {
        if self.p.aio.remove(&id).is_none() {
            return -EINVAL;
        }
        self.sys_munmap(id, PAGE);
        // Whoever waits in io_getevents on it finds it gone
        self.poke();
        0
    }

    pub(crate) fn sys_io_cancel(&mut self, id: u64, iocb: u64) -> i64 {
        if self.u64_at(iocb + 8).is_none() {
            return -EFAULT;
        }
        // A request is done when it is submitted: none is left to cancel
        let _ = id;
        -EINVAL
    }

    pub(crate) fn sys_io_submit(&mut self, id: u64, nr: i64, iocbpp: u64) -> Ret {
        if nr < 0 || !self.p.aio.contains_key(&id) {
            return Ret::Val(-EINVAL);
        }
        let nr = (nr as u64).min(self.p.aio[&id].cap);
        for i in 0..nr {
            let r = match self.u64_at(iocbpp + 8 * i) {
                None => Err(Ret::Val(-EFAULT)),
                Some(iocb) => self.aio_one(id, iocb),
            };
            if let Err(r) = r {
                // What it did before this one stands
                return if i > 0 { Ret::Val(i as i64) } else { r };
            }
        }
        Ret::Val(nr as i64)
    }

    /// Runs the request at `iocb` and queues its completion. An error is for io_submit to
    /// return: what is wrong with the request, or the wait for what it needs.
    fn aio_one(&mut self, id: u64, iocb: u64) -> Result<(), Ret> {
        let v = |e: i64| Err(Ret::Val(e));
        let Some(b) = self.bytes_at(iocb, 64) else {
            return v(-EFAULT);
        };
        let w = |o: usize, n: usize| {
            let mut x = [0u8; 8];
            x[..n].copy_from_slice(&b[o..o + n]);
            u64::from_le_bytes(x)
        };
        let (data, rw_flags, op) = (w(0, 8), w(12, 4), w(16, 2) as u16);
        let (fd, buf, nbytes, off) = (w(20, 4) as i32, w(24, 8), w(32, 8), w(40, 8));
        let (reserved, flags, resfd) = (w(48, 8), w(56, 4) as u32, w(60, 4) as i32);
        if reserved != 0 || flags & !(IOCB_FLAG_RESFD | IOCB_FLAG_IOPRIO) != 0 {
            return v(-EINVAL);
        }
        if flags & IOCB_FLAG_RESFD != 0 {
            match self.files.get(resfd).map(|d| &d.kind) {
                None => return v(-EBADF),
                Some(Kind::Event { .. }) => {},
                Some(_) => return v(-EINVAL),
            }
        }
        if self.files.get(fd).is_none() {
            return v(-EBADF);
        }
        if self.p.aio[&id].events.len() as u64 >= self.p.aio[&id].cap {
            return v(-EAGAIN);
        }
        let (write, vectored) = match op {
            IOCB_CMD_PREAD => (false, false),
            IOCB_CMD_PWRITE => (true, false),
            IOCB_CMD_PREADV => (false, true),
            IOCB_CMD_PWRITEV => (true, true),
            IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => {
                if buf != 0 || nbytes != 0 || off != 0 {
                    return v(-EINVAL);
                }
                return self.aio_done(id, [data, iocb, 0, 0], flags, resfd);
            },
            _ => return v(-EINVAL),
        };
        if (off as i64) < 0 {
            return v(-EINVAL);
        }
        let iov = if vectored {
            match self.iovecs(buf, nbytes) {
                Some(i) => i,
                None => return v(-EFAULT),
            }
        } else {
            vec![(buf, nbytes)]
        };
        let res = match self.rw_iov(fd, write, &iov, Some(off), rw_flags) {
            Ok(n) => n as i64,
            Err(Ret::Val(e)) => e,
            Err(r) => return Err(r),
        };
        self.aio_done(id, [data, iocb, res as u64, 0], flags, resfd)
    }

    fn aio_done(&mut self, id: u64, ev: [u64; 4], flags: u32, resfd: i32) -> Result<(), Ret> {
        self.p.aio.get_mut(&id).unwrap().events.push_back(ev);
        if flags & IOCB_FLAG_RESFD != 0 {
            let _ = self.write_fd(resfd, &1u64.to_le_bytes(), None);
        }
        self.poke();
        Ok(())
    }

    /// io_getevents and io_pgetevents (whose signal mask is not applied): at least `min_nr` and
    /// at most `nr` completions into `events`, waiting for them up to `timeout` (a timespec, or
    /// none to wait as long as it takes); when it passes, the completions there are (up to `nr`).
    pub(crate) fn sys_io_getevents(
        &mut self,
        id: u64,
        min_nr: i64,
        nr: i64,
        events: u64,
        timeout: u64,
    ) -> Ret {
        let Some(ctx) = self.p.aio.get(&id) else {
            return Ret::Val(-EINVAL);
        };
        if min_nr < 0 || nr < min_nr {
            return Ret::Val(-EINVAL);
        }
        let take = (ctx.events.len() as u64).min(nr as u64);
        // A zero timeout, or the wait that has run out: what is there is the answer
        let zero = (timeout != 0 && self.timespec(timeout).is_some_and(|d| d.is_zero()))
            || self.resume_deadline.is_some_and(|d| d <= Instant::now());
        if take >= min_nr as u64 || zero {
            if take == 0 {
                return Ret::Val(0);
            }
            let mut out = Vec::new();
            for e in ctx.events.iter().take(take as usize) {
                e.iter()
                    .for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
            }
            if !self.write(events, &out) {
                return Ret::Val(-EFAULT);
            }
            self.p
                .aio
                .get_mut(&id)
                .unwrap()
                .events
                .drain(..take as usize);
            self.poke();
            return Ret::Val(take as i64);
        }
        let deadline = self.resume_deadline.or_else(|| {
            (timeout != 0)
                .then(|| self.timespec(timeout).map(|d| Instant::now() + d))
                .flatten()
        });
        Ret::Retry {
            deadline,
            on_timeout: RETRY_AT_DEADLINE,
        }
    }

    // ------------------------------------------------------------ groups

    pub(crate) fn sys_getgroups(&mut self, size: i64, list: u64) -> i64 {
        let n = self.p.groups.len() as i64;
        if size == 0 {
            return n;
        }
        if size < n {
            return -EINVAL;
        }
        let b: Vec<u8> = self.p.groups.iter().flat_map(|g| g.to_le_bytes()).collect();
        if self.write(list, &b) {
            n
        } else {
            -EFAULT
        }
    }

    /// The program is root: any list is allowed (up to NGROUPS_MAX).
    pub(crate) fn sys_setgroups(&mut self, size: u64, list: u64) -> i64 {
        if size > 65536 {
            return -EINVAL;
        }
        let Some(b) = self.bytes_at(list, 4 * size as usize) else {
            return -EFAULT;
        };
        self.p.groups = (0..size as usize)
            .map(|i| u32::from_le_bytes(b[4 * i..4 * i + 4].try_into().unwrap()))
            .collect();
        0
    }
}
