//! Files: the process's descriptors and what they refer to (open file descriptions, as Linux has
//! them: `dup` and the like share one, with its offset), pipes, paths resolved in the machine's
//! filesystem (armless's, the one the 9p device serves a Linux guest), and the kernel's structs
//! for stat, statx and directory entries.

use std::collections::{BTreeMap, VecDeque};

use armless::dev::fs::{self, Content, Fail, Fs, Ino, ROOT, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};

use crate::net::Net;
use crate::ser::{R, W};

pub(crate) const S_IFCHR: u32 = 0o020000;
pub(crate) const S_IFIFO: u32 = 0o010000;
pub(crate) const S_IFSOCK: u32 = 0o140000;

pub(crate) const O_ACCMODE: u32 = 3;
pub(crate) const O_WRONLY: u32 = 1;
pub(crate) const O_CREAT: u32 = 0o100;
pub(crate) const O_EXCL: u32 = 0o200;
pub(crate) const O_TRUNC: u32 = 0o1000;
pub(crate) const O_APPEND: u32 = 0o2000;
pub(crate) const O_NONBLOCK: u32 = 0o4000;
pub(crate) const O_ASYNC: u32 = 0o20000;
pub(crate) const O_DIRECTORY: u32 = 0o40000;
pub(crate) const O_NOFOLLOW: u32 = 0o100000;
pub(crate) const O_CLOEXEC: u32 = 0o2000000;

pub(crate) const ENOENT: i64 = 2;
pub(crate) const EBADF: i64 = 9;
pub(crate) const EACCES: i64 = 13;
pub(crate) const EEXIST: i64 = 17;
pub(crate) const ENOTDIR: i64 = 20;
pub(crate) const EISDIR: i64 = 21;
pub(crate) const EINVAL: i64 = 22;
pub(crate) const ENAMETOOLONG: i64 = 36;
pub(crate) const ELOOP: i64 = 40;

/// What a call into the filesystem gave: a value, an errno, or blobs to wait for.
pub(crate) enum Got<T> {
    Ok(T),
    Err(i64),
    Need(Vec<String>),
}

pub(crate) fn got<T>(r: fs::Res<T>) -> Got<T> {
    match r {
        Ok(v) => Got::Ok(v),
        Err(Fail::Errno(e)) => Got::Err(-(e as i64)),
        Err(Fail::Need(b)) => Got::Need(b),
    }
}

/// What an open file description is of.
#[derive(Clone)]
pub(crate) enum Kind {
    /// The terminal (stdin, stdout, stderr, /dev/tty)
    Tty,
    Null,
    Zero,
    Random,
    /// A file of the filesystem, and the offset reads and writes go on from
    File {
        ino: Ino,
        pos: u64,
    },
    /// A directory, and how many of its entries getdents gave already
    Dir {
        ino: Ino,
        pos: u64,
    },
    /// Contents made here (/proc files), read-only
    Mem {
        data: Vec<u8>,
        pos: u64,
    },
    PipeRead(u32),
    PipeWrite(u32),
    Event {
        count: u64,
    },
    /// A signalfd: reading it takes the pending signals of `mask` (signal.rs)
    Signal {
        mask: u64,
    },
    /// A pidfd: readable once process `pid` has ended
    Pidfd {
        pid: i64,
    },
    /// A timerfd: its timer and the expirations not read yet (timerfd.rs)
    TimerFd(crate::timerfd::Timer),
    Epoll {
        watch: BTreeMap<i32, Watch>,
    },
    /// A socket of `Files::net`
    Sock(u32),
}

/// What an epoll instance watches on a descriptor: the events asked for (EPOLLET: only as the
/// descriptor becomes ready; EPOLLONESHOT: once until modified), the data given back, and the
/// events it was last ready for (to tell when it becomes ready).
#[derive(Clone, Copy)]
pub(crate) struct Watch {
    pub events: u32,
    pub data: u64,
    pub last: u32,
    /// `Process::epoch` when it was last looked at (an edge-triggered watch on a descriptor that
    /// stays readable is reported again once something changed since). Not saved: a restored
    /// machine starts its epochs again.
    pub epoch: u64,
}

/// An open file description: what it is of, its status flags (O_NONBLOCK, O_APPEND, the access
/// mode), and how many descriptors refer to it.
#[derive(Clone)]
pub(crate) struct Desc {
    pub kind: Kind,
    pub flags: u32,
    pub refs: u32,
}

/// A pipe's buffered bytes, and how many descriptions read and write it (a read past the last
/// writer is the end of input).
#[derive(Default)]
pub(crate) struct Pipe {
    pub buf: VecDeque<u8>,
    pub readers: u32,
    pub writers: u32,
}

/// A pipe holds at most this before a write waits.
pub(crate) const PIPE_BUF_MAX: usize = 64 << 10;

/// The descriptor table and what it refers to.
#[derive(Default)]
pub(crate) struct Files {
    /// fd -> (description, close on exec)
    pub fds: BTreeMap<i32, (u32, bool)>,
    pub descs: BTreeMap<u32, Desc>,
    pub pipes: BTreeMap<u32, Pipe>,
    /// The sockets (see net.rs)
    pub net: Net,
    /// fcntl's and flock's locks (locks.rs)
    pub locks: crate::locks::Locks,
    next_desc: u32,
    next_pipe: u32,
}

impl Files {
    /// stdin, stdout and stderr on the terminal
    pub fn standard() -> Files {
        let mut f = Files::default();
        let tty = f.add(Kind::Tty, 2);
        for fd in 0..3 {
            f.fds.insert(fd, (tty, false));
            f.descs.get_mut(&tty).unwrap().refs = 3;
        }
        f
    }

    /// A new description (no descriptor yet).
    pub fn add(&mut self, kind: Kind, flags: u32) -> u32 {
        let id = self.next_desc;
        self.next_desc += 1;
        if let Kind::PipeRead(p) = kind {
            self.pipes.entry(p).or_default().readers += 1;
        }
        if let Kind::PipeWrite(p) = kind {
            self.pipes.entry(p).or_default().writers += 1;
        }
        self.descs.insert(
            id,
            Desc {
                kind,
                flags,
                refs: 0,
            },
        );
        id
    }

    pub fn new_pipe(&mut self) -> u32 {
        let p = self.next_pipe;
        self.next_pipe += 1;
        self.pipes.insert(p, Pipe::default());
        p
    }

    /// The lowest free descriptor at or above `min`, for description `d`.
    pub fn install(&mut self, d: u32, min: i32, cloexec: bool) -> i32 {
        let fd = (min..).find(|n| !self.fds.contains_key(n)).unwrap();
        self.put(fd, d, cloexec);
        fd
    }

    /// Descriptor `fd` refers to description `d` (closing what it referred to).
    pub fn put(&mut self, fd: i32, d: u32, cloexec: bool) {
        self.close(fd);
        self.fds.insert(fd, (d, cloexec));
        if let Some(desc) = self.descs.get_mut(&d) {
            desc.refs += 1;
        }
    }

    /// The description `fd` refers to.
    pub fn get(&self, fd: i32) -> Option<&Desc> {
        self.fds.get(&fd).and_then(|(d, _)| self.descs.get(d))
    }

    pub fn get_mut(&mut self, fd: i32) -> Option<&mut Desc> {
        let d = self.fds.get(&fd)?.0;
        self.descs.get_mut(&d)
    }

    pub fn desc_of(&self, fd: i32) -> Option<u32> {
        self.fds.get(&fd).map(|&(d, _)| d)
    }

    /// Close `fd`: the description goes when no descriptor refers to it, a pipe end with it.
    /// Returns the inode of a file whose last description went, for its reference.
    pub fn close(&mut self, fd: i32) -> Option<Option<Ino>> {
        let (d, _) = self.fds.remove(&fd)?;
        self.release(d)
    }

    /// One more reference to description `d`: a descriptor passed over a socket (SCM_RIGHTS)
    /// while it is on its way.
    pub fn hold(&mut self, d: u32) {
        if let Some(desc) = self.descs.get_mut(&d) {
            desc.refs += 1;
        }
    }

    /// One reference fewer to description `d` (a descriptor closed, or one passed over a socket
    /// that nobody took): it goes when it has none, as `close` says.
    pub fn release(&mut self, d: u32) -> Option<Option<Ino>> {
        let desc = self.descs.get_mut(&d)?;
        desc.refs -= 1;
        if desc.refs > 0 {
            return Some(None);
        }
        let desc = self.descs.remove(&d)?;
        match desc.kind {
            Kind::PipeRead(p) => self.pipe_end(p, true),
            Kind::PipeWrite(p) => self.pipe_end(p, false),
            Kind::File { ino, .. } | Kind::Dir { ino, .. } => return Some(Some(ino)),
            Kind::Sock(id) => self.net.close(id),
            _ => {},
        }
        Some(None)
    }

    fn pipe_end(&mut self, p: u32, reader: bool) {
        if let Some(pipe) = self.pipes.get_mut(&p) {
            if reader {
                pipe.readers -= 1;
            } else {
                pipe.writers -= 1;
            }
            if pipe.readers == 0 && pipe.writers == 0 {
                self.pipes.remove(&p);
            }
        }
    }

    /// A copy of the descriptor table for a child process: its descriptors refer to the same
    /// descriptions (one more reference each).
    pub fn dup_table(&mut self) -> BTreeMap<i32, (u32, bool)> {
        for (d, _) in self.fds.values() {
            if let Some(desc) = self.descs.get_mut(d) {
                desc.refs += 1;
            }
        }
        self.fds.clone()
    }

    /// A descriptor table (fd, description, close on exec) for a snapshot.
    pub fn save_fds(fds: &BTreeMap<i32, (u32, bool)>, w: &mut W) {
        w.u(fds.len() as u64);
        for (&fd, &(d, cloexec)) in fds {
            w.u(fd as u64);
            w.u(d as u64);
            w.u(cloexec as u64);
        }
    }

    pub fn load_fds(r: &mut R) -> Result<BTreeMap<i32, (u32, bool)>, String> {
        let mut fds = BTreeMap::new();
        for _ in 0..r.u()? {
            let fd = r.u()? as i32;
            fds.insert(fd, (r.u()? as u32, r.u()? != 0));
        }
        Ok(fds)
    }

    /// The descriptions and pipes (the descriptor tables are the processes').
    pub fn save(&self, w: &mut W) {
        w.u(self.next_desc as u64);
        w.u(self.next_pipe as u64);
        w.u(self.pipes.len() as u64);
        for (&id, p) in &self.pipes {
            w.u(id as u64);
            w.b(&p.buf.iter().copied().collect::<Vec<u8>>());
            w.u(p.readers as u64);
            w.u(p.writers as u64);
        }
        w.u(self.descs.len() as u64);
        for (&id, d) in &self.descs {
            w.u(id as u64);
            w.u(d.flags as u64);
            w.u(d.refs as u64);
            match &d.kind {
                Kind::Tty => w.u(0),
                Kind::Null => w.u(1),
                Kind::Zero => w.u(2),
                Kind::Random => w.u(3),
                Kind::File { ino, pos } => {
                    w.u(4);
                    w.u(*ino);
                    w.u(*pos);
                },
                Kind::Dir { ino, pos } => {
                    w.u(5);
                    w.u(*ino);
                    w.u(*pos);
                },
                Kind::Mem { data, pos } => {
                    w.u(6);
                    w.b(data);
                    w.u(*pos);
                },
                Kind::PipeRead(p) => {
                    w.u(7);
                    w.u(*p as u64);
                },
                Kind::PipeWrite(p) => {
                    w.u(8);
                    w.u(*p as u64);
                },
                Kind::Event { count } => {
                    w.u(9);
                    w.u(*count);
                },
                Kind::Sock(id) => {
                    w.u(11);
                    w.u(*id as u64);
                },
                Kind::Signal { mask } => {
                    w.u(12);
                    w.u(*mask);
                },
                Kind::Pidfd { pid } => {
                    w.u(13);
                    w.u(*pid as u64);
                },
                Kind::TimerFd(t) => {
                    w.u(14);
                    t.save(w, crate::host::Instant::now());
                },
                Kind::Epoll { watch } => {
                    w.u(10);
                    w.u(watch.len() as u64);
                    for (&f, w8) in watch {
                        w.u(f as u64);
                        w.u(w8.events as u64);
                        w.u(w8.data);
                        w.u(w8.last as u64);
                    }
                },
            }
        }
        self.net.save(w);
    }

    pub fn load(r: &mut R) -> Result<Files, String> {
        let mut f = Files {
            next_desc: r.u()? as u32,
            next_pipe: r.u()? as u32,
            ..Files::default()
        };
        for _ in 0..r.u()? {
            let id = r.u()? as u32;
            let buf = r.b()?.iter().copied().collect();
            f.pipes.insert(
                id,
                Pipe {
                    buf,
                    readers: r.u()? as u32,
                    writers: r.u()? as u32,
                },
            );
        }
        for _ in 0..r.u()? {
            let id = r.u()? as u32;
            let (flags, refs) = (r.u()? as u32, r.u()? as u32);
            let kind = match r.u()? {
                0 => Kind::Tty,
                1 => Kind::Null,
                2 => Kind::Zero,
                3 => Kind::Random,
                4 => Kind::File {
                    ino: r.u()?,
                    pos: r.u()?,
                },
                5 => Kind::Dir {
                    ino: r.u()?,
                    pos: r.u()?,
                },
                6 => Kind::Mem {
                    data: r.b()?.to_vec(),
                    pos: r.u()?,
                },
                7 => Kind::PipeRead(r.u()? as u32),
                8 => Kind::PipeWrite(r.u()? as u32),
                9 => Kind::Event { count: r.u()? },
                11 => Kind::Sock(r.u()? as u32),
                12 => Kind::Signal { mask: r.u()? },
                13 => Kind::Pidfd { pid: r.u()? as i64 },
                14 => Kind::TimerFd(crate::timerfd::Timer::load(r)?),
                10 => {
                    let mut watch = BTreeMap::new();
                    for _ in 0..r.u()? {
                        watch.insert(
                            r.u()? as i32,
                            Watch {
                                events: r.u()? as u32,
                                data: r.u()?,
                                last: r.u()? as u32,
                                epoch: 0,
                            },
                        );
                    }
                    Kind::Epoll { watch }
                },
                t => return Err(format!("unknown description {t}")),
            };
            f.descs.insert(id, Desc { kind, flags, refs });
        }
        f.net = Net::load(r)?;
        Ok(f)
    }
}

/// Where a path starts: the root, or a directory (the cwd, or a directory descriptor's).
/// Resolves `path` from `start`, following symlinks (the last component's only with
/// `follow_last`), as namei does. Returns the inode, or an errno.
pub(crate) fn resolve(fs: &Fs, start: Ino, path: &[u8], follow_last: bool) -> Result<Ino, i64> {
    if path.is_empty() {
        return Err(-ENOENT);
    }
    if path.len() > 4095 {
        return Err(-ENAMETOOLONG);
    }
    let mut cur = if path[0] == b'/' { ROOT } else { start };
    // Components still to walk, last first, so a symlink's target goes on top
    let mut todo: Vec<Vec<u8>> = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .rev()
        .map(<[u8]>::to_vec)
        .collect();
    let trailing_slash = path.ends_with(b"/");
    let mut links = 0;
    while let Some(name) = todo.pop() {
        let is_last = todo.is_empty();
        let next = match fs.lookup(cur, &name) {
            Ok(i) => i,
            Err(Fail::Errno(e)) => return Err(-(e as i64)),
            Err(Fail::Need(_)) => return Err(-ENOENT),
        };
        let inode = fs.get(next).ok_or(-ENOENT)?;
        if inode.mode & S_IFMT == S_IFLNK && (!is_last || follow_last || trailing_slash) {
            links += 1;
            if links > 40 {
                return Err(-ELOOP);
            }
            let Content::Symlink(target) = &inode.content else {
                return Err(-ENOENT);
            };
            if target.first() == Some(&b'/') {
                cur = ROOT;
            }
            todo.extend(
                target
                    .split(|&b| b == b'/')
                    .filter(|c| !c.is_empty())
                    .rev()
                    .map(<[u8]>::to_vec),
            );
            continue;
        }
        if !is_last && !inode.is_dir() {
            return Err(-ENOTDIR);
        }
        cur = next;
    }
    if trailing_slash && !fs.get(cur).is_some_and(|i| i.is_dir()) {
        return Err(-ENOTDIR);
    }
    Ok(cur)
}

/// The absolute path `path` (from the directory `start`) names with every symlink followed, as
/// realpath gives it: what /proc/self/exe says of a program. (`path_of` finds a directory's path
/// from its inode; a file's has to be walked to.)
pub(crate) fn canonical(fs: &Fs, start: Ino, path: &[u8]) -> Result<Vec<u8>, i64> {
    if path.is_empty() {
        return Err(-ENOENT);
    }
    let split = |p: &[u8]| -> Vec<Vec<u8>> {
        p.split(|&b| b == b'/')
            .filter(|c| !c.is_empty())
            .rev()
            .map(<[u8]>::to_vec)
            .collect()
    };
    // The names walked so far, with their inodes; a relative path starts at `start`'s
    let mut walked: Vec<(Vec<u8>, Ino)> = Vec::new();
    if path[0] != b'/' {
        let mut at = ROOT;
        for name in split(&path_of(fs, start)).into_iter().rev() {
            at = fs.lookup(at, &name).map_err(|_| -ENOENT)?;
            walked.push((name, at));
        }
    }
    let mut todo = split(path);
    let mut links = 0;
    while let Some(name) = todo.pop() {
        let cur = walked.last().map_or(ROOT, |w| w.1);
        match &name[..] {
            b"." => continue,
            b".." => {
                walked.pop();
                continue;
            },
            _ => {},
        }
        let next = fs.lookup(cur, &name).map_err(|_| -ENOENT)?;
        let inode = fs.get(next).ok_or(-ENOENT)?;
        if inode.mode & S_IFMT == S_IFLNK {
            links += 1;
            if links > 40 {
                return Err(-ELOOP);
            }
            let Content::Symlink(target) = &inode.content else {
                return Err(-ENOENT);
            };
            if target.first() == Some(&b'/') {
                walked.clear();
            }
            todo.extend(split(target));
            continue;
        }
        walked.push((name, next));
    }
    let mut out = Vec::new();
    for (name, _) in &walked {
        out.push(b'/');
        out.extend_from_slice(name);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    Ok(out)
}

/// The directory `path`'s last component would be in, and that name (for creating, removing and
/// renaming). The directory is resolved following every symlink.
pub(crate) fn resolve_parent(fs: &Fs, start: Ino, path: &[u8]) -> Result<(Ino, Vec<u8>), i64> {
    let trimmed = {
        let mut p = path;
        while p.len() > 1 && p.ends_with(b"/") {
            p = &p[..p.len() - 1];
        }
        p
    };
    let (dir_path, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(0) => (&b"/"[..], &trimmed[1..]),
        Some(i) => (&trimmed[..i], &trimmed[i + 1..]),
        None => (&b"."[..], trimmed),
    };
    if name.is_empty() {
        return Err(-EEXIST);
    }
    let dir = if dir_path == b"." { start } else { resolve(fs, start, dir_path, true)? };
    if !fs.get(dir).is_some_and(|i| i.is_dir()) {
        return Err(-ENOTDIR);
    }
    Ok((dir, name.to_vec()))
}

/// Where `path` goes when it is a symlink (or a chain of them) that ends at a path `special`
/// recognizes, a device or a /proc file the kernel makes rather than the filesystem holds:
/// images link their log files to /dev/stdout and /dev/stderr, and opening one opens the device.
pub(crate) fn link_to_special(
    fs: &Fs,
    mut start: Ino,
    path: &[u8],
    special: impl Fn(&[u8]) -> bool,
) -> Option<Vec<u8>> {
    let mut path = path.to_vec();
    for _ in 0..40 {
        if path.first() == Some(&b'/') && special(&path) {
            return Some(path);
        }
        let (dir, name) = resolve_parent(fs, start, &path).ok()?;
        let ino = fs.lookup(dir, &name).ok()?;
        let Content::Symlink(target) = &fs.get(ino)?.content else {
            return None;
        };
        // A relative target is from the directory the link is in
        start = dir;
        path = target.clone();
    }
    None
}

/// The absolute path of a file whose inode is `ino`, found by walking the tree from the root (a
/// file has no parent to go up to): for what /proc/self/fd/N says of it. The first of its names
/// if it has more than one; None if it has none (unlinked).
pub(crate) fn find_path(fs: &Fs, ino: Ino) -> Option<Vec<u8>> {
    let mut todo = vec![(ROOT, Vec::new())];
    while let Some((dir, path)) = todo.pop() {
        let Ok(entries) = fs.list(dir) else { continue };
        for (name, i) in entries.into_iter().skip(2) {
            let mut p = path.clone();
            p.push(b'/');
            p.extend_from_slice(&name);
            if i == ino {
                return Some(p);
            }
            if fs.get(i).is_some_and(|n| n.is_dir()) {
                todo.push((i, p));
            }
        }
    }
    None
}

/// The absolute path of a directory (for getcwd), from its parents' entries.
pub(crate) fn path_of(fs: &Fs, mut ino: Ino) -> Vec<u8> {
    let mut parts = Vec::new();
    while ino != ROOT {
        let parent = fs.parent(ino);
        let Ok(entries) = fs.list(parent) else { break };
        let Some((name, _)) = entries.into_iter().skip(2).find(|&(_, i)| i == ino) else {
            break;
        };
        parts.push(name);
        if parent == ino {
            break;
        }
        ino = parent;
    }
    let mut out = Vec::new();
    for p in parts.iter().rev() {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// What stat says of something: a file's inode, or (`ino` 0) a device of the given mode.
pub(crate) struct Stat {
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
}

impl Stat {
    pub fn of(fs: &Fs, ino: Ino) -> Option<Stat> {
        let i = fs.get(ino)?;
        Some(Stat {
            ino,
            mode: i.mode,
            nlink: i.nlink.max(1),
            uid: i.uid,
            gid: i.gid,
            rdev: i.rdev,
            size: i.size,
            atime: i.atime,
            mtime: i.mtime,
            ctime: i.ctime,
        })
    }

    pub fn device(mode: u32, rdev: u64) -> Stat {
        Stat {
            ino: 0,
            mode,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev,
            size: 0,
            atime: (0, 0),
            mtime: (0, 0),
            ctime: (0, 0),
        }
    }

    /// The kernel's struct stat (arm64, 128 bytes)
    pub fn to_stat(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        let put64 =
            |b: &mut [u8; 128], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        let put32 =
            |b: &mut [u8; 128], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put64(&mut b, 0, 0x2a); // st_dev
        put64(&mut b, 8, self.ino);
        put32(&mut b, 16, self.mode);
        put32(&mut b, 20, self.nlink);
        put32(&mut b, 24, self.uid);
        put32(&mut b, 28, self.gid);
        put64(&mut b, 32, self.rdev);
        put64(&mut b, 48, self.size);
        put32(&mut b, 56, 4096); // st_blksize
        put64(&mut b, 64, self.size.div_ceil(512)); // st_blocks
        for (at, (s, ns)) in [(72, self.atime), (88, self.mtime), (104, self.ctime)] {
            put64(&mut b, at, s);
            put64(&mut b, at + 8, ns as u64);
        }
        b
    }

    /// The kernel's struct statx (256 bytes), with the basic stats
    pub fn to_statx(&self) -> [u8; 256] {
        let mut b = [0u8; 256];
        let put64 =
            |b: &mut [u8; 256], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        let put32 =
            |b: &mut [u8; 256], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put32(&mut b, 0, 0x7ff); // STATX_BASIC_STATS
        put32(&mut b, 4, 4096);
        put32(&mut b, 16, self.nlink);
        put32(&mut b, 20, self.uid);
        put32(&mut b, 24, self.gid);
        b[28..30].copy_from_slice(&(self.mode as u16).to_le_bytes());
        put64(&mut b, 32, self.ino);
        put64(&mut b, 40, self.size);
        put64(&mut b, 48, self.size.div_ceil(512));
        for (at, (s, ns)) in [(64, self.atime), (96, self.ctime), (112, self.mtime)] {
            put64(&mut b, at, s);
            put32(&mut b, at + 8, ns);
        }
        put32(&mut b, 128, (self.rdev >> 8) as u32 & 0xfff);
        put32(&mut b, 132, self.rdev as u32 & 0xff);
        b
    }
}

/// d_type of a mode
pub(crate) fn dtype(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFDIR => 4,
        S_IFREG => 8,
        S_IFLNK => 10,
        S_IFCHR => 2,
        S_IFIFO => 1,
        S_IFSOCK => 12,
        _ => 0,
    }
}

/// struct linux_dirent64 entries of `dir` from entry `pos` on, as many as fit in `max` bytes.
/// Returns the bytes and the next position.
pub(crate) fn dirents(fs: &Fs, dir: Ino, pos: u64, max: usize) -> Result<(Vec<u8>, u64), i64> {
    let entries = fs.list(dir).map_err(|_| -ENOTDIR)?;
    let typed = entries
        .into_iter()
        .enumerate()
        .skip(pos as usize)
        .map(|(i, (name, ino))| (ino, i as u64 + 1, fs.get(ino).map_or(0, |i| dtype(i.mode)), name));
    encode_dirents(typed, pos, max)
}

/// linux_dirent64 records of `entries` (inode, the position after it, type, name), as many as
/// `max` bytes hold, and the position to go on from: the one place they are made, for the
/// filesystem's directories and for /proc's processes (procfs.rs).
pub(crate) fn encode_dirents(
    entries: impl IntoIterator<Item = (Ino, u64, u8, Vec<u8>)>,
    pos: u64,
    max: usize,
) -> Result<(Vec<u8>, u64), i64> {
    let mut out = Vec::new();
    let mut next = pos;
    for (ino, off, typ, name) in entries {
        let reclen = (19 + name.len() + 1).next_multiple_of(8);
        if out.len() + reclen > max {
            if out.is_empty() {
                return Err(-EINVAL); // the buffer can't hold even one entry
            }
            break;
        }
        next = off;
        out.extend_from_slice(&ino.to_le_bytes());
        out.extend_from_slice(&(off as i64).to_le_bytes());
        out.extend_from_slice(&(reclen as u16).to_le_bytes());
        out.push(typ);
        out.extend_from_slice(&name);
        out.resize(out.len() + reclen - 19 - name.len(), 0);
    }
    Ok((out, next))
}

/// The devices a path may name, which the filesystem doesn't hold (the guest has no devtmpfs):
/// the kind of description they open, and their mode and device number for stat.
pub(crate) fn device(path: &[u8]) -> Option<(Kind, u32, u64)> {
    Some(match path {
        b"/dev/null" => (Kind::Null, S_IFCHR | 0o666, 0x103),
        b"/dev/zero" => (Kind::Zero, S_IFCHR | 0o666, 0x105),
        b"/dev/random" | b"/dev/urandom" => (Kind::Random, S_IFCHR | 0o666, 0x109),
        b"/dev/tty" | b"/dev/console" | b"/dev/stdin" | b"/dev/stdout" | b"/dev/stderr"
        | b"/dev/pts/0" => (Kind::Tty, S_IFCHR | 0o620, 0x8800),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// /usr/lib/libc.so, /lib -> usr/lib, /usr/bin/link -> ../lib/libc.so, /loop -> /loop
    fn tree() -> Fs {
        let mut fs = Fs::from_json(br#"{"version":3,"fsroot":[]}"#).unwrap();
        let usr = fs.create(ROOT, b"usr", S_IFDIR | 0o755, 0, 0, 0).unwrap();
        let lib = fs.create(usr, b"lib", S_IFDIR | 0o755, 0, 0, 0).unwrap();
        let bin = fs.create(usr, b"bin", S_IFDIR | 0o755, 0, 0, 0).unwrap();
        fs.create(lib, b"libc.so", S_IFREG | 0o755, 0, 0, 0)
            .unwrap();
        fs.symlink(ROOT, b"lib", b"usr/lib", 0, 0, 0).unwrap();
        fs.symlink(bin, b"link", b"../lib/libc.so", 0, 0, 0)
            .unwrap();
        fs.symlink(ROOT, b"loop", b"/loop", 0, 0, 0).unwrap();
        fs
    }

    #[test]
    fn paths_resolve_through_symlinks_and_dotdot() {
        let fs = tree();
        let libc = resolve(&fs, ROOT, b"/usr/lib/libc.so", true).unwrap();
        assert_eq!(
            resolve(&fs, ROOT, b"/lib/libc.so", true),
            Ok(libc),
            "a symlinked directory"
        );
        assert_eq!(
            resolve(&fs, ROOT, b"/usr/bin/link", true),
            Ok(libc),
            "a relative target"
        );
        let usr = resolve(&fs, ROOT, b"/usr", true).unwrap();
        assert_eq!(
            resolve(&fs, usr, b"bin/../lib/./libc.so", true),
            Ok(libc),
            "from a directory"
        );
        let link = resolve(&fs, ROOT, b"/usr/bin/link", false).unwrap();
        assert_ne!(link, libc, "the last symlink itself, unfollowed");
        assert_eq!(resolve(&fs, ROOT, b"/loop", true), Err(-ELOOP));
        assert_eq!(resolve(&fs, ROOT, b"/usr/nope", true), Err(-ENOENT));
        assert_eq!(
            resolve(&fs, ROOT, b"/usr/lib/libc.so/x", true),
            Err(-ENOTDIR)
        );
        assert_eq!(
            path_of(&fs, resolve(&fs, ROOT, b"/lib", true).unwrap()),
            b"/usr/lib"
        );
    }

    #[test]
    fn a_parent_and_name_for_creating() {
        let fs = tree();
        let lib = resolve(&fs, ROOT, b"/usr/lib", true).unwrap();
        assert_eq!(
            resolve_parent(&fs, ROOT, b"/lib/new.so"),
            Ok((lib, b"new.so".to_vec()))
        );
        assert_eq!(
            resolve_parent(&fs, lib, b"x/"),
            Ok((lib, b"x".to_vec())),
            "a trailing slash"
        );
        assert_eq!(
            resolve_parent(&fs, ROOT, b"/usr/lib/libc.so/x"),
            Err(-ENOTDIR)
        );
    }

    #[test]
    fn dirents_are_linux_dirent64_and_resume() {
        let fs = tree();
        let usr = resolve(&fs, ROOT, b"/usr", true).unwrap();
        let (b, next) = dirents(&fs, usr, 0, 4096).unwrap();
        assert_eq!(next, 4, ". .. bin lib");
        // The first: ".", a directory, 24 bytes (8-aligned)
        assert_eq!(u16::from_le_bytes([b[16], b[17]]), 24);
        assert_eq!(b[18], 4);
        assert_eq!(&b[19..21], b".\0");
        // Room for one entry only: one, and from there the rest
        let (b1, n1) = dirents(&fs, usr, 0, 24).unwrap();
        assert_eq!((b1.len(), n1), (24, 1));
        assert_eq!(dirents(&fs, usr, 0, 8), Err(-EINVAL));
        assert_eq!(dirents(&fs, usr, 4, 4096).unwrap().0.len(), 0, "the end");
    }

    #[test]
    fn dirents_carry_the_positions_they_are_given() {
        // /proc's go on by pid (procfs.rs): d_off is the entry's own, the next call's start
        let entries = [(7, 3, 4, b"a".to_vec()), (8, 1 << 32 | 1000, 4, b"1000".to_vec())];
        let (b, next) = encode_dirents(entries.clone(), 2, 4096).unwrap();
        assert_eq!(next, 1 << 32 | 1000);
        assert_eq!(i64::from_le_bytes(b[8..16].try_into().unwrap()), 3);
        assert_eq!(&b[24 + 19..24 + 23], b"1000");
        assert_eq!(encode_dirents(entries.clone(), 2, 24).unwrap(), (b[..24].to_vec(), 3));
        assert_eq!(encode_dirents(entries, 2, 16), Err(-EINVAL));
    }
}
