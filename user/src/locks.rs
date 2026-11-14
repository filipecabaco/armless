//! File locks, as Linux keeps them:
//!
//! - **Record locks** (fcntl F_GETLK, F_SETLK, F_SETLKW): read and write locks on byte ranges of
//!   a file, held by a process. They go when the process closes any descriptor of the file (all
//!   of its locks on it, whatever descriptor took them) or ends, and a fork's child has none.
//! - **Open file description locks** (F_OFD_GETLK, F_OFD_SETLK, F_OFD_SETLKW): the same, held by
//!   an open file description, and gone with it. They and the record locks are one table: each
//!   conflicts with the other.
//! - **flock**: a shared or exclusive lock on the whole file, held by a description, in a table of
//!   its own.
//!
//! A lock whose holder is gone (a process that ended, a description closed) is dropped when the
//! table is next looked at, so no path that ends one has to know of locks; what ends one pokes,
//! and a call waiting for a lock (F_SETLKW, a flock without LOCK_NB) looks again. SQLite takes
//! its locks so, and the Supabase CLI tells whether its stack's owner is running by one.

use std::collections::BTreeMap;

use armless::dev::fs::Ino;

use crate::errno::{EAGAIN, EBADF, EFAULT, EINVAL, EOVERFLOW};
use crate::files::Kind;
use crate::ser::{R, W};
use crate::{Process, Ret};

/// Who holds a lock
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Owner {
    /// A process's record lock
    Proc(i64),
    /// An open file description's (OFD lock, or flock)
    Desc(u32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Lock {
    pub owner: Owner,
    pub write: bool,
    pub start: u64,
    /// Exclusive; `u64::MAX`: to the end of the file, however far it grows
    pub end: u64,
}

#[derive(Default)]
pub(crate) struct Locks {
    /// Record and OFD locks, by file
    ranges: BTreeMap<Ino, Vec<Lock>>,
    /// flock: by file, the descriptions that hold one, and whether it is exclusive
    flocks: BTreeMap<Ino, Vec<(u32, bool)>>,
}

impl Locks {
    /// Drops the locks whose holder `alive` says is gone.
    pub fn sweep(&mut self, alive: impl Fn(Owner) -> bool) {
        for v in self.ranges.values_mut() {
            v.retain(|l| alive(l.owner));
        }
        self.ranges.retain(|_, v| !v.is_empty());
        for v in self.flocks.values_mut() {
            v.retain(|&(d, _)| alive(Owner::Desc(d)));
        }
        self.flocks.retain(|_, v| !v.is_empty());
    }

    /// A lock another holder has that `owner`'s lock of `start..end` (a write one or not) can't
    /// be taken beside
    pub fn conflict(&self, ino: Ino, owner: Owner, write: bool, start: u64, end: u64) -> Option<Lock> {
        self.ranges.get(&ino)?.iter().copied().find(|l| {
            l.owner != owner && (write || l.write) && l.start < end && start < l.end
        })
    }

    /// `owner` holds `start..end` as `kind` (Some(write), or None: unlocked): what it held of
    /// that range before goes, the rest of its ranges stay (split where the range cuts them).
    /// Check `conflict` first.
    pub fn set(&mut self, ino: Ino, owner: Owner, kind: Option<bool>, start: u64, end: u64) {
        let v = self.ranges.entry(ino).or_default();
        let mut kept = Vec::with_capacity(v.len() + 2);
        for l in v.drain(..) {
            if l.owner != owner || l.end <= start || end <= l.start {
                kept.push(l);
                continue;
            }
            if l.start < start {
                kept.push(Lock { end: start, ..l });
            }
            if end < l.end {
                kept.push(Lock { start: end, ..l });
            }
        }
        if let Some(write) = kind {
            kept.push(Lock {
                owner,
                write,
                start,
                end,
            });
        }
        *v = kept;
        if v.is_empty() {
            self.ranges.remove(&ino);
        }
    }

    /// Process `pid` closed a descriptor of `ino`: its record locks on it go (POSIX's rule).
    pub fn closed(&mut self, ino: Ino, pid: i64) {
        self.set(ino, Owner::Proc(pid), None, 0, u64::MAX);
    }

    /// flock on `ino` for description `d`: Some(exclusive) or None (unlock). Ok, or whether
    /// another description holds what it can't be taken beside (Err).
    pub fn flock(&mut self, ino: Ino, d: u32, kind: Option<bool>) -> Result<(), ()> {
        let v = self.flocks.entry(ino).or_default();
        if let Some(ex) = kind {
            if v.iter().any(|&(o, oex)| o != d && (ex || oex)) {
                return Err(());
            }
        }
        v.retain(|&(o, _)| o != d);
        if let Some(ex) = kind {
            v.push((d, ex));
        }
        if v.is_empty() {
            self.flocks.remove(&ino);
        }
        Ok(())
    }

    pub fn save(&self, w: &mut W) {
        let owner = |w: &mut W, o: Owner| match o {
            Owner::Proc(p) => {
                w.u(0);
                w.u(p as u64);
            },
            Owner::Desc(d) => {
                w.u(1);
                w.u(d as u64);
            },
        };
        w.u(self.ranges.values().map(Vec::len).sum::<usize>() as u64);
        for (&ino, v) in &self.ranges {
            for l in v {
                w.u(ino);
                owner(w, l.owner);
                w.u(l.write as u64);
                w.u(l.start);
                w.u(l.end);
            }
        }
        w.u(self.flocks.values().map(Vec::len).sum::<usize>() as u64);
        for (&ino, v) in &self.flocks {
            for &(d, ex) in v {
                w.u(ino);
                w.u(d as u64);
                w.u(ex as u64);
            }
        }
    }

    pub fn load(r: &mut R) -> Result<Locks, String> {
        let mut locks = Locks::default();
        for _ in 0..r.u()? {
            let ino = r.u()?;
            let owner = match (r.u()?, r.u()?) {
                (0, p) => Owner::Proc(p as i64),
                (_, d) => Owner::Desc(d as u32),
            };
            let (write, start, end) = (r.u()? != 0, r.u()?, r.u()?);
            locks.ranges.entry(ino).or_default().push(Lock {
                owner,
                write,
                start,
                end,
            });
        }
        for _ in 0..r.u()? {
            let (ino, d, ex) = (r.u()?, r.u()? as u32, r.u()? != 0);
            locks.flocks.entry(ino).or_default().push((d, ex));
        }
        Ok(locks)
    }
}

// ------------------------------------------------------------ the calls

impl Process {
    /// The lock table, its locks whose holder is gone dropped first (locks.rs)
    fn locks(&mut self) -> &mut crate::locks::Locks {
        use crate::locks::Owner;
        let pids: Vec<i64> = self
            .proc_list()
            .filter(|p| p.zombie.is_none())
            .map(|p| p.pid)
            .collect();
        let descs = &self.files.descs;
        self.files.locks.sweep(|o| match o {
            Owner::Proc(p) => pids.contains(&p),
            Owner::Desc(d) => descs.contains_key(&d),
        });
        &mut self.files.locks
    }

    /// fcntl's locks: F_GETLK (5), F_SETLK (6), F_SETLKW (7), and F_OFD_GETLK, F_OFD_SETLK,
    /// F_OFD_SETLKW (36, 37, 38), with the struct flock at `at`.
    pub(crate) fn fcntl_lock(&mut self, fd: i32, cmd: u64, at: u64) -> Ret {
        use crate::locks::Owner;
        let v = Ret::Val;
        let (Some(d), Some(desc)) = (self.files.desc_of(fd), self.files.get(fd)) else {
            return v(-EBADF);
        };
        let (ino, pos, mode) = match desc.kind {
            Kind::File { ino, pos } => (ino, pos, desc.flags & 3),
            _ => return v(-EINVAL),
        };
        // struct flock: l_type, l_whence (i16), l_start, l_len (i64, at 8 and 16), l_pid (i32, 24)
        let Some(b) = self.bytes_at(at, 32) else {
            return v(-EFAULT);
        };
        let i16_at = |o: usize| i16::from_le_bytes([b[o], b[o + 1]]);
        let i64_at = |o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let (typ, whence, start, len) = (i16_at(0), i16_at(2), i64_at(8), i64_at(16));
        let ofd = cmd >= 36;
        if ofd && i32::from_le_bytes(b[24..28].try_into().unwrap()) != 0 {
            return v(-EINVAL);
        }
        let kind = match typ {
            0 => Some(false),
            1 => Some(true),
            2 => None,
            _ => return v(-EINVAL),
        };
        // A read lock needs the file open for reading, a write lock for writing
        if cmd != 5 && cmd != 36 {
            match kind {
                Some(false) if mode == 1 => return v(-EBADF),
                Some(true) if mode == 0 => return v(-EBADF),
                _ => {},
            }
        }
        let base = match whence {
            0 => 0,
            1 => pos as i64,
            2 => self.fs(|fs| fs.get(ino).map_or(0, |i| i.size)).unwrap_or(0) as i64,
            _ => return v(-EINVAL),
        };
        let Some(s) = base.checked_add(start) else {
            return v(-EOVERFLOW);
        };
        // A negative length is the bytes before the start
        let (s, e) = match len {
            0 => (s, u64::MAX as i128),
            l if l > 0 => (s, s as i128 + l as i128),
            l => (s + l, s as i128),
        };
        if s < 0 {
            return v(-EINVAL);
        }
        let (s, e) = (s as u64, e.min(u64::MAX as i128) as u64);
        let owner = if ofd { Owner::Desc(d) } else { Owner::Proc(self.p.pid) };
        let locks = self.locks();
        match cmd {
            // F_GETLK: the first lock in the way, or F_UNLCK
            5 | 36 => {
                let mut out = b.clone();
                match locks.conflict(ino, owner, kind != Some(false), s, e) {
                    None => out[0..2].copy_from_slice(&2i16.to_le_bytes()),
                    Some(l) => {
                        out[0..2].copy_from_slice(&(l.write as i16).to_le_bytes());
                        out[2..4].copy_from_slice(&0i16.to_le_bytes());
                        out[8..16].copy_from_slice(&(l.start as i64).to_le_bytes());
                        let len = if l.end == u64::MAX { 0 } else { (l.end - l.start) as i64 };
                        out[16..24].copy_from_slice(&len.to_le_bytes());
                        let holder = match l.owner {
                            Owner::Proc(p) => p as i32,
                            Owner::Desc(_) => -1,
                        };
                        out[24..28].copy_from_slice(&holder.to_le_bytes());
                    },
                }
                v(if self.write(at, &out) { 0 } else { -EFAULT })
            },
            _ => {
                if let Some(write) = kind {
                    if locks.conflict(ino, owner, write, s, e).is_some() {
                        // F_SETLK: EAGAIN; F_SETLKW: wait for the holder to let go
                        if cmd == 6 || cmd == 37 {
                            return v(-EAGAIN);
                        }
                        return Ret::Retry {
                            deadline: None,
                            on_timeout: 0,
                        };
                    }
                }
                locks.set(ino, owner, kind, s, e);
                // (What waits for a lock looks again)
                self.poke();
                v(0)
            },
        }
    }

    /// flock(fd, op): LOCK_SH (1), LOCK_EX (2) or LOCK_UN (8), with LOCK_NB (4) not to wait.
    pub(crate) fn flock(&mut self, fd: i32, op: u64) -> Ret {
        let v = Ret::Val;
        let Some(d) = self.files.desc_of(fd) else {
            return v(-EBADF);
        };
        let ino = match self.files.get(fd).map(|x| &x.kind) {
            Some(Kind::File { ino, .. } | Kind::Dir { ino, .. }) => *ino,
            // (Anything else: nothing else takes its lock)
            Some(_) => return v(0),
            None => return v(-EBADF),
        };
        let kind = match op & !4 {
            1 => Some(false),
            2 => Some(true),
            8 => None,
            _ => return v(-EINVAL),
        };
        match self.locks().flock(ino, d, kind) {
            Ok(()) => {
                self.poke();
                v(0)
            },
            Err(()) if op & 4 != 0 => v(-EAGAIN),
            Err(()) => Ret::Retry {
                deadline: None,
                on_timeout: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_unlocked_in_the_middle_leaves_both_ends() {
        let mut l = Locks::default();
        let me = Owner::Proc(1);
        l.set(5, me, Some(true), 0, 100);
        l.set(5, me, None, 40, 60);
        let other = Owner::Proc(2);
        assert!(l.conflict(5, other, false, 0, 40).is_some());
        assert!(l.conflict(5, other, true, 40, 60).is_none());
        assert!(l.conflict(5, other, false, 60, 61).is_some());
    }

    #[test]
    fn read_locks_share_and_a_write_lock_excludes() {
        let mut l = Locks::default();
        l.set(5, Owner::Proc(1), Some(false), 0, u64::MAX);
        assert!(l.conflict(5, Owner::Proc(2), false, 10, 20).is_none());
        assert!(l.conflict(5, Owner::Desc(3), true, 10, 20).is_some(), "OFD and POSIX conflict");
        assert!(l.conflict(5, Owner::Proc(1), true, 10, 20).is_none(), "its own lock is upgraded");
    }

    #[test]
    fn flock_is_shared_or_exclusive_per_description() {
        let mut l = Locks::default();
        assert!(l.flock(5, 1, Some(false)).is_ok());
        assert!(l.flock(5, 2, Some(false)).is_ok());
        assert!(l.flock(5, 3, Some(true)).is_err());
        assert!(l.flock(5, 1, Some(true)).is_err(), "2 still shares it");
        l.sweep(|o| o != Owner::Desc(2));
        assert!(l.flock(5, 1, Some(true)).is_ok());
    }
}
