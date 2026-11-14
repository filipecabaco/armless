//! System V shared memory: shmget, shmat, shmdt and shmctl. Postgres makes a small segment as
//! its interlock (another postmaster on the same data directory would find it attached) before
//! anything else, and a program on Linux may use them for its own memory.
//!
//! A segment belongs to the machine, as the filesystem does: it has frames of its own (zero, made
//! at its first attach), every attach maps those same frames as a shared area, and fork shares
//! them as any MAP_SHARED area. IPC_RMID lets go of the segment's own hold on them once nothing
//! is attached by shmat (the areas still mapped keep theirs).

use std::collections::BTreeMap;

use crate::errno::{EACCES, EEXIST, EFAULT, EINVAL, ENOENT, ENOMEM, ENOSPC, EPERM};
use crate::mm::{PROT_READ, PROT_WRITE, VA_END};
use crate::ser::{R, W};
use crate::{page_up, Process, Ret, PAGE};

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_RMID: u64 = 0;
const IPC_SET: u64 = 1;
const IPC_STAT: u64 = 2;
const IPC_INFO: u64 = 3;
const SHM_STAT: u64 = 13;
const SHM_INFO: u64 = 14;
const SHM_STAT_ANY: u64 = 15;
/// IPC_64, which musl adds to every command
const IPC_64: u64 = 0x100;
const SHM_RDONLY: u64 = 0o10000;
const SHM_RND: u64 = 0o20000;
/// SHM_REMAP: an attach at an address may replace what is mapped there
const SHM_REMAP: u64 = 0o40000;
/// SHMLBA on arm64
const SHMLBA: u64 = 4 * PAGE;
/// The most a segment may be (SHMMAX), and how many there may be (SHMMNI)
const SHMMAX: u64 = 1 << 36;
const SHMMNI: usize = 4096;


#[derive(Clone, Debug, Default)]
struct Segment {
    key: i32,
    size: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    cpid: i64,
    lpid: i64,
    /// Attaches made by shmat and not undone by shmdt
    nattch: u64,
    /// IPC_RMID: it goes once nothing is attached
    removed: bool,
    /// Its frames, once it was first attached
    frames: Vec<u64>,
    atime: u64,
    dtime: u64,
    ctime: u64,
}

/// The machine's segments, and where each process attached them.
#[derive(Default)]
pub(crate) struct Shm {
    segs: BTreeMap<i32, Segment>,
    next: i32,
    /// (pid, address) -> (segment, length)
    attached: BTreeMap<(i64, u64), (i32, u64)>,
}

impl Shm {
    pub(crate) fn save(&self, w: &mut W) {
        w.u(self.next as u64);
        w.u(self.segs.len() as u64);
        for (&id, s) in &self.segs {
            w.u(id as u64);
            w.u(s.key as u32 as u64);
            for v in [s.size, s.mode as u64, s.uid as u64, s.gid as u64, s.cuid as u64, s.cgid as u64] {
                w.u(v);
            }
            for v in [s.cpid as u64, s.lpid as u64, s.nattch, s.removed as u64, s.atime, s.dtime, s.ctime] {
                w.u(v);
            }
            w.u(s.frames.len() as u64);
            for &f in &s.frames {
                w.u(f);
            }
        }
        w.u(self.attached.len() as u64);
        for (&(pid, at), &(id, len)) in &self.attached {
            for v in [pid as u64, at, id as u64, len] {
                w.u(v);
            }
        }
    }

    /// The frames the segments hold
    pub(crate) fn frames(&self) -> impl Iterator<Item = u64> + '_ {
        self.segs.values().flat_map(|s| s.frames.iter().copied())
    }

    pub(crate) fn load(r: &mut R) -> Result<Shm, String> {
        let mut shm = Shm {
            next: r.u()? as i32,
            ..Shm::default()
        };
        for _ in 0..r.u()? {
            let id = r.u()? as i32;
            let mut s = Segment {
                key: r.u()? as u32 as i32,
                size: r.u()?,
                mode: r.u()? as u32,
                uid: r.u()? as u32,
                gid: r.u()? as u32,
                cuid: r.u()? as u32,
                cgid: r.u()? as u32,
                cpid: r.u()? as i64,
                lpid: r.u()? as i64,
                nattch: r.u()?,
                removed: r.u()? != 0,
                atime: r.u()?,
                dtime: r.u()?,
                ctime: r.u()?,
                frames: Vec::new(),
            };
            for _ in 0..r.u()? {
                s.frames.push(r.u()?);
            }
            // No frames yet (never attached), or one for each of its pages (`Shm::frames` are
            // checked against the frame pool by the restore)
            if s.size == 0
                || s.size > SHMMAX
                || !(s.frames.is_empty() || s.frames.len() as u64 == page_up(s.size) / PAGE)
            {
                return Err("bad shared memory segment".into());
            }
            shm.segs.insert(id, s);
        }
        for _ in 0..r.u()? {
            let (pid, at, id, len) = (r.u()? as i64, r.u()?, r.u()? as i32, r.u()?);
            shm.attached.insert((pid, at), (id, len));
        }
        Ok(shm)
    }
}

impl Process {
    /// shmget(key, size, flags)
    pub(crate) fn sys_shmget(&mut self, key: i32, size: u64, flags: u64) -> Ret {
        let v = Ret::Val;
        if key != IPC_PRIVATE {
            if let Some((&id, s)) = self.shm.segs.iter().find(|(_, s)| s.key == key && !s.removed) {
                if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                    return v(-EEXIST);
                }
                if size > s.size {
                    return v(-EINVAL);
                }
                return v(id as i64);
            }
            if flags & IPC_CREAT == 0 {
                return v(-ENOENT);
            }
        }
        if size == 0 || size > SHMMAX {
            return v(-EINVAL);
        }
        if self.shm.segs.len() >= SHMMNI {
            return v(-ENOSPC);
        }
        // Ids aren't reused soon, so a stale one is an error rather than another segment
        let id = (self.shm.next..)
            .chain(0..self.shm.next)
            .find(|i| !self.shm.segs.contains_key(i))
            .unwrap_or(0);
        self.shm.next = id.wrapping_add(1).max(0);
        let c = self.p.creds;
        let now = self.now_secs();
        self.shm.segs.insert(
            id,
            Segment {
                key,
                size,
                mode: flags as u32 & 0o777,
                uid: c.euid(),
                gid: c.egid(),
                cuid: c.euid(),
                cgid: c.egid(),
                cpid: self.p.pid,
                ctime: now,
                ..Segment::default()
            },
        );
        v(id as i64)
    }

    /// shmat(id, addr, flags): where the segment is mapped
    pub(crate) fn sys_shmat(&mut self, id: i32, addr: u64, flags: u64) -> Ret {
        let v = Ret::Val;
        let Some(s) = self.shm.segs.get(&id) else {
            return v(-EINVAL);
        };
        let len = page_up(s.size);
        // An address given is where it goes, as MAP_FIXED, but within the address space and (as
        // Linux has it) over nothing already mapped unless SHM_REMAP
        let fixed = (addr != 0).then_some(if flags & SHM_RND != 0 { addr & !(SHMLBA - 1) } else { addr });
        if let Some(at) = fixed {
            if !at.is_multiple_of(PAGE)
                || at < PAGE
                || at.checked_add(len).is_none_or(|e| e > VA_END)
                || (flags & SHM_REMAP == 0 && self.p.mm.borrow().overlaps(at, at + len))
            {
                return v(-EINVAL);
            }
        }
        let mut frames = s.frames.clone();
        if frames.is_empty() {
            let mut f = self.frames.borrow_mut();
            for _ in 0..len / PAGE {
                match f.alloc() {
                    Some(pa) => frames.push(pa),
                    None => {
                        for pa in frames {
                            f.decref(pa);
                        }
                        return v(-ENOMEM);
                    },
                }
            }
        }
        let at = match fixed {
            Some(at) => at,
            None => match self.p.mm.borrow_mut().alloc(len) {
                Some(at) => at,
                None => return v(-ENOMEM),
            },
        };
        let prot = if flags & SHM_RDONLY != 0 { PROT_READ } else { PROT_READ | PROT_WRITE };
        self.map_area(at, at + len, prot, true, None);
        let mapped = {
            let mut f = self.frames.borrow_mut();
            self.p.mm.borrow_mut().map_frames(&mut f, at, &frames, prot)
        };
        if !mapped {
            let _ = self.sys_munmap(at, len);
            return v(-ENOMEM);
        }
        let (now, pid) = (self.now_secs(), self.p.pid);
        let s = self.shm.segs.get_mut(&id).expect("looked up above");
        s.frames = frames;
        s.nattch += 1;
        s.atime = now;
        s.lpid = pid;
        self.shm.attached.insert((pid, at), (id, len));
        v(at as i64)
    }

    /// shmdt(addr)
    pub(crate) fn sys_shmdt(&mut self, addr: u64) -> Ret {
        let Some((id, len)) = self.shm.attached.remove(&(self.p.pid, addr)) else {
            return Ret::Val(-EINVAL);
        };
        let _ = self.sys_munmap(addr, len);
        let (now, pid) = (self.now_secs(), self.p.pid);
        if let Some(s) = self.shm.segs.get_mut(&id) {
            s.nattch = s.nattch.saturating_sub(1);
            s.dtime = now;
            s.lpid = pid;
        }
        self.shm_reap(id);
        Ret::Val(0)
    }

    /// shmctl(id, cmd, buf)
    pub(crate) fn sys_shmctl(&mut self, id: i32, cmd: u64, buf: u64) -> Ret {
        let v = Ret::Val;
        let cmd = cmd & !IPC_64;
        match cmd {
            IPC_INFO | SHM_INFO => {
                // Limits (IPC_INFO) or use (SHM_INFO): zeros say nothing is in the way
                if buf != 0 && !self.write(buf, &[0u8; 128]) {
                    return v(-EFAULT);
                }
                return v(self.shm.segs.keys().max().map_or(0, |&m| m as i64));
            },
            _ => {},
        }
        let c = self.p.creds;
        let Some(s) = self.shm.segs.get_mut(&id) else {
            return v(-EINVAL);
        };
        let owner = c.euid() == 0 || c.euid() == s.uid || c.euid() == s.cuid;
        match cmd {
            IPC_STAT | SHM_STAT | SHM_STAT_ANY => {
                let s = s.clone();
                if !self.write(buf, &shmid_ds(&s)) {
                    return v(-EFAULT);
                }
                v(if cmd == IPC_STAT { 0 } else { id as i64 })
            },
            IPC_SET => {
                if !owner {
                    return v(-EPERM);
                }
                let Some(b) = self.bytes_at(buf, 24) else {
                    return v(-EFAULT);
                };
                let u32_at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
                let now = self.now_secs();
                let s = self.shm.segs.get_mut(&id).expect("looked up above");
                s.uid = u32_at(4);
                s.gid = u32_at(8);
                s.mode = u32_at(20) & 0o777;
                s.ctime = now;
                v(0)
            },
            IPC_RMID => {
                if !owner {
                    return v(-EPERM);
                }
                s.removed = true;
                s.key = IPC_PRIVATE;
                self.shm_reap(id);
                v(0)
            },
            _ if !owner => v(-EACCES),
            _ => v(-EINVAL),
        }
    }

    /// A removed segment with nothing attached goes, and its hold on its frames with it.
    fn shm_reap(&mut self, id: i32) {
        if !self.shm.segs.get(&id).is_some_and(|s| s.removed && s.nattch == 0) {
            return;
        }
        let s = self.shm.segs.remove(&id).expect("looked up above");
        let mut f = self.frames.borrow_mut();
        for pa in s.frames {
            f.decref(pa);
        }
        f.reap();
    }
}

/// struct shmid64_ds, arm64's: ipc64_perm (key, uid, gid, cuid, cgid, mode, seq), then size,
/// times, creator and last pids and the attach count.
fn shmid_ds(s: &Segment) -> [u8; 112] {
    let mut b = [0u8; 112];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &s.key.to_le_bytes());
    put(4, &s.uid.to_le_bytes());
    put(8, &s.gid.to_le_bytes());
    put(12, &s.cuid.to_le_bytes());
    put(16, &s.cgid.to_le_bytes());
    put(20, &(s.mode | if s.removed { 0o1000 } else { 0 }).to_le_bytes());
    put(48, &s.size.to_le_bytes());
    put(56, &s.atime.to_le_bytes());
    put(64, &s.dtime.to_le_bytes());
    put(72, &s.ctime.to_le_bytes());
    put(80, &(s.cpid as i32).to_le_bytes());
    put(84, &(s.lpid as i32).to_le_bytes());
    put(88, &s.nattch.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(size: u64, frames: Vec<u64>) -> Result<Shm, String> {
        let mut shm = Shm::default();
        shm.segs.insert(
            0,
            Segment {
                size,
                frames,
                ..Segment::default()
            },
        );
        let mut w = W(Vec::new());
        shm.save(&mut w);
        Shm::load(&mut R(&w.0))
    }

    #[test]
    fn a_restored_segment_has_a_frame_for_each_of_its_pages_or_none() {
        assert!(with(PAGE + 1, vec![]).is_ok());
        assert!(with(PAGE + 1, vec![0, PAGE]).is_ok());
        assert!(with(PAGE, vec![0, PAGE]).is_err());
        assert!(with(0, vec![]).is_err());
        assert!(with(u64::MAX, vec![]).is_err());
    }
}
