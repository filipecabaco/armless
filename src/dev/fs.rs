//! The 9p server's filesystem: an inode tree in memory, built from v86's
//! `filesystem.json` (format 3). File contents are content-addressed blobs
//! the host fetches on first use (`Event::Blob`); files the guest writes keep
//! their contents here, and are part of snapshots.
//!
//! A large file can be a list of chunks instead of one blob: reads fetch only
//! the chunks they cover, and writes copy only the chunks they touch.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use super::json::{self, Json};
use crate::ser::{R, W};

pub type Ino = u64;
pub const ROOT: Ino = 1;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

pub const ENOENT: u32 = 2;
pub const EIO: u32 = 5;
pub const EBADF: u32 = 9;
pub const EEXIST: u32 = 17;
pub const EXDEV: u32 = 18;
pub const ENOTDIR: u32 = 20;
pub const EISDIR: u32 = 21;
pub const EINVAL: u32 = 22;
pub const EFBIG: u32 = 27;
pub const ENOTEMPTY: u32 = 39;
pub const ENODATA: u32 = 61;
pub const EOPNOTSUPP: u32 = 95;
pub const EPERM: u32 = 1;

/// Largest file the guest may grow (they live in host memory).
const MAX_FILE: u64 = 1 << 31;
/// Fetched file contents kept at most by default; older ones are fetched again
/// when needed (the guest's page cache keeps what it uses often anyway). Hosts
/// set their own budget (`DeviceConfig::fs_cache`, `Board::set_fs_cache`).
pub const DEFAULT_CACHE: usize = 256 << 20;

#[derive(Clone, Debug)]
pub enum Content {
    Dir {
        entries: BTreeMap<Vec<u8>, Ino>,
        parent: Ino,
    },
    Symlink(Vec<u8>),
    /// Contents in blob `name` (fetched lazily), `size` bytes.
    Blob(String),
    Mem(Vec<u8>),
    /// Contents in pieces, back to back; their lengths add up to `size`.
    Chunks(Vec<Chunk>),
    /// Devices, FIFOs, sockets.
    Special,
}

/// `len` bytes of a chunked file.
#[derive(Clone, Debug)]
pub struct Chunk {
    pub len: u64,
    pub data: Piece,
}

#[derive(Clone, Debug)]
pub enum Piece {
    /// The first `len` bytes of a blob (fetched lazily).
    Blob(String),
    /// Written by the guest: exactly `len` bytes.
    Mem(Vec<u8>),
}

#[derive(Clone, Debug)]
pub struct Inode {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub size: u64,
    pub rdev: u64,
    pub atime: (u64, u32),
    pub mtime: (u64, u32),
    pub ctime: (u64, u32),
    pub content: Content,
    /// References from 9p fids (not saved; recomputed from the fid table).
    pub refs: u32,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn qid_type(&self) -> u8 {
        match self.mode & S_IFMT {
            S_IFDIR => 0x80,
            S_IFLNK => 0x02,
            _ => 0,
        }
    }
}

/// Why an operation could not complete.
#[derive(Debug, PartialEq)]
pub enum Fail {
    /// errno for the guest
    Errno(u32),
    /// Waits for these blobs from the host.
    Need(Vec<String>),
}

impl From<u32> for Fail {
    fn from(e: u32) -> Fail {
        Fail::Errno(e)
    }
}

pub type Res<T> = Result<T, Fail>;

pub struct Fs {
    inodes: Vec<Option<Inode>>,
    free: Vec<Ino>,
    /// Decompressed blobs fetched so far (not saved: fetched again), oldest
    /// first in `order`, at most `max_cached` bytes.
    blobs: HashMap<String, Arc<Vec<u8>>>,
    order: VecDeque<String>,
    cached: usize,
    max_cached: usize,
    /// Blobs the host could not provide: reads fail with EIO.
    failed: HashSet<String>,
    /// Chunks a waiting read covers: kept until it completes, so a read over
    /// several chunks doesn't lose the first while it waits for the next.
    pinned: HashSet<String>,
}

impl Fs {
    /// Just an empty root directory.
    pub fn empty() -> Fs {
        let mut fs = Fs {
            inodes: vec![None],
            free: Vec::new(),
            blobs: HashMap::new(),
            order: VecDeque::new(),
            cached: 0,
            max_cached: DEFAULT_CACHE,
            failed: HashSet::new(),
            pinned: HashSet::new(),
        };
        let root = fs.alloc(Inode {
            mode: S_IFDIR | 0o755,
            uid: 0,
            gid: 0,
            nlink: 2,
            size: 4096,
            rdev: 0,
            atime: (0, 0),
            mtime: (0, 0),
            ctime: (0, 0),
            content: Content::Dir {
                entries: BTreeMap::new(),
                parent: ROOT,
            },
            refs: 0,
        });
        debug_assert_eq!(root, ROOT);
        fs
    }

    /// Build the tree described by v86's `filesystem.json`.
    pub fn from_json(text: &[u8]) -> Result<Fs, String> {
        let doc = json::parse(text)?;
        let version = doc.get("version").and_then(Json::as_f64).unwrap_or(0.0);
        if version != 3.0 {
            return Err(format!(
                "filesystem.json version {version}: only version 3 is supported"
            ));
        }
        let root = doc
            .get("fsroot")
            .and_then(Json::as_arr)
            .ok_or("filesystem.json has no fsroot")?;
        let mut fs = Fs::empty();
        fs.add_children(ROOT, root)?;
        Ok(fs)
    }

    fn add_children(&mut self, dir: Ino, list: &[Json]) -> Result<(), String> {
        for e in list {
            let a = e.as_arr().ok_or("filesystem.json: entry is not an array")?;
            let field = |i: usize| a.get(i).and_then(Json::as_f64).unwrap_or(0.0) as u64;
            let name = a
                .first()
                .and_then(Json::as_str)
                .ok_or("filesystem.json: entry without a name")?;
            let mode = field(3) as u32;
            let mtime = (field(2), 0);
            let target = a.get(6);
            let mut inode = Inode {
                mode,
                uid: field(4) as u32,
                gid: field(5) as u32,
                nlink: 1,
                size: field(1),
                rdev: 0,
                atime: mtime,
                mtime,
                ctime: mtime,
                content: Content::Special,
                refs: 0,
            };
            match mode & S_IFMT {
                S_IFDIR => {
                    inode.nlink = 2;
                    inode.size = 4096;
                    inode.content = Content::Dir {
                        entries: BTreeMap::new(),
                        parent: dir,
                    };
                },
                S_IFLNK => {
                    let t = target.and_then(Json::as_str).unwrap_or("");
                    inode.size = t.len() as u64;
                    inode.content = Content::Symlink(t.as_bytes().to_vec());
                },
                S_IFREG => {
                    inode.content = match target {
                        Some(Json::Str(blob)) if inode.size > 0 && !blob.is_empty() => {
                            Content::Blob(blob.to_string())
                        },
                        Some(Json::Arr(list)) if inode.size > 0 => {
                            Content::Chunks(Self::chunks_from_json(name, inode.size, list)?)
                        },
                        _ => {
                            inode.size = 0;
                            Content::Mem(Vec::new())
                        },
                    };
                },
                _ => {},
            }
            let is_dir = inode.is_dir();
            let ino = self.alloc(inode);
            self.link_into(dir, name.as_bytes(), ino);
            if is_dir {
                self.get_mut(dir).unwrap().nlink += 1;
                if let Some(children) = target.and_then(Json::as_arr) {
                    self.add_children(ino, children)?;
                }
            }
        }
        Ok(())
    }

    /// A chunked file's `[[blob, length], ...]`.
    fn chunks_from_json(name: &str, size: u64, list: &[Json]) -> Result<Vec<Chunk>, String> {
        let mut chunks = Vec::with_capacity(list.len());
        let mut total = 0u64;
        for c in list {
            let pair = c.as_arr().unwrap_or(&[]);
            let (Some(blob), Some(len)) = (
                pair.first().and_then(Json::as_str),
                pair.get(1).and_then(Json::as_f64),
            ) else {
                return Err(format!(
                    "filesystem.json: {name}: a chunk is not [blob, length]"
                ));
            };
            let len = len as u64;
            if len == 0 {
                continue;
            }
            total += len;
            chunks.push(Chunk {
                len,
                data: Piece::Blob(blob.to_string()),
            });
        }
        if total != size {
            return Err(format!(
                "filesystem.json: {name}: chunks hold {total} bytes, the file {size}"
            ));
        }
        Ok(chunks)
    }

    fn alloc(&mut self, inode: Inode) -> Ino {
        if let Some(i) = self.free.pop() {
            self.inodes[i as usize] = Some(inode);
            i
        } else {
            self.inodes.push(Some(inode));
            (self.inodes.len() - 1) as Ino
        }
    }

    pub fn get(&self, ino: Ino) -> Option<&Inode> {
        self.inodes.get(ino as usize)?.as_ref()
    }

    pub fn get_mut(&mut self, ino: Ino) -> Option<&mut Inode> {
        self.inodes.get_mut(ino as usize)?.as_mut()
    }

    pub fn inode(&self, ino: Ino) -> Res<&Inode> {
        self.get(ino).ok_or(Fail::Errno(ENOENT))
    }

    fn inode_mut(&mut self, ino: Ino) -> Res<&mut Inode> {
        self.get_mut(ino).ok_or(Fail::Errno(ENOENT))
    }

    fn entries(&self, dir: Ino) -> Res<&BTreeMap<Vec<u8>, Ino>> {
        match &self.inode(dir)?.content {
            Content::Dir { entries, .. } => Ok(entries),
            _ => Err(ENOTDIR.into()),
        }
    }

    fn entries_mut(&mut self, dir: Ino) -> Res<&mut BTreeMap<Vec<u8>, Ino>> {
        match &mut self.inode_mut(dir)?.content {
            Content::Dir { entries, .. } => Ok(entries),
            _ => Err(ENOTDIR.into()),
        }
    }

    fn link_into(&mut self, dir: Ino, name: &[u8], ino: Ino) {
        if let Ok(e) = self.entries_mut(dir) {
            e.insert(name.to_vec(), ino);
        }
    }

    pub fn parent(&self, dir: Ino) -> Ino {
        match self.get(dir).map(|i| &i.content) {
            Some(Content::Dir { parent, .. }) => *parent,
            _ => ROOT,
        }
    }

    pub fn lookup(&self, dir: Ino, name: &[u8]) -> Res<Ino> {
        match name {
            b"." | b"" => {
                self.entries(dir)?;
                Ok(dir)
            },
            b".." => {
                self.entries(dir)?;
                Ok(self.parent(dir))
            },
            _ => self
                .entries(dir)?
                .get(name)
                .copied()
                .ok_or(Fail::Errno(ENOENT)),
        }
    }

    /// Resolve an absolute path (following no symlinks but the last).
    pub fn lookup_path(&self, path: &str) -> Option<Ino> {
        let mut ino = ROOT;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            ino = self.lookup(ino, part.as_bytes()).ok()?;
        }
        Some(ino)
    }

    /// Directory listing including "." and "..".
    pub fn list(&self, dir: Ino) -> Res<Vec<(Vec<u8>, Ino)>> {
        let mut out = vec![(b".".to_vec(), dir), (b"..".to_vec(), self.parent(dir))];
        out.extend(self.entries(dir)?.iter().map(|(k, &v)| (k.clone(), v)));
        Ok(out)
    }

    fn check_name(name: &[u8]) -> Res<()> {
        if name.is_empty()
            || name == b"."
            || name == b".."
            || name.contains(&b'/')
            || name.len() > 255
        {
            return Err(EINVAL.into());
        }
        Ok(())
    }

    /// Create a new inode named `name` in `dir`.
    pub fn create(
        &mut self,
        dir: Ino,
        name: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
        now: u64,
    ) -> Res<Ino> {
        Self::check_name(name)?;
        if self.entries(dir)?.contains_key(name) {
            return Err(EEXIST.into());
        }
        let t = (now, 0);
        let content = match mode & S_IFMT {
            S_IFDIR => Content::Dir {
                entries: BTreeMap::new(),
                parent: dir,
            },
            S_IFREG => Content::Mem(Vec::new()),
            S_IFLNK => Content::Symlink(Vec::new()),
            _ => Content::Special,
        };
        let is_dir = mode & S_IFMT == S_IFDIR;
        let ino = self.alloc(Inode {
            mode,
            uid,
            gid,
            nlink: if is_dir { 2 } else { 1 },
            size: if is_dir { 4096 } else { 0 },
            rdev: 0,
            atime: t,
            mtime: t,
            ctime: t,
            content,
            refs: 0,
        });
        self.link_into(dir, name, ino);
        let d = self.inode_mut(dir)?;
        if is_dir {
            d.nlink += 1;
        }
        d.mtime = t;
        d.ctime = t;
        Ok(ino)
    }

    pub fn symlink(
        &mut self,
        dir: Ino,
        name: &[u8],
        target: &[u8],
        uid: u32,
        gid: u32,
        now: u64,
    ) -> Res<Ino> {
        let ino = self.create(dir, name, S_IFLNK | 0o777, uid, gid, now)?;
        let i = self.inode_mut(ino)?;
        i.size = target.len() as u64;
        i.content = Content::Symlink(target.to_vec());
        Ok(ino)
    }

    pub fn link(&mut self, dir: Ino, name: &[u8], target: Ino, now: u64) -> Res<()> {
        Self::check_name(name)?;
        if self.inode(target)?.is_dir() {
            return Err(EPERM.into());
        }
        if self.entries(dir)?.contains_key(name) {
            return Err(EEXIST.into());
        }
        self.link_into(dir, name, target);
        let i = self.inode_mut(target)?;
        i.nlink += 1;
        i.ctime = (now, 0);
        Ok(())
    }

    /// Drop an inode once nothing names or references it.
    fn maybe_free(&mut self, ino: Ino) {
        if ino == ROOT {
            return;
        }
        if let Some(i) = self.get(ino) {
            if i.nlink == 0 && i.refs == 0 {
                self.inodes[ino as usize] = None;
                self.free.push(ino);
            }
        }
    }

    pub fn add_ref(&mut self, ino: Ino) {
        if let Some(i) = self.get_mut(ino) {
            i.refs += 1;
        }
    }

    pub fn drop_ref(&mut self, ino: Ino) {
        if let Some(i) = self.get_mut(ino) {
            i.refs = i.refs.saturating_sub(1);
        }
        self.maybe_free(ino);
    }

    /// unlinkat: `rmdir` says whether the guest expects a directory.
    pub fn unlink(&mut self, dir: Ino, name: &[u8], rmdir: bool, now: u64) -> Res<()> {
        let ino = self.lookup(dir, name)?;
        if name == b"." || name == b".." {
            return Err(EINVAL.into());
        }
        let is_dir = self.inode(ino)?.is_dir();
        if rmdir && !is_dir {
            return Err(ENOTDIR.into());
        }
        if !rmdir && is_dir {
            return Err(EISDIR.into());
        }
        if is_dir && !self.entries(ino)?.is_empty() {
            return Err(ENOTEMPTY.into());
        }
        self.entries_mut(dir)?.remove(name);
        let d = self.inode_mut(dir)?;
        if is_dir {
            d.nlink -= 1;
        }
        d.mtime = (now, 0);
        d.ctime = (now, 0);
        let i = self.inode_mut(ino)?;
        i.nlink = if is_dir { 0 } else { i.nlink.saturating_sub(1) };
        i.ctime = (now, 0);
        self.maybe_free(ino);
        Ok(())
    }

    fn is_ancestor(&self, a: Ino, mut b: Ino) -> bool {
        for _ in 0..4096 {
            if a == b {
                return true;
            }
            if b == ROOT {
                return false;
            }
            b = self.parent(b);
        }
        false
    }

    pub fn rename(
        &mut self,
        odir: Ino,
        oname: &[u8],
        ndir: Ino,
        nname: &[u8],
        now: u64,
    ) -> Res<()> {
        Self::check_name(nname)?;
        let ino = self.lookup(odir, oname)?;
        if odir == ndir && oname == nname {
            return Ok(());
        }
        let is_dir = self.inode(ino)?.is_dir();
        self.entries(ndir)?;
        if is_dir && self.is_ancestor(ino, ndir) {
            return Err(EINVAL.into());
        }
        if let Ok(existing) = self.lookup(ndir, nname) {
            if existing == ino {
                // Another name of the same file: rename(2) does nothing.
                return Ok(());
            }
            let e_dir = self.inode(existing)?.is_dir();
            if e_dir != is_dir {
                return Err(if e_dir { EISDIR } else { ENOTDIR }.into());
            }
            self.unlink(ndir, nname, e_dir, now)?;
        }
        self.entries_mut(odir)?.remove(oname);
        self.link_into(ndir, nname, ino);
        if is_dir {
            if let Content::Dir { parent, .. } = &mut self.inode_mut(ino)?.content {
                *parent = ndir;
            }
            self.inode_mut(odir)?.nlink -= 1;
            self.inode_mut(ndir)?.nlink += 1;
        }
        for d in [odir, ndir] {
            let d = self.inode_mut(d)?;
            d.mtime = (now, 0);
            d.ctime = (now, 0);
        }
        self.inode_mut(ino)?.ctime = (now, 0);
        Ok(())
    }

    /// The contents of a regular file, fetching its blob first if needed.
    fn data(&mut self, ino: Ino) -> Res<&[u8]> {
        let name = match &self.inode(ino)?.content {
            Content::Blob(name) => name.clone(),
            Content::Mem(_) => {
                let Content::Mem(d) = &self.inode(ino)?.content else {
                    unreachable!()
                };
                return Ok(d);
            },
            Content::Dir { .. } => return Err(EISDIR.into()),
            _ => return Err(EINVAL.into()),
        };
        if self.failed.contains(&name) {
            return Err(EIO.into());
        }
        match self.blobs.get(&name) {
            Some(b) => Ok(b.as_slice()),
            None => Err(Fail::Need(vec![name])),
        }
    }

    /// A chunk's contents, if fetched (EIO if the host failed it, or it is short).
    fn piece<'a>(&'a self, c: &'a Chunk) -> Res<Option<&'a [u8]>> {
        match &c.data {
            Piece::Mem(d) => Ok(Some(d)),
            Piece::Blob(n) if self.failed.contains(n) => Err(EIO.into()),
            Piece::Blob(n) => match self.blobs.get(n) {
                Some(b) if b.len() as u64 >= c.len => Ok(Some(&b[..c.len as usize])),
                Some(_) => Err(EIO.into()),
                None => Ok(None),
            },
        }
    }

    /// The chunks overlapping `start..end`, as (index, offset of the chunk).
    fn chunks_in(chunks: &[Chunk], start: u64, end: u64) -> Vec<(usize, u64)> {
        let mut out = Vec::new();
        if start >= end {
            return out;
        }
        let mut pos = 0u64;
        for (k, c) in chunks.iter().enumerate() {
            if pos >= end {
                break;
            }
            if pos + c.len > start {
                out.push((k, pos));
            }
            pos += c.len;
        }
        out
    }

    /// Every chunk of `start..end` is fetched, or the ones still missing are asked
    /// for (and those already here pinned until then).
    fn need_chunks(&mut self, ino: Ino, start: u64, end: u64) -> Res<()> {
        let Content::Chunks(chunks) = &self.inode(ino)?.content else {
            return Ok(());
        };
        let mut missing = Vec::new();
        let mut covered = Vec::new();
        for (k, _) in Self::chunks_in(chunks, start, end) {
            let c = &chunks[k];
            if let Piece::Blob(n) = &c.data {
                if self.piece(c)?.is_none() && !missing.contains(n) {
                    missing.push(n.clone());
                }
                covered.push(n.clone());
            }
        }
        if missing.is_empty() {
            for n in &covered {
                self.pinned.remove(n);
            }
            Ok(())
        } else {
            self.pinned.extend(covered);
            Err(Fail::Need(missing))
        }
    }

    fn read_chunks(&mut self, ino: Ino, offset: u64, end: u64) -> Res<Vec<u8>> {
        self.need_chunks(ino, offset, end)?;
        let Content::Chunks(chunks) = &self.inode(ino)?.content else {
            unreachable!()
        };
        let mut out = Vec::with_capacity((end - offset) as usize);
        for (k, pos) in Self::chunks_in(chunks, offset, end) {
            let c = &chunks[k];
            let d = self.piece(c)?.ok_or(Fail::Errno(EIO))?;
            let a = offset.max(pos) - pos;
            let b = end.min(pos + c.len) - pos;
            out.extend_from_slice(&d[a as usize..b as usize]);
        }
        Ok(out)
    }

    /// Make the chunks of `start..end` writable (private copies of those only).
    fn materialize_chunks(&mut self, ino: Ino, start: u64, end: u64) -> Res<()> {
        self.need_chunks(ino, start, end)?;
        let Content::Chunks(chunks) = &self.inode(ino)?.content else {
            unreachable!()
        };
        let mut copies = Vec::new();
        for (k, _) in Self::chunks_in(chunks, start, end) {
            if let Piece::Blob(_) = chunks[k].data {
                let d = self.piece(&chunks[k])?.ok_or(Fail::Errno(EIO))?;
                copies.push((k, d.to_vec()));
            }
        }
        let Content::Chunks(chunks) = &mut self.inode_mut(ino)?.content else {
            unreachable!()
        };
        for (k, d) in copies {
            chunks[k].data = Piece::Mem(d);
        }
        Ok(())
    }

    /// Grow or shrink a chunked file to `size` bytes (zeros when growing).
    fn resize_chunks(chunks: &mut Vec<Chunk>, size: u64) {
        let mut pos = 0u64;
        let mut keep = 0;
        for c in chunks.iter_mut() {
            if pos >= size {
                break;
            }
            if pos + c.len > size {
                // The new end is inside this chunk: a blob keeps a prefix.
                c.len = size - pos;
                if let Piece::Mem(d) = &mut c.data {
                    d.truncate(c.len as usize);
                }
            }
            pos += c.len;
            keep += 1;
        }
        chunks.truncate(keep);
        if pos < size {
            let grow = size - pos;
            match chunks.last_mut() {
                Some(Chunk {
                    len,
                    data: Piece::Mem(d),
                }) => {
                    *len += grow;
                    d.resize(*len as usize, 0);
                },
                _ => chunks.push(Chunk {
                    len: grow,
                    data: Piece::Mem(vec![0; grow as usize]),
                }),
            }
        }
    }

    fn write_chunks(&mut self, ino: Ino, offset: u64, bytes: &[u8]) -> Res<()> {
        let end = offset + bytes.len() as u64;
        let size = self.inode(ino)?.size;
        // Fetch what is overwritten before changing anything (the write is tried again).
        self.materialize_chunks(ino, offset, end.min(size))?;
        let i = self.inode_mut(ino)?;
        let Content::Chunks(chunks) = &mut i.content else {
            unreachable!()
        };
        if end > size {
            Self::resize_chunks(chunks, end);
            i.size = end;
        }
        for (k, pos) in Self::chunks_in(chunks, offset, end) {
            let c = &mut chunks[k];
            let Piece::Mem(d) = &mut c.data else {
                unreachable!("written chunks are in memory")
            };
            let a = offset.max(pos);
            let b = end.min(pos + c.len);
            d[(a - pos) as usize..(b - pos) as usize]
                .copy_from_slice(&bytes[(a - offset) as usize..(b - offset) as usize]);
        }
        Ok(())
    }

    pub fn read(&mut self, ino: Ino, offset: u64, count: usize) -> Res<Vec<u8>> {
        let size = self.inode(ino)?.size;
        if offset >= size {
            // Reading at or past the end needs no contents.
            self.inode(ino)?;
            return Ok(Vec::new());
        }
        if let Content::Chunks(_) = self.inode(ino)?.content {
            let end = offset.saturating_add(count as u64).min(size);
            return self.read_chunks(ino, offset, end);
        }
        let data = self.data(ino)?;
        let end = (offset as usize).saturating_add(count).min(data.len());
        let start = (offset as usize).min(end);
        Ok(data[start..end].to_vec())
    }

    /// Make a file's contents writable (a private copy).
    fn materialize(&mut self, ino: Ino) -> Res<&mut Vec<u8>> {
        if let Content::Blob(_) = self.inode(ino)?.content {
            let data = self.data(ino)?.to_vec();
            self.inode_mut(ino)?.content = Content::Mem(data);
        }
        match &mut self.inode_mut(ino)?.content {
            Content::Mem(d) => Ok(d),
            Content::Dir { .. } => Err(EISDIR.into()),
            _ => Err(EINVAL.into()),
        }
    }

    pub fn write(&mut self, ino: Ino, offset: u64, bytes: &[u8], now: u64) -> Res<usize> {
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or(Fail::Errno(EFBIG))?;
        if end > MAX_FILE {
            return Err(EFBIG.into());
        }
        if let Content::Chunks(_) = self.inode(ino)?.content {
            self.write_chunks(ino, offset, bytes)?;
            let i = self.inode_mut(ino)?;
            i.mtime = (now, 0);
            i.ctime = (now, 0);
            return Ok(bytes.len());
        }
        let d = self.materialize(ino)?;
        if d.len() < end as usize {
            d.resize(end as usize, 0);
        }
        d[offset as usize..end as usize].copy_from_slice(bytes);
        let len = d.len() as u64;
        let i = self.inode_mut(ino)?;
        i.size = len;
        i.mtime = (now, 0);
        i.ctime = (now, 0);
        Ok(bytes.len())
    }

    pub fn truncate(&mut self, ino: Ino, size: u64) -> Res<()> {
        if size > MAX_FILE {
            return Err(EFBIG.into());
        }
        if size == 0 {
            if let Content::Blob(_) | Content::Chunks(_) = self.inode(ino)?.content {
                self.inode_mut(ino)?.content = Content::Mem(Vec::new());
            }
        }
        if let Content::Chunks(chunks) = &mut self.inode_mut(ino)?.content {
            Self::resize_chunks(chunks, size);
            self.inode_mut(ino)?.size = size;
            return Ok(());
        }
        let d = self.materialize(ino)?;
        d.resize(size as usize, 0);
        self.inode_mut(ino)?.size = size;
        Ok(())
    }

    pub fn readlink(&self, ino: Ino) -> Res<Vec<u8>> {
        match &self.inode(ino)?.content {
            Content::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL.into()),
        }
    }

    // ------------------------------------------------------------ blobs

    pub fn has_blob(&self, name: &str) -> bool {
        self.blobs.contains_key(name)
    }

    /// The host delivered a blob (zstd-compressed, or raw).
    pub fn provide_blob(&mut self, name: &str, data: Vec<u8>) -> Result<(), String> {
        let raw = if data.len() >= 4 && data[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
            crate::zstd::decompress(&data).ok_or_else(|| format!("{name}: corrupt zstd data"))?
        } else {
            data
        };
        self.failed.remove(name);
        if self.blobs.contains_key(name) {
            return Ok(());
        }
        self.cached += raw.len();
        self.blobs.insert(name.to_string(), Arc::new(raw));
        self.order.push_back(name.to_string());
        self.evict();
        Ok(())
    }

    /// The most decompressed file content to keep; older content goes first, and the newest
    /// always stays (a read is waiting for it).
    pub fn set_cache_limit(&mut self, bytes: usize) {
        self.max_cached = bytes;
        self.evict();
    }

    fn evict(&mut self) {
        // Pinned chunks stay, at the front, until the read waiting on them completes.
        let mut kept = Vec::new();
        while self.cached > self.max_cached && self.order.len() > 1 {
            let old = self.order.pop_front().unwrap();
            if self.pinned.contains(&old) {
                kept.push(old);
            } else if let Some(b) = self.blobs.remove(&old) {
                self.cached -= b.len();
            }
        }
        while let Some(n) = kept.pop() {
            self.order.push_front(n);
        }
    }

    pub fn blob_failed(&mut self, name: &str) {
        self.failed.insert(name.to_string());
    }

    /// Drop every fetched blob (they are fetched again when needed).
    pub fn forget_blobs(&mut self) {
        self.blobs.clear();
        self.order.clear();
        self.cached = 0;
        self.pinned.clear();
    }

    /// The blob a file's contents are in, if they are still unmodified.
    pub fn blob_of(&self, ino: Ino) -> Option<&str> {
        match &self.get(ino)?.content {
            Content::Blob(n) => Some(n),
            _ => None,
        }
    }

    /// Whole contents of a file, or the blobs it waits on.
    pub fn read_all(&mut self, ino: Ino) -> Res<Vec<u8>> {
        if let Content::Chunks(_) = self.inode(ino)?.content {
            let size = self.inode(ino)?.size;
            return self.read_chunks(ino, 0, size);
        }
        Ok(self.data(ino)?.to_vec())
    }

    pub fn statfs(&self) -> (u64, u64) {
        let used: u64 = self.inodes.iter().flatten().map(|i| i.size).sum();
        let files = self.inodes.iter().flatten().count() as u64;
        (used, files)
    }

    pub fn reset_refs(&mut self) {
        for i in self.inodes.iter_mut().flatten() {
            i.refs = 0;
        }
    }

    /// Free inodes that are neither named nor referenced (after a restore).
    pub fn collect(&mut self) {
        let ids: Vec<Ino> = (0..self.inodes.len() as Ino).collect();
        for ino in ids {
            self.maybe_free(ino);
        }
    }

    // ------------------------------------------------------------ snapshot

    pub fn save(&self, w: &mut W) {
        w.u64(self.inodes.len() as u64);
        for slot in &self.inodes {
            let Some(i) = slot else {
                w.u8(0);
                continue;
            };
            w.u8(1);
            w.u32(i.mode);
            w.u32(i.uid);
            w.u32(i.gid);
            w.u32(i.nlink);
            w.u64(i.size);
            w.u64(i.rdev);
            for t in [i.atime, i.mtime, i.ctime] {
                w.u64(t.0);
                w.u32(t.1);
            }
            match &i.content {
                Content::Dir { entries, parent } => {
                    w.u8(0);
                    w.u64(*parent);
                    w.u64(entries.len() as u64);
                    for (k, v) in entries {
                        w.bytes(k);
                        w.u64(*v);
                    }
                },
                Content::Symlink(t) => {
                    w.u8(1);
                    w.bytes(t);
                },
                Content::Blob(n) => {
                    w.u8(2);
                    w.str(n);
                },
                Content::Mem(d) => {
                    w.u8(3);
                    w.bytes(d);
                },
                Content::Special => w.u8(4),
                Content::Chunks(chunks) => {
                    // Only written chunks carry their bytes.
                    w.u8(5);
                    w.u64(chunks.len() as u64);
                    for c in chunks {
                        w.u64(c.len);
                        match &c.data {
                            Piece::Blob(n) => {
                                w.u8(0);
                                w.str(n);
                            },
                            Piece::Mem(d) => {
                                w.u8(1);
                                w.bytes(d);
                            },
                        }
                    }
                },
            }
        }
    }

    pub fn load(r: &mut R) -> Option<Fs> {
        let n = r.u64()? as usize;
        let mut fs = Fs::empty();
        fs.inodes.clear();
        for ino in 0..n {
            if r.u8()? == 0 {
                fs.inodes.push(None);
                if ino > 0 {
                    fs.free.push(ino as Ino);
                }
                continue;
            }
            let mode = r.u32()?;
            let uid = r.u32()?;
            let gid = r.u32()?;
            let nlink = r.u32()?;
            let size = r.u64()?;
            let rdev = r.u64()?;
            let mut times = [(0u64, 0u32); 3];
            for t in &mut times {
                *t = (r.u64()?, r.u32()?);
            }
            let content = match r.u8()? {
                0 => {
                    let parent = r.u64()?;
                    let count = r.u64()?;
                    let mut entries = BTreeMap::new();
                    for _ in 0..count {
                        let k = r.bytes()?.to_vec();
                        entries.insert(k, r.u64()?);
                    }
                    Content::Dir { entries, parent }
                },
                1 => Content::Symlink(r.bytes()?.to_vec()),
                2 => Content::Blob(r.str()?),
                3 => Content::Mem(r.bytes()?.to_vec()),
                5 => {
                    let mut chunks = Vec::new();
                    for _ in 0..r.u64()? {
                        let len = r.u64()?;
                        let data = match r.u8()? {
                            0 => Piece::Blob(r.str()?),
                            1 => Piece::Mem(r.bytes()?.to_vec()),
                            _ => return None,
                        };
                        if matches!(&data, Piece::Mem(d) if d.len() as u64 != len) {
                            return None;
                        }
                        chunks.push(Chunk { len, data });
                    }
                    Content::Chunks(chunks)
                },
                _ => Content::Special,
            };
            fs.inodes.push(Some(Inode {
                mode,
                uid,
                gid,
                nlink,
                size,
                rdev,
                atime: times[0],
                mtime: times[1],
                ctime: times[2],
                content,
                refs: 0,
            }));
        }
        fs.get(ROOT)?;
        Some(fs)
    }
}

#[cfg(test)]
mod cache_tests {
    use super::Fs;

    #[test]
    fn the_cache_stays_within_its_budget() {
        let mut fs = Fs::empty();
        fs.set_cache_limit(20);
        for name in ["a", "b", "c"] {
            fs.provide_blob(name, vec![1; 8]).unwrap();
        }
        // 24 bytes is over 20: the oldest went first
        assert!(!fs.blobs.contains_key("a"));
        assert!(fs.blobs.contains_key("b") && fs.blobs.contains_key("c"));
        assert_eq!(fs.cached, 16);

        // Lowering the budget drops what is over it now; the newest always stays
        fs.set_cache_limit(0);
        assert_eq!(fs.blobs.keys().collect::<Vec<_>>(), ["c"]);
        assert_eq!(fs.cached, 8);
    }
}

#[cfg(test)]
mod chunk_tests {
    use super::*;

    /// /big: 10 bytes in chunks "a" (4), "b" (4), "c" (2); /old: one blob "o".
    fn image() -> Fs {
        let json = br#"{"version":3,"fsroot":[
            ["big",10,0,33188,0,0,[["a",4],["b",4],["c",2]]],
            ["old",3,0,33188,0,0,"o"]]}"#;
        Fs::from_json(json).unwrap()
    }

    fn provide_all(fs: &mut Fs) {
        for (n, d) in [
            ("a", &b"0123"[..]),
            ("b", b"4567"),
            ("c", b"89"),
            ("o", b"xyz"),
        ] {
            fs.provide_blob(n, d.to_vec()).unwrap();
        }
    }

    fn need(names: &[&str]) -> Fail {
        Fail::Need(names.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn reads_fetch_only_the_chunks_they_cover() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        assert_eq!(fs.inode(big).unwrap().size, 10);
        assert_eq!(fs.read(big, 0, 2), Err(need(&["a"])));
        assert_eq!(fs.read(big, 5, 2), Err(need(&["b"])));
        // Across a boundary: everything missing is asked for at once
        assert_eq!(fs.read(big, 2, 7), Err(need(&["a", "b", "c"])));
        fs.provide_blob("b", b"4567".to_vec()).unwrap();
        assert_eq!(fs.read(big, 2, 7), Err(need(&["a", "c"])));
        fs.provide_blob("a", b"0123".to_vec()).unwrap();
        assert_eq!(fs.read(big, 2, 4).unwrap(), b"2345");
        assert_eq!(fs.read(big, 2, 7), Err(need(&["c"])));
        fs.provide_blob("c", b"89".to_vec()).unwrap();
        assert_eq!(fs.read(big, 2, 100).unwrap(), b"23456789");
        assert_eq!(fs.read(big, 10, 5).unwrap(), b"");
        assert_eq!(fs.read_all(big).unwrap(), b"0123456789");
    }

    #[test]
    fn old_single_blob_files_still_work() {
        let mut fs = image();
        let old = fs.lookup_path("/old").unwrap();
        assert_eq!(fs.blob_of(old), Some("o"));
        assert_eq!(fs.read(old, 0, 10), Err(need(&["o"])));
        provide_all(&mut fs);
        assert_eq!(fs.read(old, 1, 10).unwrap(), b"yz");
        fs.write(old, 3, b"!", 1).unwrap();
        assert_eq!(fs.read_all(old).unwrap(), b"xyz!");
    }

    #[test]
    fn a_failed_or_short_chunk_is_an_io_error() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        fs.blob_failed("a");
        assert_eq!(fs.read(big, 0, 1), Err(Fail::Errno(EIO)));
        fs.provide_blob("b", b"45".to_vec()).unwrap();
        assert_eq!(fs.read(big, 4, 1), Err(Fail::Errno(EIO)));
    }

    #[test]
    fn writes_copy_only_the_chunks_they_touch() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        // Overwriting part of "b" needs only "b"
        assert_eq!(fs.write(big, 5, b"XY", 1), Err(need(&["b"])));
        fs.provide_blob("b", b"4567".to_vec()).unwrap();
        assert_eq!(fs.write(big, 5, b"XY", 1).unwrap(), 2);
        let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
            panic!()
        };
        assert!(matches!(&c[0].data, Piece::Blob(n) if n == "a"));
        assert!(matches!(&c[1].data, Piece::Mem(d) if d == b"4XY7"));
        assert!(matches!(&c[2].data, Piece::Blob(n) if n == "c"));
        assert_eq!(fs.inode(big).unwrap().mtime, (1, 0));
        // The shared blob is untouched
        provide_all(&mut fs);
        assert_eq!(fs.read_all(big).unwrap(), b"01234XY789");
        assert_eq!(&**fs.blobs.get("b").unwrap(), b"4567");
        // Across a boundary, and past the end with a hole
        fs.write(big, 3, b"ab", 2).unwrap();
        fs.write(big, 12, b"Z", 2).unwrap();
        assert_eq!(fs.inode(big).unwrap().size, 13);
        assert_eq!(fs.read_all(big).unwrap(), b"012abXY789\0\0Z");
        // Appending right at the end extends the last in-memory chunk
        fs.write(big, 13, b"!", 3).unwrap();
        let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
            panic!()
        };
        assert_eq!(c.len(), 4);
        assert_eq!(fs.read(big, 10, 10).unwrap(), b"\0\0Z!");
    }

    #[test]
    fn truncate_keeps_a_prefix_without_fetching() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        fs.truncate(big, 6).unwrap();
        let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
            panic!()
        };
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].len, 2);
        assert!(fs.blobs.is_empty());
        // Growing again shows zeros, not the rest of the blob
        fs.truncate(big, 9).unwrap();
        provide_all(&mut fs);
        assert_eq!(fs.read_all(big).unwrap(), b"012345\0\0\0");
        // A cut inside an in-memory chunk
        fs.write(big, 0, b"Q", 1).unwrap();
        fs.truncate(big, 3).unwrap();
        assert_eq!(fs.read_all(big).unwrap(), b"Q12");
        assert_eq!(fs.inode(big).unwrap().size, 3);
        fs.truncate(big, 0).unwrap();
        assert_eq!(fs.read_all(big).unwrap(), b"");
        assert!(matches!(fs.inode(big).unwrap().content, Content::Mem(_)));
    }

    #[test]
    fn hard_links_and_renames_share_the_chunks() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        fs.link(ROOT, b"also", big, 1).unwrap();
        fs.rename(ROOT, b"big", ROOT, b"moved", 1).unwrap();
        provide_all(&mut fs);
        fs.write(big, 0, b"!", 1).unwrap();
        let also = fs.lookup_path("/also").unwrap();
        assert_eq!(fs.read_all(also).unwrap(), b"!123456789");
        assert_eq!(fs.lookup_path("/moved"), Some(big));
    }

    #[test]
    fn snapshots_keep_only_written_chunks() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        provide_all(&mut fs);
        fs.write(big, 4, b"W", 1).unwrap();
        fs.truncate(big, 9).unwrap();
        let mut w = W::new();
        fs.save(&mut w);
        // "a" stays a name; only the written chunk "b" carries bytes
        assert!(w.0.windows(4).any(|s| s == b"W567"));
        assert!(!w.0.windows(4).any(|s| s == b"0123"));
        let mut back = Fs::load(&mut R(&w.0)).unwrap();
        let big2 = back.lookup_path("/big").unwrap();
        assert_eq!(back.read(big2, 0, 100), Err(need(&["a", "c"])));
        provide_all(&mut back);
        assert_eq!(back.read_all(big2).unwrap(), b"0123W5678");
    }

    #[test]
    fn snapshots_from_before_chunks_still_load() {
        // A root directory with a file in blob "o" and a written one, encoded by hand
        // the way format 3 always has.
        let mut w = W::new();
        let inode = |w: &mut W, mode: u32, nlink: u32, size: u64| {
            w.u8(1);
            for v in [mode, 0, 0, nlink] {
                w.u32(v);
            }
            w.u64(size);
            w.u64(0);
            for _ in 0..3 {
                w.u64(5);
                w.u32(0);
            }
        };
        w.u64(4);
        w.u8(0);
        inode(&mut w, S_IFDIR | 0o755, 2, 4096);
        w.u8(0);
        w.u64(ROOT);
        w.u64(2);
        w.bytes(b"new");
        w.u64(3);
        w.bytes(b"old");
        w.u64(2);
        inode(&mut w, S_IFREG | 0o644, 1, 3);
        w.u8(2);
        w.str("o");
        inode(&mut w, S_IFREG | 0o644, 1, 2);
        w.u8(3);
        w.bytes(b"hi");
        let mut fs = Fs::load(&mut R(&w.0)).unwrap();
        let old = fs.lookup_path("/old").unwrap();
        assert_eq!(fs.read(old, 0, 9), Err(need(&["o"])));
        fs.provide_blob("o", b"xyz".to_vec()).unwrap();
        assert_eq!(fs.read(old, 0, 9).unwrap(), b"xyz");
        let new = fs.lookup_path("/new").unwrap();
        assert_eq!(fs.read_all(new).unwrap(), b"hi");
    }

    #[test]
    fn chunk_lengths_must_add_up() {
        let json = br#"{"version":3,"fsroot":[["f",9,0,33188,0,0,[["a",4],["b",4]]]]}"#;
        assert!(Fs::from_json(json).err().unwrap().contains("8 bytes"));
        let json = br#"{"version":3,"fsroot":[["f",4,0,33188,0,0,[["a"]]]]}"#;
        assert!(Fs::from_json(json).is_err());
    }

    #[test]
    fn a_waiting_read_keeps_its_chunks_cached() {
        let mut fs = image();
        let big = fs.lookup_path("/big").unwrap();
        fs.set_cache_limit(0);
        assert!(fs.read(big, 0, 10).is_err());
        fs.provide_blob("a", b"0123".to_vec()).unwrap();
        fs.provide_blob("b", b"4567".to_vec()).unwrap();
        fs.provide_blob("c", b"89".to_vec()).unwrap();
        // Over budget, but the read still has everything it waited for
        assert_eq!(fs.read_all(big).unwrap(), b"0123456789");
        // Done: no longer pinned, so the next blob pushes them out
        fs.provide_blob("o", b"xyz".to_vec()).unwrap();
        assert_eq!(fs.blobs.len(), 1);
    }
}
