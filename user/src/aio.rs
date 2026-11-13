//! Linux's native AIO (io_setup, io_submit, io_getevents and the rest), done synchronously: a
//! request runs as it is submitted (as preadv2 and pwritev2 run, with its RWF_ flags) and its
//! completion waits in the context's queue for io_getevents. With IOCB_FLAG_RESFD a completion
//! also adds 1 to an eventfd.
//!
//! A context is named by the address of one zeroed page of the program's memory: a program that
//! looks there for the ring's header, as libaio does, finds no magic number and asks the kernel.
//! A context belongs to the process that made it and goes with its address space: a child
//! doesn't inherit it and exec drops it. Not done: IOCB_CMD_POLL, and io_pgetevents's signal mask.

use std::collections::{BTreeMap, VecDeque};

use crate::files::{Kind, EBADF, EINVAL};
use crate::host::Instant;
use crate::ser::{R, W};
use crate::{Process, Ret, PAGE, RETRY_AT_DEADLINE};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;

/// aio-max-nr
const AIO_MAX_NR: u64 = 65536;

const IOCB_CMD_PREAD: u16 = 0;
const IOCB_CMD_PWRITE: u16 = 1;
const IOCB_CMD_FSYNC: u16 = 2;
const IOCB_CMD_FDSYNC: u16 = 3;
const IOCB_CMD_PREADV: u16 = 7;
const IOCB_CMD_PWRITEV: u16 = 8;
const IOCB_FLAG_RESFD: u32 = 1;
const IOCB_FLAG_IOPRIO: u32 = 2;

/// One context: how many completions it holds at most, and those not yet reaped (io_event:
/// data, obj, res, res2).
pub(crate) struct Aio {
    pub cap: u64,
    pub events: VecDeque<[u64; 4]>,
}

impl Process {
    /// io_setup(nr_events, ctxp)
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
        let cap = (nr as u64).max(4);
        self.p.aio.insert(
            id,
            Aio {
                cap,
                events: VecDeque::new(),
            },
        );
        0
    }

    /// io_destroy(ctx)
    pub(crate) fn sys_io_destroy(&mut self, id: u64) -> i64 {
        if self.p.aio.remove(&id).is_none() {
            return -EINVAL;
        }
        self.sys_munmap(id, PAGE);
        // Whoever waits in io_getevents on it finds it gone
        self.poke();
        0
    }

    /// io_cancel(ctx, iocb, result): a request is done when it is submitted, so none is left
    pub(crate) fn sys_io_cancel(&mut self, id: u64, iocb: u64) -> i64 {
        if !self.p.aio.contains_key(&id) {
            return -EINVAL;
        }
        if self.u64_at(iocb).is_none() {
            return -EFAULT;
        }
        -EINVAL
    }

    /// io_submit(ctx, nr, iocbpp): how many it took (those before a bad one stand)
    pub(crate) fn sys_io_submit(&mut self, id: u64, nr: i64, iocbpp: u64) -> Ret {
        let Some(ctx) = self.p.aio.get(&id) else {
            return Ret::Val(-EINVAL);
        };
        if nr < 0 {
            return Ret::Val(-EINVAL);
        }
        let nr = (nr as u64).min(ctx.cap);
        for i in 0..nr {
            let r = match self.u64_at(iocbpp + 8 * i) {
                None => Err(Ret::Val(-EFAULT)),
                Some(iocb) => self.aio_one(id, iocb),
            };
            if let Err(r) = r {
                return if i > 0 { Ret::Val(i as i64) } else { r };
            }
        }
        Ret::Val(nr as i64)
    }

    /// Runs the request at `iocb` and queues its completion. An error is io_submit's: what is
    /// wrong with the request, or the wait for what it needs.
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
            // The filesystem is in memory: nothing to sync
            IOCB_CMD_FSYNC | IOCB_CMD_FDSYNC => {
                if buf != 0 || nbytes != 0 || off != 0 || rw_flags != 0 {
                    return v(-EINVAL);
                }
                self.aio_done(id, [data, iocb, 0, 0], flags, resfd);
                return Ok(());
            },
            _ => return v(-EINVAL),
        };
        if (off as i64) < 0 {
            return v(-EINVAL);
        }
        let iov = if vectored {
            self.iovecs(buf, nbytes).ok_or(Ret::Val(-EFAULT))?
        } else {
            vec![(buf, nbytes)]
        };
        let res = match self.rw_iov(fd, write, &iov, Some(off), rw_flags) {
            Ret::Val(r) => r,
            r => return Err(r),
        };
        self.aio_done(id, [data, iocb, res as u64, 0], flags, resfd);
        Ok(())
    }

    fn aio_done(&mut self, id: u64, ev: [u64; 4], flags: u32, resfd: i32) {
        if let Some(c) = self.p.aio.get_mut(&id) {
            c.events.push_back(ev);
        }
        if flags & IOCB_FLAG_RESFD != 0 {
            let _ = self.write_fd_as(resfd, &1u64.to_le_bytes(), None, None);
        }
        self.poke();
    }

    /// io_getevents and io_pgetevents (ctx, min_nr, nr, events, timeout): at least `min_nr` and
    /// at most `nr` completions into `events`, waiting for them up to `timeout` (none: as long as
    /// it takes). When the time is up, the ones there are.
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
        let take = (ctx.events.len() as u64).min(nr as u64) as usize;
        let wait = match (self.resume_deadline, timeout) {
            (Some(d), _) => Some(Some(d)),
            (None, 0) => Some(None),
            (None, t) => match self.timespec(t) {
                Some(d) if d.is_zero() => None,
                Some(d) => Some(Some(Instant::now() + d)),
                None => return Ret::Val(-EFAULT),
            },
        };
        // Enough, or a wait that is over (no time to wait, or its deadline passed)
        let over = wait.is_none_or(|d| d.is_some_and(|d| d <= Instant::now()));
        if take as i64 >= min_nr || over {
            let out: Vec<u8> = ctx
                .events
                .iter()
                .take(take)
                .flatten()
                .flat_map(|x| x.to_le_bytes())
                .collect();
            if !self.write(events, &out) {
                return Ret::Val(-EFAULT);
            }
            if let Some(c) = self.p.aio.get_mut(&id) {
                c.events.drain(..take);
            }
            return Ret::Val(take as i64);
        }
        Ret::Retry {
            deadline: wait.flatten(),
            on_timeout: RETRY_AT_DEADLINE,
        }
    }
}

/// A process's contexts, in its state
pub(crate) fn save(aio: &BTreeMap<u64, Aio>, w: &mut W) {
    w.u(aio.len() as u64);
    for (id, c) in aio {
        w.u(*id);
        w.u(c.cap);
        w.u(c.events.len() as u64);
        c.events.iter().flatten().for_each(|&x| w.u(x));
    }
}

pub(crate) fn load(r: &mut R) -> Result<BTreeMap<u64, Aio>, String> {
    let mut aio = BTreeMap::new();
    for _ in 0..r.u()? {
        let (id, cap) = (r.u()?, r.u()?);
        let events = (0..r.u()?)
            .map(|_| Ok([r.u()?, r.u()?, r.u()?, r.u()?]))
            .collect::<Result<_, String>>()?;
        aio.insert(id, Aio { cap, events });
    }
    Ok(aio)
}
