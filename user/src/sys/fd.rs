//! The descriptor table: new descriptors, close, dup, fcntl, pipes and eventfds.

use crate::errno::{EBADF, EFAULT, EINVAL, EMFILE};
use crate::files::{self, Kind, O_APPEND, O_ASYNC, O_CLOEXEC, O_NONBLOCK, O_WRONLY, PIPE_BUF_MAX};
use crate::{Process, Ret};


impl Process {
    /// A descriptor for description `d` (the lowest free at or above `min`), or -EMFILE when the
    /// table is full: then `d` goes, unless a descriptor still refers to it.
    pub(crate) fn new_fd(&mut self, d: u32, min: i32, cloexec: bool) -> i64 {
        match self.files.install(d, min, cloexec) {
            Ok(fd) => fd as i64,
            Err(e) => {
                self.files.hold(d);
                self.release_desc(d);
                e
            },
        }
    }

    pub(crate) fn close_fd(&mut self, fd: i32) -> bool {
        // A process's record locks on a file go when it closes any descriptor of it
        if let Some(Kind::File { ino, .. }) = self.files.get(fd).map(|d| &d.kind) {
            let ino = *ino;
            self.files.locks.closed(ino, self.p.pid);
        }
        match self.files.close(fd) {
            None => false,
            Some(ino) => {
                if let Some(ino) = ino {
                    self.fs(|fs| fs.drop_ref(ino));
                }
                self.poke();
                true
            },
        }
    }

    /// close_range(first, last, flags): CLOSE_RANGE_CLOEXEC (4) marks them instead;
    /// CLOSE_RANGE_UNSHARE (2) asks for a table of its own, which a process has here
    pub(super) fn sys_close_range(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let (first, last) = (a[0] as u32, a[1] as u32);
        if first > last || a[2] as u32 & !(2 | 4) != 0 {
            return v(-EINVAL);
        }
        let top = |fd: u32| fd.min(i32::MAX as u32) as i32;
        let fds: Vec<i32> = self
            .files
            .fds
            .range(top(first)..=top(last))
            .map(|(&f, _)| f)
            .collect();
        for fd in fds {
            if a[2] & 4 != 0 {
                self.files.fds.get_mut(&fd).unwrap().1 = true;
            } else {
                self.close_fd(fd);
            }
        }
        v(0)
    }

    /// dup
    pub(super) fn sys_dup(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.files.desc_of(fd0) {
            Some(d) => v(self.new_fd(d, 0, false)),
            None => v(-EBADF),
        }
    }

    /// dup3(old, new, flags), checked in Linux's order: its flags (O_CLOEXEC only), the
    /// same descriptor twice, then a new one past the table (unsigned: a negative one too)
    pub(super) fn sys_dup3(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let new = a[1] as u32;
        if a[2] as u32 & !O_CLOEXEC != 0 || a[0] as u32 == new {
            return v(-EINVAL);
        }
        if new as u64 >= files::NOFILE {
            return v(-EBADF);
        }
        match self.files.desc_of(fd0) {
            Some(d) => {
                self.close_fd(new as i32);
                self.files.put(new as i32, d, a[2] as u32 & O_CLOEXEC != 0);
                v(new as i64)
            },
            None => v(-EBADF),
        }
    }

    /// fcntl
    pub(super) fn sys_fcntl(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let Some(d) = self.files.desc_of(fd0) else {
            return v(-EBADF);
        };
        match a[1] {
            // F_DUPFD, F_DUPFD_CLOEXEC: from an unsigned lowest descriptor in the table
            0 | 1030 if a[2] >= files::NOFILE => v(-EINVAL),
            0 | 1030 => v(self.new_fd(d, a[2] as i32, a[1] == 1030)),
            1 => v(self.files.fds[&fd0].1 as i64),
            2 => {
                self.files.fds.get_mut(&fd0).unwrap().1 = a[2] & 1 != 0;
                v(0)
            },
            3 => v(self.files.descs[&d].flags as i64),
            4 => {
                let desc = self.files.descs.get_mut(&d).unwrap();
                desc.flags = (desc.flags & !(O_APPEND | O_NONBLOCK | O_ASYNC))
                    | (a[2] as u32 & (O_APPEND | O_NONBLOCK | O_ASYNC));
                v(0)
            },
            // F_GETLK, F_SETLK, F_SETLKW and their OFD forms (locks.rs)
            5..=7 | 36..=38 => self.fcntl_lock(fd0, a[1], a[2]),
            // F_SETOWN, F_SETSIG: who gets SIGIO, which isn't sent; F_GETOWN, F_GETSIG
            8 | 10 => v(0),
            9 | 11 => v(0),
            // F_SETPIPE_SZ, F_GETPIPE_SZ
            1031 | 1032 => v(PIPE_BUF_MAX as i64),
            _ => v(-EINVAL),
        }
    }

    /// pipe2
    pub(super) fn sys_pipe2(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let p = self.files.new_pipe();
        let flags = a[1] as u32;
        let nb = flags & O_NONBLOCK;
        let r = self.files.add(Kind::PipeRead(p), nb);
        let w = self.files.add(Kind::PipeWrite(p), O_WRONLY | nb);
        let cloexec = flags & O_CLOEXEC != 0;
        // Both ends or neither
        let rf = self.new_fd(r, 0, cloexec);
        let wf = self.new_fd(w, 0, cloexec);
        if rf < 0 || wf < 0 {
            if rf >= 0 {
                self.close_fd(rf as i32);
            }
            return v(-EMFILE);
        }
        v(
            if self.put_u32(a[0], rf as u32) && self.put_u32(a[0].wrapping_add(4), wf as u32) {
                0
            } else {
                -EFAULT
            },
        )
    }

    /// eventfd2
    pub(super) fn sys_eventfd2(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let d = self
            .files
            .add(Kind::Event { count: a[0] }, 2 | (a[1] as u32 & O_NONBLOCK));
        v(self.new_fd(d, 0, a[1] as u32 & O_CLOEXEC != 0))
    }
}
