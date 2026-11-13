//! Paths and files: lookups, the files made here (/proc, /dev), open, stat, links, directories.

use armless::dev::fs::{Ino, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

use crate::errno::{EACCES, EBADF, EEXIST, EFAULT, EINVAL, EISDIR, ELOOP, ENOENT, ENOTDIR, ERANGE};
use crate::files::{self, device, Desc, Kind, Stat, S_IFIFO};
use crate::files::{
    O_ACCMODE, O_APPEND, O_CLOEXEC, O_CREAT, O_DIRECTORY, O_EXCL, O_NOFOLLOW, O_NONBLOCK, O_TRUNC,
};
use crate::{Process, Ret};

use super::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};


impl Process {
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
    pub(super) fn proc_file(&self, path: &[u8]) -> Option<Vec<u8>> {
        // /proc/self/fdinfo/N: its position, flags (O_CLOEXEC with them, in octal), and for a
        // pidfd its process (-1 once it was waited for), as Linux says them
        if let Some(fd) = path
            .strip_prefix(b"/proc/self/fdinfo/")
            .and_then(|n| std::str::from_utf8(n).ok()?.parse::<i32>().ok())
        {
            let d = self.files.get(fd)?;
            let cloexec = self.files.fds.get(&fd).is_some_and(|&(_, c)| c);
            let pos = match d.kind {
                Kind::File { pos, .. } | Kind::Dir { pos, .. } | Kind::Mem { pos, .. } => pos,
                _ => 0,
            };
            let flags = d.flags | if cloexec { O_CLOEXEC } else { 0 };
            let mut s = format!("pos:\t{pos}\nflags:\t0{flags:o}\nmnt_id:\t0\nino:\t0\n");
            if let Kind::Pidfd { pid } = d.kind {
                let pid = if self.proc_ref(pid).is_some() { pid } else { -1 };
                s += &format!("Pid:\t{pid}\nNSpid:\t{pid}\n");
            }
            return Some(s.into_bytes());
        }
        if let Some(f) = self.pid_file(path) {
            return Some(f);
        }
        Some(match path {
            b"/proc/cpuinfo" => (0..self.ncpus_reported())
                .map(|i| format!("processor\t: {i}\nBogoMIPS\t: 100.00\nFeatures\t: fp asimd aes pmull sha1 sha2 crc32 atomics sha3 sm3 sm4 sha512\nCPU implementer\t: 0x41\nCPU architecture: 8\n\n"))
                .collect::<String>()
                .into_bytes(),
            b"/proc/meminfo" => {
                let kb = self.ram_size / 1024;
                format!("MemTotal:       {kb} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\n", kb / 2, kb / 2).into_bytes()
            },
            b"/proc/self/mounts" | b"/proc/mounts" => b"host9p / 9p rw 0 0\nproc /proc proc rw 0 0\n".to_vec(),
            // The interfaces netlink.rs has, with nothing counted (ifconfig reads their list here)
            b"/proc/net/dev" | b"/proc/self/net/dev" => {
                let mut s = String::from("Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n");
                let network = self.board.as_ref().is_some_and(|b| b.has_network());
                for name in ["lo"].into_iter().chain(network.then_some("eth0")) {
                    s += &format!("{name:>6}:{}\n", "       0".repeat(16));
                }
                s.into_bytes()
            },
            b"/proc/sys/kernel/osrelease" => b"6.6.0-armless-user\n".to_vec(),
            b"/sys/fs/cgroup/cpu.max" => b"max 100000\n".to_vec(),
            _ => return None,
        })
    }

    /// Where a symlink in the filesystem sends `path` when its target is a device or a /proc file
    pub(super) fn special_target(&self, start: Ino, path: &[u8]) -> Option<Vec<u8>> {
        self.fs(|fs| {
            files::link_to_special(fs, start, path, |p| {
                device(p).is_some() || self.proc_file(p).is_some()
            })
        })
        .flatten()
    }

    /// `path` from `dirfd` as an absolute path, when it is relative to a directory of the files made
    /// here (/dev, /proc, /sys): a program that opens `null` from a descriptor of /dev, as
    /// wasmtime's guests open every file from their preopened directories, gets /dev/null.
    pub(super) fn under_special(&self, dirfd: i32, path: &[u8]) -> Option<Vec<u8>> {
        if path.first() == Some(&b'/') || path.is_empty() || path.split(|&b| b == b'/').any(|c| c == b"..") {
            return None;
        }
        let dir = match self.files.get(dirfd).map(|d| &d.kind) {
            Some(Kind::Dir { ino, .. }) => self.fs(|fs| files::path_of(fs, *ino))?,
            _ if dirfd == AT_FDCWD => self.fs(|fs| files::path_of(fs, self.p.cwd))?,
            _ => return None,
        };
        // (Its first component; /dev/shm is a directory of the filesystem, shm_open's)
        let top = dir.split(|&b| b == b'/').nth(1).unwrap_or_default();
        if !matches!(top, b"dev" | b"proc" | b"sys") || dir.starts_with(b"/dev/shm") {
            return None;
        }
        let mut full = dir.clone();
        if !full.ends_with(b"/") {
            full.push(b'/');
        }
        full.extend_from_slice(path.strip_prefix(b"./").unwrap_or(path));
        Some(full)
    }

    pub(super) fn openat(&mut self, dirfd: i32, path: &[u8], flags: u32, mode: u32) -> Ret {
        if let Some(full) = self.under_special(dirfd, path) {
            return self.openat(AT_FDCWD, &full, flags, mode);
        }
        // /proc, to list the processes (procfs.rs): a directory of the filesystem, made if the
        // image has none. (musl's glob opens it as "/proc/".)
        let path = match path.strip_prefix(b"/proc") {
            Some(slashes) if !slashes.is_empty() && slashes.iter().all(|&b| b == b'/') => b"/proc",
            _ => path,
        };
        if path == b"/proc" {
            self.make_proc_dir();
        }
        let cloexec = flags & O_CLOEXEC != 0;
        let status = flags & (O_ACCMODE | O_APPEND | O_NONBLOCK);
        let absolute = path.first() == Some(&b'/');
        if absolute {
            if let Some((kind, _, _)) = device(path) {
                let d = self.files.add(kind, status);
                return Ret::Val(self.new_fd(d, 0, cloexec));
            }
            if let Some(data) = self.proc_file(path) {
                let d = self.files.add(Kind::Mem { data, pos: 0 }, status);
                return Ret::Val(self.new_fd(d, 0, cloexec));
            }
            if path.starts_with(b"/proc/")
                || path.starts_with(b"/sys/")
                || path.starts_with(b"/dev/")
                    // POSIX shared memory's files (shm_open) are made in /dev/shm, a directory
                    // of the machine's filesystem here (Process::exec makes it)
                    && !path.starts_with(b"/dev/shm/")
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
                return Err(-ELOOP); // O_NOFOLLOW on a symlink
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
        Ret::Val(self.new_fd(d, 0, cloexec))
    }

    /// What stat says of `fd`
    pub(super) fn stat_fd(&self, fd: i32) -> Result<Stat, i64> {
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
            Kind::Event { .. }
            | Kind::Signal { .. }
            | Kind::Pidfd { .. }
            | Kind::TimerFd(_)
            | Kind::Epoll { .. } => {
                Stat::device(0o600, 0)
            },
        })
    }

    /// What stat says of `path` from `dirfd` (flags: AT_SYMLINK_NOFOLLOW, AT_EMPTY_PATH)
    pub(super) fn stat_path(&self, dirfd: i32, path: &[u8], flags: u64) -> Result<Stat, i64> {
        if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
            return self.stat_fd(dirfd);
        }
        if let Some(full) = self.under_special(dirfd, path) {
            return self.stat_path(AT_FDCWD, &full, flags);
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
        if path == b"/proc" || path == b"/dev" {
            return Ok(Stat::device(S_IFDIR | 0o555, 0));
        }
        // A process's directory, while there is that process
        match self.pid_dir(path) {
            Some(true) => return Ok(Stat::device(S_IFDIR | 0o555, 0)),
            Some(false) => return Err(-ENOENT),
            None => {},
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

    /// openat
    pub(super) fn sys_openat(&mut self, a: &[u64; 6]) -> Ret {
        let path = self.cstr(a[1]);
        self.openat(a[0] as i32, &path, a[2] as u32, a[3] as u32)
    }

    /// newfstatat
    pub(super) fn sys_newfstatat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.stat_path(a[0] as i32, &self.cstr(a[1]), a[3]) {
            Ok(st) => v(if self.write(a[2], &st.to_stat()) { 0 } else { -EFAULT }),
            Err(e) => v(e),
        }
    }

    /// fstat
    pub(super) fn sys_fstat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.stat_fd(fd0) {
            Ok(st) => v(if self.write(a[1], &st.to_stat()) { 0 } else { -EFAULT }),
            Err(e) => v(e),
        }
    }

    /// statx(dirfd, path, flags, mask, buf)
    pub(super) fn sys_statx(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.stat_path(a[0] as i32, &self.cstr(a[1]), a[2]) {
            Ok(st) => v(if self.write(a[4], &st.to_statx()) { 0 } else { -EFAULT }),
            Err(e) => v(e),
        }
    }

    /// faccessat, faccessat2
    pub(super) fn sys_faccessat(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// readlinkat
    pub(super) fn sys_readlinkat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
                Some(Kind::TimerFd(_)) => Ok(b"anon_inode:[timerfd]".to_vec()),
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
    }

    /// getdents64
    pub(super) fn sys_getdents64(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        let Some(Desc {
            kind: Kind::Dir { ino, pos },
            ..
        }) = self.files.get(fd0).cloned()
        else {
            return v(if self.files.get(fd0).is_some() { -ENOTDIR } else { -EBADF });
        };
        let listed = if self.is_proc_dir(ino) {
            Some(self.proc_dirents(ino, pos, a[2] as usize))
        } else {
            self.fs(|fs| files::dirents(fs, ino, pos, a[2] as usize))
        };
        match listed {
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
    }

    /// getcwd
    pub(super) fn sys_getcwd(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let mut path = self
            .fs(|fs| files::path_of(fs, self.p.cwd))
            .unwrap_or_else(|| b"/".to_vec());
        path.push(0);
        if path.len() > a[1] as usize {
            return v(-ERANGE);
        }
        v(if self.write(a[0], &path) { path.len() as i64 } else { -EFAULT })
    }

    /// chdir
    pub(super) fn sys_chdir(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.lookup(AT_FDCWD, &self.cstr(a[0]), true) {
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
        }
    }

    /// fchdir
    pub(super) fn sys_fchdir(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.files.get(fd0).map(|d| &d.kind) {
            Some(Kind::Dir { ino, .. }) => {
                self.p.cwd = *ino;
                v(0)
            },
            Some(_) => v(-ENOTDIR),
            None => v(-EBADF),
        }
    }
}
