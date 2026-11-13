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

use crate::ser::{R, W};

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
