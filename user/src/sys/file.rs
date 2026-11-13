//! Changing files: directories and links made and removed, renames, sizes, modes, owners, and
//! what statfs and umask say.

use armless::dev::fs::{S_IFDIR, S_IFMT};

use crate::errno::{
    EBADF, EEXIST, EFAULT, EFBIG, EINVAL, EIO, ENODEV, ENOENT, EOPNOTSUPP, EPERM, ESPIPE,
};
use crate::files::{self, Kind, O_ACCMODE};
use crate::{Process, Ret};

use super::{AT_FDCWD, AT_REMOVEDIR};

impl Process {
    /// mkdirat
    pub(super) fn sys_mkdirat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// unlinkat
    pub(super) fn sys_unlinkat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// symlinkat(target, dirfd, path)
    pub(super) fn sys_symlinkat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// linkat(olddirfd, old, newdirfd, new, flags)
    pub(super) fn sys_linkat(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// renameat, renameat2
    pub(super) fn sys_renameat(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// ftruncate
    pub(super) fn sys_ftruncate(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        match self.files.get(fd0).map(|d| d.kind.clone()) {
            Some(Kind::File { ino, .. }) => v(self
                .fs(|fs| fs.truncate(ino, a[1]).map(|_| 0).unwrap_or_else(errno))
                .unwrap_or(-EBADF)),
            Some(_) => v(-EINVAL),
            None => v(-EBADF),
        }
    }

    /// fallocate(fd, mode, offset, len): with mode 0 (posix_fallocate) the file is at least
    /// offset + len long, its new bytes zero (Postgres sizes a POSIX shared memory segment
    /// so); FALLOC_FL_KEEP_SIZE leaves it as it is, as space here is never reserved
    pub(super) fn sys_fallocate(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        const FALLOC_FL_KEEP_SIZE: u64 = 1;
        let (mode, off, len) = (a[1], a[2] as i64, a[3] as i64);
        if off < 0 || len <= 0 {
            return v(-EINVAL);
        }
        if mode & !FALLOC_FL_KEEP_SIZE != 0 {
            return v(-EOPNOTSUPP); // punching holes, zeroing and collapsing ranges
        }
        let Some(end) = off.checked_add(len) else {
            return v(-EFBIG);
        };
        match self.files.get(fd0).map(|d| (d.kind.clone(), d.flags)) {
            Some((Kind::File { ino, .. }, flags)) if flags & O_ACCMODE != 0 => {
                if mode & FALLOC_FL_KEEP_SIZE != 0 {
                    return v(0);
                }
                v(self
                    .fs(|fs| {
                        let size = fs.get(ino).map_or(0, |i| i.size);
                        if (end as u64) <= size {
                            return 0;
                        }
                        fs.truncate(ino, end as u64).map(|_| 0).unwrap_or_else(errno)
                    })
                    .unwrap_or(-EBADF))
            },
            Some((Kind::File { .. }, _)) => v(-EBADF),
            Some((Kind::PipeRead(_) | Kind::PipeWrite(_), _)) => v(-ESPIPE),
            Some(_) => v(-ENODEV),
            None => v(-EBADF),
        }
    }

    /// truncate
    pub(super) fn sys_truncate(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        match self.lookup(AT_FDCWD, &self.cstr(a[0]), true) {
            Ok(ino) => v(self
                .fs(|fs| fs.truncate(ino, a[1]).map(|_| 0).unwrap_or_else(errno))
                .unwrap_or(-ENOENT)),
            Err(e) => v(e),
        }
    }

    /// fchmod, fchmodat
    pub(super) fn sys_fchmod(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
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
    }

    /// fchown(fd, uid, gid), fchownat(dirfd, path, uid, gid, flags): -1 leaves one; root
    /// may give a file to anyone, its owner only to one of its own groups
    pub(super) fn sys_fchown(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd0 = a[0] as i32;
        const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
        const AT_EMPTY_PATH: u64 = 0x1000;
        let (ino, uid, gid) = if nr == 55
            || a[4] & AT_EMPTY_PATH != 0 && self.cstr(a[1]).is_empty()
        {
            let fd = if nr == 55 { fd0 } else { a[0] as i32 };
            let (uid, gid) = if nr == 55 { (a[1], a[2]) } else { (a[2], a[3]) };
            match self.files.get(fd).map(|d| &d.kind) {
                Some(Kind::File { ino, .. } | Kind::Dir { ino, .. }) => (Ok(*ino), uid, gid),
                Some(_) => (Err(-EINVAL), 0, 0),
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
                return -EPERM;
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
    }

    /// statfs, fstatfs: one 9p filesystem
    pub(super) fn sys_statfs(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
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
    }

    /// umask
    pub(super) fn sys_umask(&mut self, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let old = self.p.umask;
        self.p.umask = a[0] as u32 & 0o777;
        v(old as i64)
    }
}

/// -errno of a filesystem failure
fn errno(e: armless::dev::fs::Fail) -> i64 {
    match e {
        armless::dev::fs::Fail::Errno(e) => -(e as i64),
        armless::dev::fs::Fail::Need(_) => -EIO, // metadata never waits on blobs
    }
}
