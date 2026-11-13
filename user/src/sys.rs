//! The system calls (arm64 numbers), as Linux answers them.

use std::time::Duration;

use armless::cpu::Cpu;
use armless::dev::fs::{Fs, Ino, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use armless::Host;

use crate::files::{self, device, got, Desc, Got, Kind, Stat, PIPE_BUF_MAX, S_IFIFO};
use crate::files::{EACCES, EBADF, EEXIST, EINVAL, EISDIR, ENOENT, ENOTDIR};
use crate::files::{
    O_ACCMODE, O_APPEND, O_ASYNC, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_NOFOLLOW, O_NONBLOCK,
    O_TRUNC, O_WRONLY,
};
use crate::host::{self, Instant};
use crate::proc::CloneArgs;
use crate::{page_up, Process, Ret, Wait};

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const EPIPE: i64 = 32;
const ERANGE: i64 = 34;
const ENOSYS: i64 = 38;

const AT_FDCWD: i32 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_EMPTY_PATH: u64 = 0x1000;

const NCPUS_REPORTED: u64 = 4;

/// What a descriptor is ready for: (readable, writable, hung up)
type Ready = (bool, bool, bool);

impl Process {
    /// FUTEX_WAKE_OP: applies the operation `enc` (the kernel's encoded `op`, `oparg`, `cmp` and
    /// `cmparg`) to the word at `addr2`, wakes up to `nwake` threads waiting on `addr`, and, if
    /// the old value of the word satisfies the comparison, up to `nwake2` waiting on `addr2`.
    fn futex_wake_op(
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

    // ------------------------------------------------------------ descriptors

    fn ready(&self, fd: i32) -> Option<Ready> {
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
            Kind::Pidfd { pid } => (
                self.proc_ref(*pid).is_none_or(|p| p.zombie.is_some()),
                false,
                false,
            ),
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

    pub(crate) fn nonblock(&self, fd: i32) -> bool {
        self.files
            .get(fd)
            .is_some_and(|d| d.flags & O_NONBLOCK != 0)
    }

    /// Up to `max` bytes from `fd` (at `at`, for pread), or what the call must do instead.
    fn read_fd(&mut self, fd: i32, max: usize, at: Option<u64>) -> Result<Vec<u8>, Ret> {
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
                            .map(|ms| Instant::now() + Duration::from_millis(ms))
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
                let out = data[start..(start + max).min(data.len())].to_vec();
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
                self.sock_recv(fd, id, max, 0).map(|r| r.0)
            },
            Kind::PipeWrite(_) | Kind::Epoll { .. } => Err(Ret::Val(-EINVAL)),
        }
    }

    /// Writes `data` to `fd` (at `at`, for pwrite): how much it took, or what the call must do.
    fn write_fd(&mut self, fd: i32, data: &[u8], at: Option<u64>) -> Result<usize, Ret> {
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
                let append = d.flags & O_APPEND != 0;
                let r = self.fs(|fs| {
                    let off =
                        at.unwrap_or(if append { fs.get(ino).map_or(0, |i| i.size) } else { pos });
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
                let Some(pipe) = self.files.pipes.get_mut(&p) else {
                    return Err(Ret::Val(-EPIPE));
                };
                if pipe.readers == 0 {
                    return Err(Ret::Val(-EPIPE));
                }
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
            Kind::Pidfd { .. } => Err(Ret::Val(-EINVAL)),
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
                self.sock_send(fd, id, data, 0, None, &[])
            },
            Kind::PipeRead(_) | Kind::Epoll { .. } => Err(Ret::Val(-EBADF)),
        }
    }

    /// The (base, len) pairs of an iovec array
    pub(crate) fn iovecs(&self, at: u64, n: u64) -> Option<Vec<(u64, u64)>> {
        (0..n.min(1024))
            .map(|i| Some((self.u64_at(at + 16 * i)?, self.u64_at(at + 16 * i + 8)?)))
            .collect()
    }

    pub(crate) fn close_fd(&mut self, fd: i32) -> bool {
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

    // ------------------------------------------------------------ paths

    /// Where a path given with `dirfd` starts: the cwd, or that directory.
    pub(crate) fn start_dir(&self, dirfd: i32) -> Result<Ino, i64> {
        if dirfd == AT_FDCWD {
            return Ok(self.p.cwd);
        }
        match self.files.get(dirfd).map(|d| &d.kind) {
            Some(Kind::Dir { ino, .. }) => Ok(*ino),
            Some(Kind::File { .. }) => Err(-ENOTDIR),
            _ => Err(-EBADF),
        }
    }

    /// The inode `path` names from `dirfd`
    pub(crate) fn lookup(&self, dirfd: i32, path: &[u8], follow: bool) -> Result<Ino, i64> {
        let start = self.start_dir(dirfd)?;
        self.fs(|fs| files::resolve(fs, start, path, follow))
            .unwrap_or(Err(-ENOENT))
    }

    /// Contents made here for the /proc files programs read
    fn proc_file(&self, path: &[u8]) -> Option<Vec<u8>> {
        Some(match path {
            b"/proc/cpuinfo" => (0..NCPUS_REPORTED)
                .map(|i| format!("processor\t: {i}\nBogoMIPS\t: 100.00\nFeatures\t: fp asimd aes pmull sha1 sha2 crc32 atomics sha3 sm3 sm4 sha512\nCPU implementer\t: 0x41\nCPU architecture: 8\n\n"))
                .collect::<String>()
                .into_bytes(),
            b"/proc/meminfo" => {
                let kb = self.ram_size / 1024;
                format!("MemTotal:       {kb} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n", kb / 2, kb / 2).into_bytes()
            },
            b"/proc/self/stat" | b"/proc/thread-self/stat" => {
                let (pid, ppid, pgid, sid) = (self.p.pid, self.p.ppid, self.p.pgid, self.p.sid);
                format!("{pid} (program) R {ppid} {pgid} {sid} 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0\n").into_bytes()
            },
            b"/proc/self/cmdline" => self.p.exe.iter().copied().chain([0]).collect(),
            b"/proc/self/mounts" | b"/proc/mounts" => b"host9p / 9p rw 0 0\nproc /proc proc rw 0 0\n".to_vec(),
            b"/proc/sys/kernel/osrelease" => b"6.6.0-armless-user\n".to_vec(),
            b"/sys/fs/cgroup/cpu.max" => b"max 100000\n".to_vec(),
            _ => return None,
        })
    }

    /// Where a symlink in the filesystem sends `path` when its target is a device or a /proc file
    fn special_target(&self, start: Ino, path: &[u8]) -> Option<Vec<u8>> {
        self.fs(|fs| {
            files::link_to_special(fs, start, path, |p| {
                device(p).is_some() || self.proc_file(p).is_some()
            })
        })
        .flatten()
    }

    fn openat(&mut self, dirfd: i32, path: &[u8], flags: u32, mode: u32) -> Ret {
        let cloexec = flags & O_CLOEXEC != 0;
        let status = flags & (O_ACCMODE | O_APPEND | O_NONBLOCK);
        let absolute = path.first() == Some(&b'/');
        if absolute {
            if let Some((kind, _, _)) = device(path) {
                let d = self.files.add(kind, status);
                return Ret::Val(self.files.install(d, 0, cloexec) as i64);
            }
            if let Some(data) = self.proc_file(path) {
                let d = self.files.add(Kind::Mem { data, pos: 0 }, status);
                return Ret::Val(self.files.install(d, 0, cloexec) as i64);
            }
            if path.starts_with(b"/proc/")
                || path.starts_with(b"/sys/")
                || path.starts_with(b"/dev/")
                    && !self
                        .fs(|fs| fs.lookup_path(&String::from_utf8_lossy(path)).is_some())
                        .unwrap_or(false)
            {
                return Ret::Val(-ENOENT);
            }
        }
        let start = match self.start_dir(dirfd) {
            Ok(s) => s,
            Err(e) => return Ret::Val(e),
        };
        if flags & O_NOFOLLOW == 0 {
            if let Some(target) = self.special_target(start, path) {
                return self.openat(AT_FDCWD, &target, flags & !(O_CREAT | O_EXCL), mode);
            }
        }
        let (now, umask) = (self.now_secs(), self.p.umask);
        let (uid, gid) = (self.p.creds.euid(), self.p.creds.egid());
        let opened = self.fs(|fs| -> Result<Ino, i64> {
            let ino = if flags & O_CREAT != 0 {
                let (dir, name) = files::resolve_parent(fs, start, path)?;
                match fs.lookup(dir, &name) {
                    Ok(_) if flags & O_EXCL != 0 => return Err(-EEXIST),
                    Ok(_) => files::resolve(fs, start, path, flags & O_NOFOLLOW == 0)?,
                    Err(_) => fs
                        .create(dir, &name, S_IFREG | (mode & !umask & 0o7777), uid, gid, now)
                        .map_err(|_| -EACCES)?,
                }
            } else {
                files::resolve(fs, start, path, flags & O_NOFOLLOW == 0)?
            };
            let inode = fs.get(ino).ok_or(-ENOENT)?;
            let kind = inode.mode & S_IFMT;
            if kind == S_IFLNK {
                return Err(-40); // ELOOP: O_NOFOLLOW on a symlink
            }
            if flags & O_DIRECTORY != 0 && kind != S_IFDIR {
                return Err(-ENOTDIR);
            }
            if kind == S_IFDIR && flags & O_ACCMODE != 0 {
                return Err(-EISDIR);
            }
            if flags & O_TRUNC != 0 && kind == S_IFREG && flags & O_ACCMODE != 0 {
                fs.truncate(ino, 0).map_err(|_| -EACCES)?;
            }
            fs.add_ref(ino);
            Ok(ino)
        });
        let ino = match opened {
            Some(Ok(ino)) => ino,
            Some(Err(e)) => return Ret::Val(e),
            None => return Ret::Val(-ENOENT),
        };
        let is_dir = self
            .fs(|fs| fs.get(ino).is_some_and(|i| i.is_dir()))
            .unwrap_or(false);
        let kind = if is_dir { Kind::Dir { ino, pos: 0 } } else { Kind::File { ino, pos: 0 } };
        let d = self.files.add(kind, status);
        self.stats
            .opens
            .push(String::from_utf8_lossy(path).into_owned());
        Ret::Val(self.files.install(d, 0, cloexec) as i64)
    }

    /// What stat says of `fd`
    fn stat_fd(&self, fd: i32) -> Result<Stat, i64> {
        let d = self.files.get(fd).ok_or(-EBADF)?;
        Ok(match &d.kind {
            Kind::Tty => Stat::device(files::S_IFCHR | 0o620, 0x8800),
            Kind::Null => Stat::device(files::S_IFCHR | 0o666, 0x103),
            Kind::Zero => Stat::device(files::S_IFCHR | 0o666, 0x105),
            Kind::Random => Stat::device(files::S_IFCHR | 0o666, 0x109),
            Kind::File { ino, .. } | Kind::Dir { ino, .. } => {
                self.fs(|fs| Stat::of(fs, *ino)).flatten().ok_or(-EBADF)?
            },
            Kind::Mem { data, .. } => Stat {
                size: data.len() as u64,
                ..Stat::device(S_IFREG | 0o444, 0)
            },
            Kind::PipeRead(_) | Kind::PipeWrite(_) => Stat::device(S_IFIFO | 0o600, 0),
            Kind::Sock(_) => Stat::device(files::S_IFSOCK | 0o777, 0),
            Kind::Event { .. } | Kind::Signal { .. } | Kind::Pidfd { .. } | Kind::Epoll { .. } => {
                Stat::device(0o600, 0)
            },
        })
    }

    /// What stat says of `path` from `dirfd` (flags: AT_SYMLINK_NOFOLLOW, AT_EMPTY_PATH)
    fn stat_path(&self, dirfd: i32, path: &[u8], flags: u64) -> Result<Stat, i64> {
        if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
            return self.stat_fd(dirfd);
        }
        if let Some((_, mode, rdev)) = device(path) {
            return Ok(Stat::device(mode, rdev));
        }
        if let Some(data) = self.proc_file(path) {
            return Ok(Stat {
                size: data.len() as u64,
                ..Stat::device(S_IFREG | 0o444, 0)
            });
        }
        if path == b"/proc" || path == b"/proc/self" || path == b"/dev" {
            return Ok(Stat::device(S_IFDIR | 0o555, 0));
        }
        if flags & AT_SYMLINK_NOFOLLOW == 0 {
            let start = self.start_dir(dirfd)?;
            if let Some(target) = self.special_target(start, path) {
                return self.stat_path(AT_FDCWD, &target, flags);
            }
        }
        let ino = self.lookup(dirfd, path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
        self.fs(|fs| Stat::of(fs, ino)).flatten().ok_or(-ENOENT)
    }

    // ------------------------------------------------------------ waiting on descriptors

    /// ppoll: struct pollfd { int fd; short events; short revents }
    fn ppoll(&mut self, fds: u64, n: u64, timeout: u64) -> Ret {
        let mut ready = 0;
        for i in 0..n.min(4096) {
            let at = fds + 8 * i;
            let Some(fd) = self.u32_at(at).map(|v| v as i32) else {
                return Ret::Val(-EFAULT);
            };
            let events = self.u32_at(at + 4).map_or(0, |v| v & 0xffff) as u16;
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
            self.write(at + 6, &revents.to_le_bytes());
            if revents != 0 {
                ready += 1;
            }
        }
        if ready > 0 {
            return Ret::Val(ready);
        }
        let deadline = match (self.resume_deadline, timeout) {
            (Some(d), _) => Some(d),
            (None, 0) => None,
            (None, t) => match self.timespec(t) {
                Some(d) if d.is_zero() => return Ret::Val(0),
                Some(d) => Some(Instant::now() + d),
                None => return Ret::Val(-EFAULT),
            },
        };
        Ret::Retry {
            deadline,
            on_timeout: 0,
        }
    }

    /// pselect6: fd_set bitmaps of n descriptors, and a timespec
    fn pselect(&mut self, n: u64, rset: u64, wset: u64, eset: u64, timeout: u64) -> Ret {
        let words = n.min(1024).div_ceil(64);
        let load = |p: &Self, at: u64| -> Vec<u64> {
            if at == 0 {
                vec![0; words as usize]
            } else {
                (0..words)
                    .map(|i| p.u64_at(at + 8 * i).unwrap_or(0))
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
        if count > 0 || timeout != 0 && self.timespec(timeout).is_some_and(|d| d.is_zero()) {
            for (at, set) in [
                (rset, &ro),
                (wset, &wo),
                (eset, &vec![0u64; words as usize]),
            ] {
                if at != 0 {
                    for (i, v) in set.iter().enumerate() {
                        self.put_u64(at + 8 * i as u64, *v);
                    }
                }
            }
            return Ret::Val(count);
        }
        let deadline = self.resume_deadline.or_else(|| {
            (timeout != 0)
                .then(|| self.timespec(timeout).map(|d| Instant::now() + d))
                .flatten()
        });
        // At the deadline: no descriptor ready, the sets cleared
        for at in [rset, wset, eset] {
            if at != 0 && deadline.is_some_and(|d| d <= Instant::now()) {
                for i in 0..words {
                    self.put_u64(at + 8 * i, 0);
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
    fn epoll_ready(&mut self, ep: i32, max: usize, take: bool) -> Vec<(u32, u64)> {
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

    // ------------------------------------------------------------ the syscalls

    pub(crate) fn syscall<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        let nr = cpu.x[8];
        let a = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5]];
        *self.stats.syscalls.entry(nr).or_default() += 1;
        let tid = self.threads[cur].tid;
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match nr {
            // ---------------------------------------------------- reading and writing
            // read
            63 => match self.read_fd(fd0, a[2] as usize, None) {
                Ok(b) => v(if self.write(a[1], &b) { b.len() as i64 } else { -EFAULT }),
                Err(r) => r,
            },
            // pread64
            67 => match self.read_fd(fd0, a[2] as usize, Some(a[3])) {
                Ok(b) => v(if self.write(a[1], &b) { b.len() as i64 } else { -EFAULT }),
                Err(r) => r,
            },
            // readv, preadv, preadv2 (whose offset -1 is the current position; its RWF_ flags only ask
            // for what happens anyway). Postgres 18 reads and writes with them.
            65 | 69 | 286 => {
                if nr == 286 && a[5] & !0x1f != 0 {
                    return v(-95); // EOPNOTSUPP
                }
                let at = match nr {
                    69 => Some(a[3]),
                    286 => (a[3] != u64::MAX).then_some(a[3]),
                    _ => None,
                };
                let Some(iov) = self.iovecs(a[1], a[2]) else {
                    return v(-EFAULT);
                };
                let total: u64 = iov.iter().map(|&(_, l)| l).sum();
                match self.read_fd(fd0, total as usize, at) {
                    Ok(b) => {
                        let mut off = 0;
                        for (base, len) in iov {
                            let n = (len as usize).min(b.len() - off);
                            self.write(base, &b[off..off + n]);
                            off += n;
                        }
                        v(b.len() as i64)
                    },
                    Err(r) => r,
                }
            },
            // write
            64 => match self.bytes_at(a[1], a[2] as usize) {
                None => v(-EFAULT),
                Some(b) => self
                    .write_fd(fd0, &b, None)
                    .map_or_else(|r| r, |n| v(n as i64)),
            },
            // pwrite64
            68 => match self.bytes_at(a[1], a[2] as usize) {
                None => v(-EFAULT),
                Some(b) => self
                    .write_fd(fd0, &b, Some(a[3]))
                    .map_or_else(|r| r, |n| v(n as i64)),
            },
            // writev, pwritev, pwritev2 (as preadv2)
            66 | 70 | 287 => {
                if nr == 287 && a[5] & !0x1f != 0 {
                    return v(-95); // EOPNOTSUPP
                }
                let at = match nr {
                    70 => Some(a[3]),
                    287 => (a[3] != u64::MAX).then_some(a[3]),
                    _ => None,
                };
                let Some(iov) = self.iovecs(a[1], a[2]) else {
                    return v(-EFAULT);
                };
                let mut all = Vec::new();
                for (base, len) in iov {
                    let Some(b) = self.bytes_at(base, len as usize) else {
                        return v(-EFAULT);
                    };
                    all.extend_from_slice(&b);
                }
                self.write_fd(fd0, &all, at)
                    .map_or_else(|r| r, |n| v(n as i64))
            },
            // sendfile(out, in, offset*, count)
            71 => {
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
            },
            // lseek
            62 => {
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
            },
            // close
            57 => v(if self.close_fd(fd0) { 0 } else { -EBADF }),
            // close_range(first, last, flags): CLOSE_RANGE_CLOEXEC (4) marks them instead
            436 => {
                let fds: Vec<i32> = self
                    .files
                    .fds
                    .range(a[0] as i32..=(a[1].min(i32::MAX as u64) as i32))
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
            },

            // ---------------------------------------------------- paths and files
            // openat
            56 => {
                let path = self.cstr(a[1]);
                self.openat(a[0] as i32, &path, a[2] as u32, a[3] as u32)
            },
            // newfstatat
            79 => match self.stat_path(a[0] as i32, &self.cstr(a[1]), a[3]) {
                Ok(st) => v(if self.write(a[2], &st.to_stat()) { 0 } else { -EFAULT }),
                Err(e) => v(e),
            },
            // fstat
            80 => match self.stat_fd(fd0) {
                Ok(st) => v(if self.write(a[1], &st.to_stat()) { 0 } else { -EFAULT }),
                Err(e) => v(e),
            },
            // statx(dirfd, path, flags, mask, buf)
            291 => match self.stat_path(a[0] as i32, &self.cstr(a[1]), a[2]) {
                Ok(st) => v(if self.write(a[4], &st.to_statx()) { 0 } else { -EFAULT }),
                Err(e) => v(e),
            },
            // faccessat, faccessat2
            48 | 439 => {
                let path = self.cstr(a[1]);
                let flags = if nr == 439 { a[3] } else { 0 };
                match self.stat_path(a[0] as i32, &path, flags) {
                    Ok(st)
                        if a[2] & 1 != 0 && st.mode & 0o111 == 0 && st.mode & S_IFMT != S_IFDIR =>
                    {
                        v(-EACCES)
                    },
                    Ok(_) => v(0),
                    Err(e) => v(e),
                }
            },
            // readlinkat
            78 => {
                let path = self.cstr(a[1]);
                let target = if path == b"/proc/self/exe" || path == b"/proc/thread-self/exe" {
                    Ok(self.p.exe.clone())
                } else if let Some(fd) = path
                    .strip_prefix(b"/proc/self/fd/")
                    .and_then(|n| std::str::from_utf8(n).ok()?.parse::<i32>().ok())
                {
                    // What it is open on, as Linux says it: realpath in Zig, Bun and Deno is
                    // open, then this link (the Supabase CLI resolves its project root so)
                    match self.files.get(fd).map(|d| d.kind.clone()) {
                        Some(Kind::Dir { ino, .. }) => Ok(self
                            .fs(|fs| files::path_of(fs, ino))
                            .unwrap_or_else(|| format!("/dev/fd/{fd}").into_bytes())),
                        Some(Kind::File { ino, .. }) => Ok(self
                            .fs(|fs| files::find_path(fs, ino))
                            .flatten()
                            .unwrap_or_else(|| format!("/dev/fd/{fd}").into_bytes())),
                        Some(Kind::PipeRead(p) | Kind::PipeWrite(p)) => {
                            Ok(format!("pipe:[{p}]").into_bytes())
                        },
                        Some(Kind::Sock(id)) => Ok(format!("socket:[{id}]").into_bytes()),
                        Some(Kind::Tty) => Ok(b"/dev/tty".to_vec()),
                        Some(Kind::Null) => Ok(b"/dev/null".to_vec()),
                        Some(Kind::Zero) => Ok(b"/dev/zero".to_vec()),
                        Some(Kind::Random) => Ok(b"/dev/urandom".to_vec()),
                        Some(Kind::Event { .. }) => Ok(b"anon_inode:[eventfd]".to_vec()),
                        Some(Kind::Signal { .. }) => Ok(b"anon_inode:[signalfd]".to_vec()),
                        Some(Kind::Pidfd { .. }) => Ok(b"anon_inode:[pidfd]".to_vec()),
                        Some(Kind::Epoll { .. }) => Ok(b"anon_inode:[eventpoll]".to_vec()),
                        Some(Kind::Mem { .. }) => Ok(format!("/dev/fd/{fd}").into_bytes()),
                        None => Err(-ENOENT),
                    }
                } else {
                    self.lookup(a[0] as i32, &path, false).and_then(|ino| {
                        self.fs(|fs| match fs.get(ino).map(|i| &i.content) {
                            Some(armless::dev::fs::Content::Symlink(t)) => Ok(t.clone()),
                            _ => Err(-EINVAL),
                        })
                        .unwrap_or(Err(-ENOENT))
                    })
                };
                match target {
                    Ok(t) => {
                        let n = t.len().min(a[3] as usize);
                        v(if self.write(a[2], &t[..n]) { n as i64 } else { -EFAULT })
                    },
                    Err(e) => v(e),
                }
            },
            // getdents64
            61 => {
                let Some(Desc {
                    kind: Kind::Dir { ino, pos },
                    ..
                }) = self.files.get(fd0).cloned()
                else {
                    return v(if self.files.get(fd0).is_some() { -ENOTDIR } else { -EBADF });
                };
                match self.fs(|fs| files::dirents(fs, ino, pos, a[2] as usize)) {
                    Some(Ok((bytes, next))) => {
                        if !self.write(a[1], &bytes) {
                            return v(-EFAULT);
                        }
                        if let Some(Desc {
                            kind: Kind::Dir { pos, .. },
                            ..
                        }) = self.files.get_mut(fd0)
                        {
                            *pos = next;
                        }
                        v(bytes.len() as i64)
                    },
                    Some(Err(e)) => v(e),
                    None => v(-EBADF),
                }
            },
            // getcwd
            17 => {
                let mut path = self
                    .fs(|fs| files::path_of(fs, self.p.cwd))
                    .unwrap_or_else(|| b"/".to_vec());
                path.push(0);
                if path.len() > a[1] as usize {
                    return v(-ERANGE);
                }
                v(if self.write(a[0], &path) { path.len() as i64 } else { -EFAULT })
            },
            // chdir
            49 => match self.lookup(AT_FDCWD, &self.cstr(a[0]), true) {
                Ok(ino)
                    if self
                        .fs(|fs| fs.get(ino).is_some_and(|i| i.is_dir()))
                        .unwrap_or(false) =>
                {
                    self.p.cwd = ino;
                    v(0)
                },
                Ok(_) => v(-ENOTDIR),
                Err(e) => v(e),
            },
            // fchdir
            50 => match self.files.get(fd0).map(|d| &d.kind) {
                Some(Kind::Dir { ino, .. }) => {
                    self.p.cwd = *ino;
                    v(0)
                },
                Some(_) => v(-ENOTDIR),
                None => v(-EBADF),
            },
            // mkdirat
            34 => {
                let (path, mode, now, umask) =
                    (self.cstr(a[1]), a[2] as u32, self.now_secs(), self.p.umask);
                let (uid, gid) = (self.p.creds.euid(), self.p.creds.egid());
                let start = match self.start_dir(a[0] as i32) {
                    Ok(s) => s,
                    Err(e) => return v(e),
                };
                v(self
                    .fs(|fs| {
                        let (dir, name) = files::resolve_parent(fs, start, &path)?;
                        fs.create(dir, &name, S_IFDIR | (mode & !umask & 0o7777), uid, gid, now)
                            .map(|_| 0)
                            .map_err(errno)
                    })
                    .unwrap_or(Err(-ENOENT))
                    .unwrap_or_else(|e| e))
            },
            // unlinkat
            35 => {
                let (path, now) = (self.cstr(a[1]), self.now_secs());
                let start = match self.start_dir(a[0] as i32) {
                    Ok(s) => s,
                    Err(e) => return v(e),
                };
                v(self
                    .fs(|fs| {
                        let (dir, name) = files::resolve_parent(fs, start, &path)?;
                        fs.unlink(dir, &name, a[2] & AT_REMOVEDIR != 0, now)
                            .map(|_| 0)
                            .map_err(errno)
                    })
                    .unwrap_or(Err(-ENOENT))
                    .unwrap_or_else(|e| e))
            },
            // symlinkat(target, dirfd, path)
            36 => {
                let (target, path, now) = (self.cstr(a[0]), self.cstr(a[2]), self.now_secs());
                let (uid, gid) = (self.p.creds.euid(), self.p.creds.egid());
                let start = match self.start_dir(a[1] as i32) {
                    Ok(s) => s,
                    Err(e) => return v(e),
                };
                v(self
                    .fs(|fs| {
                        let (dir, name) = files::resolve_parent(fs, start, &path)?;
                        fs.symlink(dir, &name, &target, uid, gid, now)
                            .map(|_| 0)
                            .map_err(errno)
                    })
                    .unwrap_or(Err(-ENOENT))
                    .unwrap_or_else(|e| e))
            },
            // linkat(olddirfd, old, newdirfd, new, flags)
            37 => {
                let (old, new, now) = (self.cstr(a[1]), self.cstr(a[3]), self.now_secs());
                let (os, ns) = match (self.start_dir(a[0] as i32), self.start_dir(a[2] as i32)) {
                    (Ok(o), Ok(n)) => (o, n),
                    (Err(e), _) | (_, Err(e)) => return v(e),
                };
                v(self
                    .fs(|fs| {
                        let target = files::resolve(fs, os, &old, a[4] & 0x400 != 0)?;
                        let (dir, name) = files::resolve_parent(fs, ns, &new)?;
                        fs.link(dir, &name, target, now).map(|_| 0).map_err(errno)
                    })
                    .unwrap_or(Err(-ENOENT))
                    .unwrap_or_else(|e| e))
            },
            // renameat, renameat2
            38 | 276 => {
                let (old, new, now) = (self.cstr(a[1]), self.cstr(a[3]), self.now_secs());
                let (os, ns) = match (self.start_dir(a[0] as i32), self.start_dir(a[2] as i32)) {
                    (Ok(o), Ok(n)) => (o, n),
                    (Err(e), _) | (_, Err(e)) => return v(e),
                };
                v(self
                    .fs(|fs| {
                        let (od, on) = files::resolve_parent(fs, os, &old)?;
                        let (nd, nn) = files::resolve_parent(fs, ns, &new)?;
                        if nr == 276 && a[4] & 1 != 0 && fs.lookup(nd, &nn).is_ok() {
                            return Err(-EEXIST); // RENAME_NOREPLACE
                        }
                        fs.rename(od, &on, nd, &nn, now).map(|_| 0).map_err(errno)
                    })
                    .unwrap_or(Err(-ENOENT))
                    .unwrap_or_else(|e| e))
            },
            // ftruncate
            46 => match self.files.get(fd0).map(|d| d.kind.clone()) {
                Some(Kind::File { ino, .. }) => v(self
                    .fs(|fs| fs.truncate(ino, a[1]).map(|_| 0).unwrap_or_else(errno))
                    .unwrap_or(-EBADF)),
                Some(_) => v(-EINVAL),
                None => v(-EBADF),
            },
            // truncate
            45 => match self.lookup(AT_FDCWD, &self.cstr(a[0]), true) {
                Ok(ino) => v(self
                    .fs(|fs| fs.truncate(ino, a[1]).map(|_| 0).unwrap_or_else(errno))
                    .unwrap_or(-ENOENT)),
                Err(e) => v(e),
            },
            // fchmod, fchmodat
            52 | 53 => {
                let (ino, mode) = if nr == 52 {
                    match self.files.get(fd0).map(|d| &d.kind) {
                        Some(Kind::File { ino, .. } | Kind::Dir { ino, .. }) => (Ok(*ino), a[1]),
                        _ => (Err(-EBADF), 0),
                    }
                } else {
                    (self.lookup(a[0] as i32, &self.cstr(a[1]), true), a[2])
                };
                match ino {
                    Ok(ino) => {
                        self.fs(|fs| {
                            if let Some(i) = fs.get_mut(ino) {
                                i.mode = (i.mode & S_IFMT) | (mode as u32 & 0o7777);
                            }
                        });
                        v(0)
                    },
                    Err(e) => v(e),
                }
            },
            // fchown(fd, uid, gid), fchownat(dirfd, path, uid, gid, flags): -1 leaves one; root
            // may give a file to anyone, its owner only to one of its own groups
            55 | 54 => {
                const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
                const AT_EMPTY_PATH: u64 = 0x1000;
                let (ino, uid, gid) = if nr == 55
                    || a[4] & AT_EMPTY_PATH != 0 && self.cstr(a[1]).is_empty()
                {
                    let fd = if nr == 55 { fd0 } else { a[0] as i32 };
                    let (uid, gid) = if nr == 55 { (a[1], a[2]) } else { (a[2], a[3]) };
                    match self.files.get(fd).map(|d| &d.kind) {
                        Some(Kind::File { ino, .. } | Kind::Dir { ino, .. }) => (Ok(*ino), uid, gid),
                        Some(_) => (Err(-(crate::files::EINVAL)), 0, 0),
                        None => (Err(-EBADF), 0, 0),
                    }
                } else {
                    let follow = a[4] & AT_SYMLINK_NOFOLLOW == 0;
                    (self.lookup(a[0] as i32, &self.cstr(a[1]), follow), a[2], a[3])
                };
                let (uid, gid) = (uid as u32, gid as u32);
                let c = self.p.creds;
                let ino = match ino {
                    Ok(i) => i,
                    Err(e) => return v(e),
                };
                let done = self.fs(|fs| {
                    let Some(i) = fs.get_mut(ino) else {
                        return -ENOENT;
                    };
                    let root = c.euid() == 0;
                    let uid_ok = uid == u32::MAX || root || uid == i.uid && i.uid == c.euid();
                    let gid_ok = gid == u32::MAX || root || i.uid == c.euid() && c.gid.contains(&gid);
                    if !(uid_ok && gid_ok) {
                        return -1; // EPERM
                    }
                    if uid != u32::MAX {
                        i.uid = uid;
                    }
                    if gid != u32::MAX {
                        i.gid = gid;
                    }
                    0
                });
                v(done.unwrap_or(-ENOENT))
            },
            // utimensat, sync, fsync, fdatasync, syncfs, fadvise64, flock
            88 | 81 | 82 | 83 | 267 | 223 | 32 => v(0),
            // statfs, fstatfs: one 9p filesystem
            43 | 44 => {
                let mut b = [0u8; 120];
                for (at, val) in [
                    (0, 0x0102_1997u64),
                    (8, 4096),
                    (16, 1 << 24),
                    (24, 1 << 23),
                    (32, 1 << 23),
                    (40, 1 << 20),
                    (48, 1 << 19),
                    (64, 255),
                    (72, 4096),
                ] {
                    b[at..at + 8].copy_from_slice(&val.to_le_bytes());
                }
                v(if self.write(a[1], &b) { 0 } else { -EFAULT })
            },
            // umask
            166 => {
                let old = self.p.umask;
                self.p.umask = a[0] as u32 & 0o777;
                v(old as i64)
            },

            // ---------------------------------------------------- descriptors
            // dup
            23 => match self.files.desc_of(fd0) {
                Some(d) => v(self.files.install(d, 0, false) as i64),
                None => v(-EBADF),
            },
            // dup3
            24 => match self.files.desc_of(fd0) {
                Some(_) if a[0] == a[1] => v(-EINVAL),
                Some(d) => {
                    self.close_fd(a[1] as i32);
                    self.files.put(a[1] as i32, d, a[2] as u32 & O_CLOEXEC != 0);
                    v(a[1] as i64)
                },
                None => v(-EBADF),
            },
            // fcntl
            25 => {
                let Some(d) = self.files.desc_of(fd0) else {
                    return v(-EBADF);
                };
                match a[1] {
                    0 | 1030 => v(self.files.install(d, a[2] as i32, a[1] == 1030) as i64),
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
                    // F_GETLK: unlocked; F_SETLK(W), OFD locks: granted
                    5 | 36 => {
                        self.write(a[2], &2u16.to_le_bytes()); // F_UNLCK
                        v(0)
                    },
                    6 | 7 | 37 | 38 => v(0),
                    // F_SETOWN, F_SETSIG: who gets SIGIO, which isn't sent; F_GETOWN, F_GETSIG
                    8 | 10 => v(0),
                    9 | 11 => v(0),
                    // F_SETPIPE_SZ, F_GETPIPE_SZ
                    1031 | 1032 => v(PIPE_BUF_MAX as i64),
                    _ => v(-EINVAL),
                }
            },
            // ioctl
            29 => self.ioctl(fd0, a[1] as u32, a[2]),
            // pipe2
            59 => {
                let p = self.files.new_pipe();
                let flags = a[1] as u32;
                let nb = flags & O_NONBLOCK;
                let r = self.files.add(Kind::PipeRead(p), nb);
                let w = self.files.add(Kind::PipeWrite(p), O_WRONLY | nb);
                let cloexec = flags & O_CLOEXEC != 0;
                let (rf, wf) = (
                    self.files.install(r, 0, cloexec),
                    self.files.install(w, 0, cloexec),
                );
                v(
                    if self.put_u32(a[0], rf as u32) && self.put_u32(a[0] + 4, wf as u32) {
                        0
                    } else {
                        -EFAULT
                    },
                )
            },
            // getitimer, setitimer (itimer.rs)
            102 => self.sys_getitimer(a[0], a[1]),
            103 => self.sys_setitimer(a[0], a[1], a[2]),
            // shmget, shmctl, shmat, shmdt (shm.rs)
            194 => self.sys_shmget(a[0] as i32, a[1], a[2]),
            195 => self.sys_shmctl(a[0] as i32, a[1], a[2]),
            196 => self.sys_shmat(a[0] as i32, a[1], a[2]),
            197 => self.sys_shmdt(a[0]),
            // pidfd_open(pid, flags), pidfd_send_signal(pidfd, sig, info, flags) (proc.rs)
            434 => self.sys_pidfd_open(a[0] as i32 as i64, a[1]),
            424 => self.sys_pidfd_send_signal(a[0] as i32, a[1] as u32),
            // signalfd4(fd, mask, sizemask, flags)
            74 => self.sys_signalfd(a[0] as i32, a[1], a[2], a[3]),
            // eventfd2
            19 => {
                let d = self
                    .files
                    .add(Kind::Event { count: a[0] }, 2 | (a[1] as u32 & O_NONBLOCK));
                v(self.files.install(d, 0, a[1] as u32 & O_CLOEXEC != 0) as i64)
            },
            // epoll_create1
            20 => {
                let d = self.files.add(
                    Kind::Epoll {
                        watch: Default::default(),
                    },
                    2,
                );
                v(self.files.install(d, 0, a[0] as u32 & O_CLOEXEC != 0) as i64)
            },
            // epoll_ctl
            21 => {
                let (ev, data) = (
                    self.u32_at(a[3]).unwrap_or(0),
                    self.u64_at(a[3] + 8).unwrap_or(0),
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
            },
            // epoll_pwait, epoll_pwait2
            22 | 441 => {
                let ready = self.epoll_ready(fd0, a[2] as usize, true);
                if !ready.is_empty() {
                    let n = ready.len();
                    for (i, &(ev, data)) in ready[..n].iter().enumerate() {
                        // struct epoll_event on arm64: u32 events, 4 bytes of padding, u64 data
                        let at = a[1] + 16 * i as u64;
                        self.put_u32(at, ev);
                        self.put_u64(at + 8, data);
                    }
                    return v(n as i64);
                }
                let deadline = if nr == 441 {
                    if a[3] == 0 {
                        None
                    } else {
                        self.timespec(a[3]).map(|d| Instant::now() + d)
                    }
                } else {
                    let ms = a[3] as i32;
                    if ms == 0 {
                        return v(0);
                    }
                    (ms > 0).then(|| Instant::now() + Duration::from_millis(ms as u64))
                };
                Ret::Retry {
                    deadline: self.resume_deadline.or(deadline),
                    on_timeout: 0,
                }
            },
            // ppoll
            73 => self.ppoll(a[0], a[1], a[2]),
            // pselect6
            72 => self.pselect(a[0], a[1], a[2], a[3], a[4]),
            // socket, socketpair and the rest of the network (net.rs)
            198..=212 | 242 | 243 | 269 => self.net_syscall(nr, a),

            // ---------------------------------------------------- memory
            // brk
            214 => v(self.sys_brk(a[0])),
            // mmap
            222 => self.sys_mmap(a[0], a[1], a[2], a[3], a[4] as i32, a[5]),
            // munmap
            215 => v(self.sys_munmap(a[0], a[1])),
            // mremap(old, old_len, new_len, flags, new)
            216 => v(self.sys_mremap(a[0], page_up(a[1]), page_up(a[2]), a[3])),
            // mprotect
            226 => v(self.sys_mprotect(a[0], a[1], a[2])),
            // msync, mlock, munlock, mbind, set_mempolicy, get_mempolicy
            227 | 228 | 229 | 235 | 237 | 236 => v(0),
            // madvise: DONTNEED and FREE give anonymous pages back
            233 => v(self.sys_madvise(a[0], a[1], a[2])),

            // ---------------------------------------------------- threads
            // clone: a thread, or a process (fork, vfork)
            220 => self.sys_clone(
                cpu,
                cur,
                CloneArgs {
                    flags: a[0] & !0xff,
                    exit_signal: (a[0] & 0xff) as u32,
                    sp: a[1],
                    ptid: a[2],
                    tls: a[3],
                    ctid: a[4],
                },
            ),
            435 => self.sys_clone3(cpu, cur, a[0], a[1]),
            // futex
            98 => {
                let (addr, op, val) = (a[0], a[1] as u32, a[2]);
                let realtime = op & 256 != 0;
                // FUTEX_PRIVATE_FLAG: the process's own; without it, a futex in shared memory is
                // the one every process mapping it meets at (futex_key)
                let private = op & 128 != 0;
                match op & 0x7f {
                    0 | 9 => {
                        let Some(cur_val) = self.u32_at(addr) else {
                            return v(-EFAULT);
                        };
                        if cur_val != val as u32 {
                            return v(-EAGAIN);
                        }
                        let mask = if op & 0x7f == 9 { a[5] as u32 } else { u32::MAX };
                        let deadline = if a[3] == 0 {
                            None
                        } else if op & 0x7f == 0 {
                            self.timespec(a[3]).map(|d| Instant::now() + d)
                        } else {
                            self.timespec(a[3])
                                .map(|t| self.deadline_at(if realtime { 0 } else { 1 }, t))
                        };
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
            },
            // set_tid_address
            96 => {
                self.threads[cur].clear_tid = a[0];
                v(tid)
            },
            // set_robust_list, prctl, membarrier, sched_setaffinity
            99 | 167 | 283 | 122 => v(0),
            // sched_set/getparam, sched_set/getscheduler, sched_get_priority_max/min,
            // sched_rr_get_interval, set/getpriority, sched_set/getattr
            118..=121 | 125..=127 | 140 | 141 | 274 | 275 => self.sys_sched(nr, cur, &a),
            // rseq: glibc and others fall back when it is missing
            293 => v(-ENOSYS),
            // sched_yield
            124 => Ret::Yield,
            // sched_getaffinity
            123 => {
                if a[1] < 8 {
                    return v(-EINVAL);
                }
                v(if self.put_u64(a[2], (1 << NCPUS_REPORTED) - 1) { 8 } else { -EFAULT })
            },
            // getcpu
            168 => {
                for at in [a[0], a[1]] {
                    if at != 0 {
                        self.put_u32(at, 0);
                    }
                }
                v(0)
            },
            // exit
            93 => Ret::ThreadExit(a[0] as i32),
            // exit_group
            94 => Ret::GroupExit(a[0] as i32),
            260 => self.sys_wait4(&a),
            95 => self.sys_waitid(&a),
            221 => self.sys_execve(cpu, cur, AT_FDCWD, [a[0], a[1], a[2], 0]),
            281 => self.sys_execve(cpu, cur, a[0] as i32, [a[1], a[2], a[3], a[4]]),

            // ---------------------------------------------------- ids
            178 => v(tid),
            172 => v(self.p.pid),
            173 => v(self.p.ppid),
            // getuid, geteuid, getgid, getegid
            174 => v(self.p.creds.uid[0] as i64),
            175 => v(self.p.creds.euid() as i64),
            176 => v(self.p.creds.gid[0] as i64),
            177 => v(self.p.creds.egid() as i64),
            // setuid, setgid, setreuid, setregid, setresuid, setresgid, setfsuid, setfsgid,
            // getresuid, getresgid (creds.rs)
            143..=152 => self.sys_ids(nr, &a),
            // getpgid, getsid
            155 | 156 => self.sys_getpgid(nr, a[0] as i32 as i64),
            157 => self.sys_setsid(),
            154 => self.sys_setpgid(a[0] as i32 as i64, a[1] as i32 as i64),
            // getgroups: no supplementary groups
            158 => v(0),
            // setgroups(size, list): root may (su calls it before setgid and setuid). The groups
            // aren't kept: nothing here checks permissions by them.
            159 => {
                const NGROUPS_MAX: u64 = 65536;
                if self.p.creds.euid() != 0 {
                    v(-1) // EPERM
                } else if a[0] > NGROUPS_MAX {
                    v(-(crate::files::EINVAL))
                } else if a[0] > 0 && self.bytes_at(a[1], a[0] as usize * 4).is_none() {
                    v(-EFAULT)
                } else {
                    v(0)
                }
            },
            // capget: no capabilities
            90 => {
                if a[1] != 0 {
                    self.write(a[1], &[0u8; 24]);
                }
                v(0)
            },

            // ---------------------------------------------------- signals (signal.rs)
            134 => self.sys_sigaction(a[0], a[1], a[2], a[3]),
            135 => self.sys_sigprocmask(cur, &a),
            136 => self.sys_sigpending(cur, a[0]),
            132 => self.sys_sigaltstack(cur, a[0], a[1], cpu.sp_el0),
            // kill, tkill, tgkill (Go's SIGURG preemption among them)
            129..=131 => self.sys_kill(nr, &a),
            139 => self.sigreturn(cpu, cur),
            133 => self.sys_sigsuspend(cur, a[0], a[1]),
            137 => self.sys_sigtimedwait(cur, &a),

            // ---------------------------------------------------- time and the system
            113 => {
                let ns = self.now_ns(a[0]);
                let ok = self.put_u64(a[1], ns / 1_000_000_000)
                    && self.put_u64(a[1] + 8, ns % 1_000_000_000);
                v(if ok { 0 } else { -EFAULT })
            },
            // gettimeofday: V8 reads its Date.now() with it, and keeps what was there when it fails
            169 => {
                let ns = self.now_ns(0);
                let ok = (a[0] == 0
                    || self.put_u64(a[0], ns / 1_000_000_000)
                        && self.put_u64(a[0] + 8, ns % 1_000_000_000 / 1000))
                    && (a[1] == 0 || self.put_u64(a[1], 0));
                v(if ok { 0 } else { -EFAULT })
            },
            114 => {
                if a[1] != 0 {
                    self.put_u64(a[1], 0);
                    self.put_u64(a[1] + 8, 1);
                }
                v(0)
            },
            101 => match self.timespec(a[0]) {
                Some(d) => Ret::Block(Wait::Sleep, Some(Instant::now() + d)),
                None => v(-EFAULT),
            },
            115 => match self.timespec(a[2]) {
                Some(d) if a[1] & 1 != 0 => {
                    Ret::Block(Wait::Sleep, Some(self.deadline_at(a[0], d)))
                },
                Some(d) => Ret::Block(Wait::Sleep, Some(Instant::now() + d)),
                None => v(-EFAULT),
            },
            // getrandom
            278 => {
                let bytes = host::random_bytes(a[1] as usize);
                v(if self.write(a[0], &bytes) { a[1] as i64 } else { -EFAULT })
            },
            // prlimit64(pid, resource, new, old), getrlimit(resource, old)
            261 | 163 => {
                let (res, old) = if nr == 261 { (a[1], a[3]) } else { (a[0], a[1]) };
                if old != 0 {
                    let (c, m) = match res {
                        3 => (crate::STACK_SIZE, u64::MAX),
                        7 => (1024, 4096),
                        _ => (u64::MAX, u64::MAX),
                    };
                    self.put_u64(old, c);
                    self.put_u64(old + 8, m);
                }
                v(0)
            },
            // setrlimit
            164 => v(0),
            // getrusage: the time it has run, as user time
            165 => {
                let mut b = [0u8; 144];
                let el = self.start.elapsed();
                b[0..8].copy_from_slice(&el.as_secs().to_le_bytes());
                b[8..16].copy_from_slice(&(el.subsec_micros() as u64).to_le_bytes());
                v(if self.write(a[1], &b) { 0 } else { -EFAULT })
            },
            // times
            153 => v((self.start.elapsed().as_millis() / 10) as i64),
            // sysinfo
            179 => {
                let mut b = [0u8; 112];
                b[0..8].copy_from_slice(&self.start.elapsed().as_secs().to_le_bytes());
                b[32..40].copy_from_slice(&self.ram_size.to_le_bytes());
                b[40..48].copy_from_slice(&(self.ram_size / 2).to_le_bytes());
                b[80..82].copy_from_slice(&(self.processes() as u16).to_le_bytes());
                b[104..108].copy_from_slice(&1u32.to_le_bytes());
                v(if self.write(a[0], &b) { 0 } else { -EFAULT })
            },
            // uname
            160 => {
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
            },
            _ => {
                *self.stats.unknown.entry(nr).or_default() += 1;
                v(-ENOSYS)
            },
        }
    }

    fn ioctl(&mut self, fd: i32, req: u32, arg: u64) -> Ret {
        let v = Ret::Val;
        let Some(d) = self.files.desc_of(fd) else {
            return v(-EBADF);
        };
        match req {
            // FIONBIO
            0x5421 => {
                let on = self.u32_at(arg).unwrap_or(0) != 0;
                let desc = self.files.descs.get_mut(&d).unwrap();
                desc.flags = if on { desc.flags | O_NONBLOCK } else { desc.flags & !O_NONBLOCK };
                return v(0);
            },
            // FIOASYNC: the flag is kept, but SIGIO isn't sent: a program that asks for it
            // (nginx does, for the channel between its processes) reads with its event loop
            0x5452 => {
                let on = self.u32_at(arg).unwrap_or(0) != 0;
                let desc = self.files.descs.get_mut(&d).unwrap();
                desc.flags = if on { desc.flags | O_ASYNC } else { desc.flags & !O_ASYNC };
                return v(0);
            },
            // FIOCLEX, FIONCLEX
            0x5451 | 0x5450 => {
                self.files.fds.get_mut(&fd).unwrap().1 = req == 0x5451;
                return v(0);
            },
            _ => {},
        }
        let kind = self.files.descs[&d].kind.clone();
        match (kind, req) {
            // TCGETS
            (Kind::Tty, 0x5401) => {
                v(if self.write(arg, &self.tty.termios.to_bytes()) { 0 } else { -EFAULT })
            },
            // TCSETS, TCSETSW, TCSETSF
            (Kind::Tty, 0x5402..=0x5404) => match self.bytes_at(arg, 36) {
                Some(b) => {
                    self.tty.set(
                        crate::tty::Termios::from_bytes(&b.try_into().unwrap()),
                        req == 0x5404,
                    );
                    self.poke();
                    v(0)
                },
                None => v(-EFAULT),
            },
            // TIOCGWINSZ
            (Kind::Tty, 0x5413) => {
                let mut b = [0u8; 8];
                b[0..2].copy_from_slice(&self.tty.rows.to_le_bytes());
                b[2..4].copy_from_slice(&self.tty.cols.to_le_bytes());
                v(if self.write(arg, &b) { 0 } else { -EFAULT })
            },
            // TIOCSWINSZ
            (Kind::Tty, 0x5414) => match self.bytes_at(arg, 4) {
                Some(b) => {
                    self.resize(
                        u16::from_le_bytes([b[2], b[3]]),
                        u16::from_le_bytes([b[0], b[1]]),
                    );
                    v(0)
                },
                None => v(-EFAULT),
            },
            // TIOCGPGRP, TIOCGSID
            (Kind::Tty, 0x540f | 0x5429) => {
                v(if self.put_u32(arg, self.tty.pgrp as u32) { 0 } else { -EFAULT })
            },
            // TIOCSPGRP
            (Kind::Tty, 0x5410) => {
                if let Some(p) = self.u32_at(arg) {
                    self.tty.pgrp = p as i64;
                }
                v(0)
            },
            // FIONREAD
            (Kind::Tty, 0x541b) => {
                v(if self.put_u32(arg, self.tty.pending() as u32) { 0 } else { -EFAULT })
            },
            (Kind::PipeRead(p), 0x541b) => {
                let n = self.files.pipes.get(&p).map_or(0, |p| p.buf.len());
                v(if self.put_u32(arg, n as u32) { 0 } else { -EFAULT })
            },
            (Kind::Sock(id), 0x541b) => {
                let n = self.sock_pending(id);
                v(if self.put_u32(arg, n as u32) { 0 } else { -EFAULT })
            },
            // TIOCSCTTY, TCFLSH, TCXONC, TIOCNOTTY, TCSBRK
            (Kind::Tty, 0x540e | 0x540b | 0x540a | 0x5422 | 0x5409) => v(0),
            (Kind::File { .. } | Kind::Dir { .. }, _) => v(-ENOTTY),
            _ => v(-ENOTTY),
        }
    }
}

/// -errno of a filesystem failure
fn errno(e: armless::dev::fs::Fail) -> i64 {
    match e {
        armless::dev::fs::Fail::Errno(e) => -(e as i64),
        armless::dev::fs::Fail::Need(_) => -5, // EIO: metadata never waits on blobs
    }
}

trait MapOk<T> {
    fn map_ok<U>(self, f: impl FnOnce(T) -> U) -> Got<U>;
}

impl<T> MapOk<T> for Got<T> {
    fn map_ok<U>(self, f: impl FnOnce(T) -> U) -> Got<U> {
        match self {
            Got::Ok(v) => Got::Ok(f(v)),
            Got::Err(e) => Got::Err(e),
            Got::Need(b) => Got::Need(b),
        }
    }
}

#[allow(dead_code)]
fn _uses(_: &Fs) {}
