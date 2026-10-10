//! virtio-9p: a 9P2000.L server over [`Fs`], for Linux's `trans=virtio`.
//!
//! A request that needs a file's contents the host has not delivered yet is
//! parked (its descriptor chain held) and the blob requested; it is answered
//! when the blob arrives.

use std::collections::{BTreeMap, HashSet};

use super::fs::{self, Fail, Fs, Ino, Res};
use super::virtio::{Chain, Transport};
use super::{Event, Io};
use crate::ser::{R, W};

const F_MOUNT_TAG: u64 = 1 << 0;
const MAX_MSIZE: u32 = 512 * 1024;

// Message types (T = request, R = T + 1)
const TLERROR: u8 = 6;
const TSTATFS: u8 = 8;
const TLOPEN: u8 = 12;
const TLCREATE: u8 = 14;
const TSYMLINK: u8 = 16;
const TMKNOD: u8 = 18;
const TRENAME: u8 = 20;
const TREADLINK: u8 = 22;
const TGETATTR: u8 = 24;
const TSETATTR: u8 = 26;
const TXATTRWALK: u8 = 30;
const TXATTRCREATE: u8 = 32;
const TREADDIR: u8 = 40;
const TFSYNC: u8 = 50;
const TLOCK: u8 = 52;
const TGETLOCK: u8 = 54;
const TLINK: u8 = 70;
const TMKDIR: u8 = 72;
const TRENAMEAT: u8 = 74;
const TUNLINKAT: u8 = 76;
const TVERSION: u8 = 100;
const TATTACH: u8 = 104;
const TFLUSH: u8 = 108;
const TWALK: u8 = 110;
const TREAD: u8 = 116;
const TWRITE: u8 = 118;
const TCLUNK: u8 = 120;
const TREMOVE: u8 = 122;

const O_TRUNC: u32 = 0o1000;
const AT_REMOVEDIR: u32 = 0x200;

#[derive(Clone, Debug)]
struct Fid {
    ino: Ino,
    open: bool,
}

struct Parked {
    chain: Chain,
    request: Vec<u8>,
    blob: String,
}

pub struct P9 {
    tag: Vec<u8>,
    pub fs: Fs,
    fids: BTreeMap<u32, Fid>,
    msize: u32,
    parked: Vec<Parked>,
    /// Blobs requested from the host and not yet answered.
    requested: HashSet<String>,
}

/// Request reader.
struct Req<'a>(R<'a>);

impl<'a> Req<'a> {
    fn u8(&mut self) -> Res<u8> {
        self.0.u8().ok_or(Fail::Errno(fs::EINVAL))
    }
    fn u16(&mut self) -> Res<u16> {
        self.0.u16().ok_or(Fail::Errno(fs::EINVAL))
    }
    fn u32(&mut self) -> Res<u32> {
        self.0.u32().ok_or(Fail::Errno(fs::EINVAL))
    }
    fn u64(&mut self) -> Res<u64> {
        self.0.u64().ok_or(Fail::Errno(fs::EINVAL))
    }
    fn str(&mut self) -> Res<&'a [u8]> {
        let n = self.u16()? as usize;
        self.0.take(n).ok_or(Fail::Errno(fs::EINVAL))
    }
    fn take(&mut self, n: usize) -> Res<&'a [u8]> {
        self.0.take(n).ok_or(Fail::Errno(fs::EINVAL))
    }
}

fn put_str(w: &mut W, s: &[u8]) {
    let s = &s[..s.len().min(u16::MAX as usize)];
    w.u16(s.len() as u16);
    w.0.extend_from_slice(s);
}

impl P9 {
    pub const FEATURES: u64 = F_MOUNT_TAG;

    pub fn new(tag: &str, fs: Fs) -> P9 {
        P9 {
            tag: tag.as_bytes().to_vec(),
            fs,
            fids: BTreeMap::new(),
            msize: 8192,
            parked: Vec::new(),
            requested: HashSet::new(),
        }
    }

    pub fn config_read(&self, off: u64) -> u8 {
        let off = off as usize;
        match off {
            0 => self.tag.len() as u8,
            1 => (self.tag.len() >> 8) as u8,
            _ => self.tag.get(off - 2).copied().unwrap_or(0),
        }
    }

    pub fn reset(&mut self) {
        for (_, f) in std::mem::take(&mut self.fids) {
            self.fs.drop_ref(f.ino);
        }
        self.parked.clear();
    }

    pub fn notify(&mut self, io: &Io, t: &mut Transport, q: usize) {
        if q != 0 {
            return;
        }
        while let Some(chain) = t.queues[0].pop(io) {
            let request = chain.read_all(io);
            self.serve(io, t, chain, request);
        }
    }

    fn serve(&mut self, io: &Io, t: &mut Transport, chain: Chain, request: Vec<u8>) {
        if request.len() >= 9 && request[4] == TFLUSH {
            // Requests are answered in order, except parked ones: answer the
            // flushed one (interrupted) so its buffers go back to the driver.
            let old = u16::from_le_bytes([request[7], request[8]]);
            if let Some(pos) = self.parked.iter().position(|p| {
                p.request.len() >= 7 && u16::from_le_bytes([p.request[5], p.request[6]]) == old
            }) {
                let p = self.parked.remove(pos);
                let mut w = W::new();
                w.u32(11);
                w.u8(TLERROR + 1);
                w.u16(old);
                w.u32(4); // EINTR
                let n = p.chain.write_all(io, &w.0);
                t.complete(io, 0, p.chain.head, n as u32);
            }
        }
        match self.handle(io, &request) {
            Ok(response) => {
                let n = chain.write_all(io, &response);
                t.complete(io, 0, chain.head, n as u32);
            },
            Err(blob) => {
                if self.requested.insert(blob.clone()) {
                    io.out.push(Event::Blob(blob.clone()));
                }
                self.parked.push(Parked {
                    chain,
                    request,
                    blob,
                });
            },
        }
    }

    /// The host delivered (or failed to deliver) a blob: answer what waited.
    pub fn blob_arrived(&mut self, io: &Io, t: &mut Transport, name: &str) {
        self.requested.remove(name);
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.parked)
            .into_iter()
            .partition(|p| p.blob == name);
        self.parked = waiting;
        for p in ready {
            self.serve(io, t, p.chain, p.request);
        }
    }

    /// Blobs that parked requests wait for (to ask again after a restore).
    pub fn wanted_blobs(&mut self) -> Vec<String> {
        let mut v: Vec<String> = self.parked.iter().map(|p| p.blob.clone()).collect();
        v.sort();
        v.dedup();
        self.requested = v.iter().cloned().collect();
        v
    }

    /// A full response message, or the blob the request waits on.
    fn handle(&mut self, io: &Io, msg: &[u8]) -> Result<Vec<u8>, String> {
        if msg.len() < 7 {
            return Ok(Vec::new());
        }
        let ty = msg[4];
        let tag = u16::from_le_bytes([msg[5], msg[6]]);
        let mut body = W::new();
        let result = self.dispatch(io, ty, tag, Req(R(&msg[7..])), &mut body);
        let (rty, body) = match result {
            Ok(()) => (ty + 1, body.0),
            Err(Fail::Need(blob)) => return Err(blob),
            Err(Fail::Errno(e)) => {
                let mut w = W::new();
                w.u32(e);
                (TLERROR + 1, w.0)
            },
        };
        let mut out = W::new();
        out.u32(7 + body.len() as u32);
        out.u8(rty);
        out.u16(tag);
        out.0.extend_from_slice(&body);
        Ok(out.0)
    }

    fn fid(&self, fid: u32) -> Res<&Fid> {
        self.fids.get(&fid).ok_or(Fail::Errno(fs::EBADF))
    }

    fn set_fid(&mut self, fid: u32, ino: Ino) {
        self.fs.add_ref(ino);
        if let Some(old) = self.fids.insert(fid, Fid { ino, open: false }) {
            self.fs.drop_ref(old.ino);
        }
    }

    fn clunk(&mut self, fid: u32) {
        if let Some(f) = self.fids.remove(&fid) {
            self.fs.drop_ref(f.ino);
        }
    }

    fn qid(&self, w: &mut W, ino: Ino) -> Res<()> {
        let i = self.fs.inode(ino)?;
        w.u8(i.qid_type());
        w.u32(0);
        w.u64(ino);
        Ok(())
    }

    fn dispatch(&mut self, io: &Io, ty: u8, _tag: u16, mut r: Req, w: &mut W) -> Res<()> {
        let now = io.now();
        match ty {
            TVERSION => {
                let msize = r.u32()?;
                let version = r.str()?;
                self.msize = msize.clamp(4096, MAX_MSIZE);
                for (_, f) in std::mem::take(&mut self.fids) {
                    self.fs.drop_ref(f.ino);
                }
                w.u32(self.msize);
                put_str(
                    w,
                    if version == b"9P2000.L" { b"9P2000.L" } else { b"unknown" },
                );
            },
            TATTACH => {
                let fid = r.u32()?;
                self.set_fid(fid, fs::ROOT);
                self.qid(w, fs::ROOT)?;
            },
            TFLUSH => {}, // see serve()
            TWALK => {
                let fid = r.u32()?;
                let newfid = r.u32()?;
                let n = r.u16()?;
                let mut ino = self.fid(fid)?.ino;
                let mut qids = W::new();
                let mut count = 0u16;
                for i in 0..n {
                    let name = r.str()?;
                    match self.fs.lookup(ino, name) {
                        Ok(next) => {
                            ino = next;
                            self.qid(&mut qids, ino)?;
                            count += 1;
                        },
                        Err(e) if i == 0 => return Err(e),
                        Err(_) => break,
                    }
                }
                if count == n {
                    self.set_fid(newfid, ino);
                }
                w.u16(count);
                w.0.extend_from_slice(&qids.0);
            },
            TLOPEN => {
                let fid = r.u32()?;
                let flags = r.u32()?;
                let ino = self.fid(fid)?.ino;
                if flags & O_TRUNC != 0 && !self.fs.inode(ino)?.is_dir() {
                    self.fs.truncate(ino, 0)?;
                }
                self.fids.get_mut(&fid).unwrap().open = true;
                self.qid(w, ino)?;
                w.u32(self.msize - 24);
            },
            TLCREATE => {
                let fid = r.u32()?;
                let name = r.str()?;
                let flags = r.u32()?;
                let mode = r.u32()?;
                let gid = r.u32()?;
                let _ = flags;
                let dir = self.fid(fid)?.ino;
                let uid = self.fs.inode(dir)?.uid;
                let ino =
                    self.fs
                        .create(dir, name, fs::S_IFREG | (mode & 0o7777), uid, gid, now)?;
                self.set_fid(fid, ino);
                self.fids.get_mut(&fid).unwrap().open = true;
                self.qid(w, ino)?;
                w.u32(self.msize - 24);
            },
            TSYMLINK => {
                let fid = r.u32()?;
                let name = r.str()?;
                let target = r.str()?;
                let gid = r.u32()?;
                let dir = self.fid(fid)?.ino;
                let uid = self.fs.inode(dir)?.uid;
                let ino = self.fs.symlink(dir, name, target, uid, gid, now)?;
                self.qid(w, ino)?;
            },
            TMKNOD => {
                let fid = r.u32()?;
                let name = r.str()?;
                let mode = r.u32()?;
                let major = r.u32()?;
                let minor = r.u32()?;
                let gid = r.u32()?;
                let dir = self.fid(fid)?.ino;
                let uid = self.fs.inode(dir)?.uid;
                let ino = self.fs.create(dir, name, mode, uid, gid, now)?;
                self.fs.get_mut(ino).unwrap().rdev = (major as u64) << 8 | minor as u64;
                self.qid(w, ino)?;
            },
            TMKDIR => {
                let fid = r.u32()?;
                let name = r.str()?;
                let mode = r.u32()?;
                let gid = r.u32()?;
                let dir = self.fid(fid)?.ino;
                let uid = self.fs.inode(dir)?.uid;
                let ino =
                    self.fs
                        .create(dir, name, fs::S_IFDIR | (mode & 0o7777), uid, gid, now)?;
                self.qid(w, ino)?;
            },
            TREADLINK => {
                let ino = self.fid(r.u32()?)?.ino;
                put_str(w, &self.fs.readlink(ino)?);
            },
            TGETATTR => {
                let ino = self.fid(r.u32()?)?.ino;
                let i = self.fs.inode(ino)?.clone();
                w.u64(0x7ff); // P9_GETATTR_BASIC
                self.qid(w, ino)?;
                w.u32(i.mode);
                w.u32(i.uid);
                w.u32(i.gid);
                w.u64(i.nlink as u64);
                w.u64(i.rdev);
                w.u64(i.size);
                w.u64(4096);
                w.u64(i.size.div_ceil(512));
                for t in [i.atime, i.mtime, i.ctime, (0, 0)] {
                    w.u64(t.0);
                    w.u64(t.1 as u64);
                }
                w.u64(0); // gen
                w.u64(0); // data_version
            },
            TSETATTR => {
                let ino = self.fid(r.u32()?)?.ino;
                let valid = r.u32()?;
                let mode = r.u32()?;
                let uid = r.u32()?;
                let gid = r.u32()?;
                let size = r.u64()?;
                let atime = (r.u64()?, r.u64()? as u32);
                let mtime = (r.u64()?, r.u64()? as u32);
                if valid & 8 != 0 {
                    if self.fs.inode(ino)?.is_dir() {
                        return Err(fs::EISDIR.into());
                    }
                    self.fs.truncate(ino, size)?;
                }
                let i = self.fs.get_mut(ino).ok_or(Fail::Errno(fs::ENOENT))?;
                if valid & 1 != 0 {
                    i.mode = (i.mode & fs::S_IFMT) | (mode & 0o7777);
                }
                if valid & 2 != 0 {
                    i.uid = uid;
                }
                if valid & 4 != 0 {
                    i.gid = gid;
                }
                if valid & 16 != 0 {
                    i.atime = if valid & 128 != 0 { atime } else { (now, 0) };
                }
                if valid & 32 != 0 {
                    i.mtime = if valid & 256 != 0 { mtime } else { (now, 0) };
                }
                i.ctime = (now, 0);
            },
            TXATTRWALK => {
                let fid = r.u32()?;
                self.fid(fid)?;
                return Err(fs::ENODATA.into());
            },
            TXATTRCREATE => return Err(fs::EOPNOTSUPP.into()),
            TREADDIR => {
                let ino = self.fid(r.u32()?)?.ino;
                let offset = r.u64()?;
                let count = (r.u32()? as usize).min(self.msize as usize - 11);
                let mut data = W::new();
                for (k, (name, child)) in self
                    .fs
                    .list(ino)?
                    .into_iter()
                    .enumerate()
                    .skip(offset as usize)
                {
                    let Some(ci) = self.fs.get(child) else {
                        continue;
                    };
                    let entry_len = 13 + 8 + 1 + 2 + name.len();
                    if data.0.len() + entry_len > count {
                        break;
                    }
                    data.u8(ci.qid_type());
                    data.u32(0);
                    data.u64(child);
                    data.u64(k as u64 + 1);
                    data.u8(((ci.mode >> 12) & 15) as u8);
                    put_str(&mut data, &name);
                }
                w.u32(data.0.len() as u32);
                w.0.extend_from_slice(&data.0);
            },
            TFSYNC => {
                self.fid(r.u32()?)?;
            },
            TLOCK => {
                self.fid(r.u32()?)?;
                w.u8(0); // P9_LOCK_SUCCESS
            },
            TGETLOCK => {
                self.fid(r.u32()?)?;
                let _type = r.u8()?;
                let start = r.u64()?;
                let length = r.u64()?;
                let proc_id = r.u32()?;
                let client = r.str()?.to_vec();
                w.u8(2); // F_UNLCK: no conflicting lock
                w.u64(start);
                w.u64(length);
                w.u32(proc_id);
                put_str(w, &client);
            },
            TLINK => {
                let dir = self.fid(r.u32()?)?.ino;
                let target = self.fid(r.u32()?)?.ino;
                let name = r.str()?;
                self.fs.link(dir, name, target, now)?;
            },
            TRENAME => return Err(fs::EOPNOTSUPP.into()),
            TRENAMEAT => {
                let odir = self.fid(r.u32()?)?.ino;
                let oname = r.str()?;
                let ndir = self.fid(r.u32()?)?.ino;
                let nname = r.str()?;
                self.fs.rename(odir, oname, ndir, nname, now)?;
            },
            TUNLINKAT => {
                let dir = self.fid(r.u32()?)?.ino;
                let name = r.str()?;
                let flags = r.u32()?;
                self.fs.unlink(dir, name, flags & AT_REMOVEDIR != 0, now)?;
            },
            TSTATFS => {
                self.fid(r.u32()?)?;
                let (used, files) = self.fs.statfs();
                let blocks: u64 = (used / 4096).max(1 << 20) * 2;
                w.u32(0x0102_1997); // V9FS_MAGIC
                w.u32(4096);
                w.u64(blocks);
                w.u64(blocks - used / 4096);
                w.u64(blocks - used / 4096);
                w.u64(files + (1 << 20));
                w.u64(1 << 20);
                w.u64(0);
                w.u32(255);
            },
            TREAD => {
                let ino = self.fid(r.u32()?)?.ino;
                let offset = r.u64()?;
                let count = (r.u32()? as usize).min(self.msize as usize - 11);
                if self.fs.inode(ino)?.is_dir() {
                    return Err(fs::EISDIR.into());
                }
                let data = self.fs.read(ino, offset, count)?;
                w.u32(data.len() as u32);
                w.0.extend_from_slice(&data);
            },
            TWRITE => {
                let ino = self.fid(r.u32()?)?.ino;
                let offset = r.u64()?;
                let count = r.u32()? as usize;
                let data = r.take(count)?;
                let n = self.fs.write(ino, offset, data, now)?;
                w.u32(n as u32);
            },
            TCLUNK => {
                let fid = r.u32()?;
                self.fid(fid)?;
                self.clunk(fid);
            },
            TREMOVE => {
                let fid = r.u32()?;
                self.fid(fid)?;
                self.clunk(fid);
                return Err(fs::EOPNOTSUPP.into());
            },
            _ => return Err(fs::EOPNOTSUPP.into()),
        }
        Ok(())
    }

    pub fn save(&self, w: &mut W) {
        self.fs.save(w);
        w.u32(self.msize);
        w.u32(self.fids.len() as u32);
        for (&id, f) in &self.fids {
            w.u32(id);
            w.u64(f.ino);
            w.bool(f.open);
        }
        w.u32(self.parked.len() as u32);
        for p in &self.parked {
            w.u16(p.chain.head);
            for list in [&p.chain.readable, &p.chain.writable] {
                w.u32(list.len() as u32);
                for &(a, l) in list.iter() {
                    w.u64(a);
                    w.u32(l);
                }
            }
            w.bytes(&p.request);
            w.str(&p.blob);
        }
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.fs = Fs::load(r)?;
        self.msize = r.u32()?;
        self.fids.clear();
        for _ in 0..r.u32()? {
            let id = r.u32()?;
            let ino = r.u64()?;
            let open = r.bool()?;
            self.fs.add_ref(ino);
            self.fids.insert(id, Fid { ino, open });
        }
        self.fs.collect();
        self.parked.clear();
        for _ in 0..r.u32()? {
            let mut chain = Chain {
                head: r.u16()?,
                ..Default::default()
            };
            for k in 0..2 {
                for _ in 0..r.u32()? {
                    let e = (r.u64()?, r.u32()?);
                    if k == 0 {
                        chain.readable.push(e);
                    } else {
                        chain.writable.push(e);
                    }
                }
            }
            let request = r.bytes()?.to_vec();
            let blob = r.str()?;
            self.parked.push(Parked {
                chain,
                request,
                blob,
            });
        }
        self.requested.clear();
        Some(())
    }
}
