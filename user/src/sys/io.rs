//! Reading and writing: read, write and their vectored and positioned forms, sendfile, lseek.

use std::time::Duration;

use crate::errno::{EAGAIN, EBADF, EFAULT, EINVAL, EISDIR, EOPNOTSUPP, EPIPE, ESPIPE};
use crate::files::{got, Desc, Got, Kind, O_ACCMODE, O_APPEND, O_NONBLOCK, O_WRONLY, PIPE_BUF_MAX};
use crate::host::{self, Instant};
use crate::{Process, Ret};


impl Process {
    pub(crate) fn nonblock(&self, fd: i32) -> bool {
        self.files
            .get(fd)
            .is_some_and(|d| d.flags & O_NONBLOCK != 0)
    }

    /// Up to `max` bytes from `fd` (at `at`, for pread), or what the call must do instead.
    pub(super) fn read_fd(&mut self, fd: i32, max: usize, at: Option<u64>) -> Result<Vec<u8>, Ret> {
        let nonblock = self.nonblock(fd);
        let Some(d) = self.files.get(fd).cloned() else {
            return Err(Ret::Val(-EBADF));
        };
        if d.flags & O_ACCMODE == O_WRONLY {
            return Err(Ret::Val(-EBADF));
        }
        match d.kind {
            Kind::Tty => match self.tty.read(max) {
                Some(b) => Ok(b),
                None if nonblock => Err(Ret::Val(-EAGAIN)),
                None => {
                    let deadline = self.resume_deadline.or_else(|| {
                        self.tty
                            .read_timeout_ms()
                            .and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)))
                    });
                    Err(Ret::Retry {
                        deadline,
                        on_timeout: 0,
                    })
                },
            },
            Kind::Null => Ok(Vec::new()),
            Kind::Zero => Ok(vec![0; max]),
            Kind::Random => Ok(host::random_bytes(max)),
            Kind::Dir { .. } => Err(Ret::Val(-EISDIR)),
            Kind::Mem { data, pos } => {
                let start = at.unwrap_or(pos).min(data.len() as u64) as usize;
                let out = data[start..start.saturating_add(max).min(data.len())].to_vec();
                if at.is_none() {
                    if let Some(Desc {
                        kind: Kind::Mem { pos, .. },
                        ..
                    }) = self.files.get_mut(fd)
                    {
                        *pos += out.len() as u64;
                    }
                }
                Ok(out)
            },
            Kind::File { ino, pos } => {
                let off = at.unwrap_or(pos);
                match self.fs(|fs| got(fs.read(ino, off, max))) {
                    Some(Got::Ok(b)) => {
                        if at.is_none() {
                            if let Some(Desc {
                                kind: Kind::File { pos, .. },
                                ..
                            }) = self.files.get_mut(fd)
                            {
                                *pos += b.len() as u64;
                            }
                        }
                        Ok(b)
                    },
                    Some(Got::Need(blobs)) => Err(Ret::Need(blobs)),
                    Some(Got::Err(e)) => Err(Ret::Val(e)),
                    None => Err(Ret::Val(-EBADF)),
                }
            },
            Kind::PipeRead(p) => {
                let Some(pipe) = self.files.pipes.get_mut(&p) else {
                    return Ok(Vec::new());
                };
                if pipe.buf.is_empty() {
                    return if pipe.writers == 0 {
                        Ok(Vec::new())
                    } else if nonblock {
                        Err(Ret::Val(-EAGAIN))
                    } else {
                        Err(Ret::Retry {
                            deadline: None,
                            on_timeout: 0,
                        })
                    };
                }
                let n = pipe.buf.len().min(max);
                let out = pipe.buf.drain(..n).collect();
                self.poke();
                Ok(out)
            },
            Kind::Signal { mask } => self.read_signalfd(mask, max, nonblock),
            Kind::Pidfd { .. } => Err(Ret::Val(-EINVAL)),
            Kind::TimerFd(_) => self.read_timerfd(fd, max, nonblock),
            Kind::Event { count } => {
                if max < 8 {
                    return Err(Ret::Val(-EINVAL));
                }
                if count == 0 {
                    return Err(if nonblock {
                        Ret::Val(-EAGAIN)
                    } else {
                        Ret::Retry {
                            deadline: None,
                            on_timeout: 0,
                        }
                    });
                }
                if let Some(Desc {
                    kind: Kind::Event { count },
                    ..
                }) = self.files.get_mut(fd)
                {
                    *count = 0;
                }
                self.poke();
                Ok(count.to_le_bytes().to_vec())
            },
            Kind::Sock(id) => {
                if at.is_some() {
                    return Err(Ret::Val(-ESPIPE));
                }
                let got = self.sock_recv(fd, id, max, 0).map(|r| r.0);
                self.drop_taken();
                got
            },
            Kind::PipeWrite(_) | Kind::Epoll { .. } => Err(Ret::Val(-EINVAL)),
        }
    }

    /// Writes `data` to `fd` (at `at`, for pwrite): how much it took, or what the call must do.
    pub(super) fn write_fd(&mut self, fd: i32, data: &[u8], at: Option<u64>) -> Result<usize, Ret> {
        self.write_fd_as(fd, data, at, None)
    }

    /// `write_fd`, appending or not as `append` says (RWF_APPEND, RWF_NOAPPEND) whatever the
    /// description's O_APPEND. A write that appends goes to the end of a file even at a position
    /// (pwrite), as on Linux.
    pub(crate) fn write_fd_as(
        &mut self,
        fd: i32,
        data: &[u8],
        at: Option<u64>,
        append: Option<bool>,
    ) -> Result<usize, Ret> {
        let nonblock = self.nonblock(fd);
        let Some(d) = self.files.get(fd).cloned() else {
            return Err(Ret::Val(-EBADF));
        };
        if d.flags & O_ACCMODE == 0 && !matches!(d.kind, Kind::Tty | Kind::Event { .. }) {
            return Err(Ret::Val(-EBADF));
        }
        match d.kind {
            Kind::Tty => {
                let shown = self.tty.output(data);
                self.output.extend_from_slice(&shown);
                Ok(data.len())
            },
            Kind::Null | Kind::Zero | Kind::Random => Ok(data.len()),
            Kind::Dir { .. } => Err(Ret::Val(-EISDIR)),
            Kind::Mem { .. } => Err(Ret::Val(-EBADF)),
            Kind::File { ino, pos } => {
                let now = self.now_secs();
                let append = append.unwrap_or(d.flags & O_APPEND != 0);
                let r = self.fs(|fs| {
                    let off = if append {
                        fs.get(ino).map_or(0, |i| i.size)
                    } else {
                        at.unwrap_or(pos)
                    };
                    got(fs.write(ino, off, data, now)).map_ok(|n| (n, off))
                });
                match r {
                    Some(Got::Ok((n, off))) => {
                        if at.is_none() {
                            if let Some(Desc {
                                kind: Kind::File { pos, .. },
                                ..
                            }) = self.files.get_mut(fd)
                            {
                                *pos = off + n as u64;
                            }
                        }
                        Ok(n)
                    },
                    Some(Got::Need(b)) => Err(Ret::Need(b)),
                    Some(Got::Err(e)) => Err(Ret::Val(e)),
                    None => Err(Ret::Val(-EBADF)),
                }
            },
            Kind::PipeWrite(p) => {
                let Some(pipe) = self.files.pipes.get_mut(&p).filter(|p| p.readers > 0) else {
                    return Err(self.broken_pipe());
                };
                let room = PIPE_BUF_MAX.saturating_sub(pipe.buf.len());
                if room == 0 {
                    return Err(if nonblock {
                        Ret::Val(-EAGAIN)
                    } else {
                        Ret::Retry {
                            deadline: None,
                            on_timeout: 0,
                        }
                    });
                }
                let n = room.min(data.len());
                pipe.buf.extend(&data[..n]);
                self.poke();
                Ok(n)
            },
            Kind::Signal { .. } => Err(Ret::Val(-EINVAL)),
            Kind::Pidfd { .. } | Kind::TimerFd(_) => Err(Ret::Val(-EINVAL)),
            Kind::Event { .. } => {
                if data.len() < 8 {
                    return Err(Ret::Val(-EINVAL));
                }
                let add = u64::from_le_bytes(data[..8].try_into().unwrap());
                if let Some(Desc {
                    kind: Kind::Event { count },
                    ..
                }) = self.files.get_mut(fd)
                {
                    *count += add;
                }
                self.poke();
                Ok(8)
            },
            Kind::Sock(id) => {
                if at.is_some() {
                    return Err(Ret::Val(-ESPIPE));
                }
                self.sock_send(fd, id, data, 0, None, &Default::default())
            },
            Kind::PipeRead(_) | Kind::Epoll { .. } => Err(Ret::Val(-EBADF)),
        }
    }

    /// A write to a pipe or stream nobody reads: EPIPE, and SIGPIPE for the thread that wrote,
    /// as Linux sends it (which ends a program that neither handles nor ignores it).
    pub(crate) fn broken_pipe(&mut self) -> Ret {
        let (cur, pid) = (self.cur, self.p.pid);
        self.raise_in(pid, crate::signal::SIGPIPE, Some(cur), crate::signal::SI_USER);
        Ret::Val(-EPIPE)
    }

    /// Reads into or writes from the program's `iov` (address, length pairs) on `fd`, at `at` or
    /// at the descriptor's position, with preadv2's and pwritev2's RWF_ `flags` (AIO's requests
    /// have them too). HIPRI, DSYNC, SYNC and DONTCACHE ask for what happens anyway (the
    /// filesystem is in memory); NOWAIT makes a call that would wait EAGAIN; APPEND and NOAPPEND
    /// override O_APPEND. ATOMIC is for filesystems that write atomically: none here does.
    pub(crate) fn rw_iov(
        &mut self,
        fd: i32,
        write: bool,
        iov: &[(u64, u64)],
        at: Option<u64>,
        flags: u64,
    ) -> Ret {
        const RWF_NOWAIT: u64 = 0x8;
        const RWF_APPEND: u64 = 0x10;
        const RWF_NOAPPEND: u64 = 0x20;
        const RWF_DONTCACHE: u64 = 0x80;
        if flags & !(0x1f | RWF_NOAPPEND | RWF_DONTCACHE) != 0 {
            return Ret::Val(-EOPNOTSUPP);
        }
        if flags & (RWF_APPEND | RWF_NOAPPEND) == RWF_APPEND | RWF_NOAPPEND {
            return Ret::Val(-EINVAL);
        }
        let r = if write {
            let mut all = Vec::new();
            for &(base, len) in iov {
                let Some(b) = self.bytes_at(base, len as usize) else {
                    return Ret::Val(-EFAULT);
                };
                all.extend_from_slice(&b);
            }
            let append = match flags & (RWF_APPEND | RWF_NOAPPEND) {
                RWF_APPEND => Some(true),
                RWF_NOAPPEND => Some(false),
                _ => None,
            };
            self.write_fd_as(fd, &all, at, append)
        } else {
            // (`iovecs` keeps the sum within a read's count)
            let total: u64 = iov.iter().map(|&(_, l)| l).sum();
            self.read_fd(fd, total as usize, at).and_then(|b| {
                // What was stored, up to a buffer that isn't there: what the read gives, or
                // EFAULT if not a byte was
                let mut off = 0;
                for &(base, len) in iov {
                    let n = (len as usize).min(b.len() - off);
                    if !self.write(base, &b[off..off + n]) {
                        break;
                    }
                    off += n;
                }
                if off == 0 && !b.is_empty() {
                    Err(Ret::Val(-EFAULT))
                } else {
                    Ok(off)
                }
            })
        };
        match r {
            Ok(n) => Ret::Val(n as i64),
            // (A file's blobs still come: a disk read)
            Err(Ret::Retry { .. }) if flags & RWF_NOWAIT != 0 => Ret::Val(-EAGAIN),
            Err(r) => r,
        }
    }

    /// The (base, len) pairs of an iovec array, as Linux takes them: EINVAL for more than
    /// UIO_MAXIOV or a length a ssize_t can't hold, and the lengths cut to what one call moves
    /// (`rw_count`), so that their sum never overflows.
    pub(crate) fn iovecs(&self, at: u64, n: u64) -> Result<Vec<(u64, u64)>, i64> {
        if n > crate::UIO_MAXIOV {
            return Err(-EINVAL);
        }
        let mut left = self.rw_count(u64::MAX) as u64;
        let mut iov = Vec::with_capacity(n as usize);
        for i in 0..n {
            let (Some(base), Some(len)) = (self.u64_at(at.wrapping_add(16 * i)), self.u64_at(at.wrapping_add(16 * i + 8))) else {
                return Err(-EFAULT);
            };
            if (len as i64) < 0 {
                return Err(-EINVAL);
            }
            let len = len.min(left);
            left -= len;
            iov.push((base, len));
        }
        Ok(iov)
    }

    /// read
    pub(super) fn sys_read(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.read_fd(fd0, self.rw_count(a[2]), None) {
            Ok(b) => v(if self.write(a[1], &b) { b.len() as i64 } else { -EFAULT }),
            Err(r) => r,
        }
    }

    /// pread64
    pub(super) fn sys_pread64(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.read_fd(fd0, self.rw_count(a[2]), Some(a[3])) {
            Ok(b) => v(if self.write(a[1], &b) { b.len() as i64 } else { -EFAULT }),
            Err(r) => r,
        }
    }

    /// readv, preadv, preadv2 (whose offset -1 is the current position). Postgres 18 reads
    /// and writes with them.
    pub(super) fn sys_readv(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let at = match nr {
            69 => Some(a[3]),
            286 => (a[3] != u64::MAX).then_some(a[3]),
            _ => None,
        };
        let iov = match self.iovecs(a[1], a[2]) {
            Ok(iov) => iov,
            Err(e) => return v(e),
        };
        self.rw_iov(fd0, false, &iov, at, if nr == 286 { a[5] } else { 0 })
    }

    /// write
    pub(super) fn sys_write(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.bytes_at(a[1], self.rw_count(a[2])) {
            None => v(-EFAULT),
            Some(b) => self
                .write_fd(fd0, &b, None)
                .map_or_else(|r| r, |n| v(n as i64)),
        }
    }

    /// pwrite64
    pub(super) fn sys_pwrite64(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.bytes_at(a[1], self.rw_count(a[2])) {
            None => v(-EFAULT),
            Some(b) => self
                .write_fd(fd0, &b, Some(a[3]))
                .map_or_else(|r| r, |n| v(n as i64)),
        }
    }

    /// writev, pwritev, pwritev2 (as preadv2)
    pub(super) fn sys_writev(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let at = match nr {
            70 => Some(a[3]),
            287 => (a[3] != u64::MAX).then_some(a[3]),
            _ => None,
        };
        let iov = match self.iovecs(a[1], a[2]) {
            Ok(iov) => iov,
            Err(e) => return v(e),
        };
        self.rw_iov(fd0, true, &iov, at, if nr == 287 { a[5] } else { 0 })
    }

    /// sendfile(out, in, offset*, count)
    pub(super) fn sys_sendfile(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let off = (a[2] != 0).then(|| self.u64_at(a[2])).flatten();
        match self.read_fd(a[1] as i32, a[3].min(1 << 20) as usize, off) {
            Ok(b) => match self.write_fd(fd0, &b, None) {
                Ok(n) => {
                    if let Some(o) = off {
                        self.put_u64(a[2], o + n as u64);
                    }
                    v(n as i64)
                },
                Err(r) => r,
            },
            Err(r) => r,
        }
    }

    /// lseek
    pub(super) fn sys_lseek(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let Some(d) = self.files.get(fd0).cloned() else {
            return v(-EBADF);
        };
        let (off, whence) = (a[1] as i64, a[2]);
        match d.kind {
            Kind::File { ino, pos } => {
                let size =
                    self.fs(|fs| fs.get(ino).map_or(0, |i| i.size)).unwrap_or(0) as i64;
                let new = match whence {
                    0 => off,
                    1 => pos as i64 + off,
                    2 => size + off,
                    _ => return v(-EINVAL),
                };
                if new < 0 {
                    return v(-EINVAL);
                }
                if let Some(Desc {
                    kind: Kind::File { pos, .. },
                    ..
                }) = self.files.get_mut(fd0)
                {
                    *pos = new as u64;
                }
                v(new)
            },
            Kind::Dir { .. } | Kind::Mem { .. } if whence == 0 && off >= 0 => {
                if let Some(Desc {
                    kind: Kind::Dir { pos, .. } | Kind::Mem { pos, .. },
                    ..
                }) = self.files.get_mut(fd0)
                {
                    *pos = off as u64;
                }
                v(off)
            },
            Kind::Null | Kind::Zero | Kind::Random => v(0),
            _ => v(-ESPIPE),
        }
    }
}
