//! The 9p server's filesystem: an inode tree in memory, built from v86's
//! `filesystem.json` (format 3). File contents are content-addressed blobs
//! the host fetches on first use (`Event::Blob`); files the guest writes keep
//! their contents here, and are part of snapshots.

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
    /// Devices, FIFOs, sockets.
    Special,
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
    /// Waits for this blob from the host.
    Need(String),
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
                    inode.content = match target.and_then(Json::as_str) {
                        Some(blob) if inode.size > 0 && !blob.is_empty() => {
                            Content::Blob(blob.to_string())
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
            None => Err(Fail::Need(name)),
        }
    }

    pub fn read(&mut self, ino: Ino, offset: u64, count: usize) -> Res<Vec<u8>> {
        let size = self.inode(ino)?.size;
        if offset >= size {
            // Reading at or past the end needs no contents.
            self.inode(ino)?;
            return Ok(Vec::new());
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
            if let Content::Blob(_) = self.inode(ino)?.content {
                self.inode_mut(ino)?.content = Content::Mem(Vec::new());
            }
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
        while self.cached > self.max_cached && self.order.len() > 1 {
            let old = self.order.pop_front().unwrap();
            if let Some(b) = self.blobs.remove(&old) {
                self.cached -= b.len();
            }
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
    }

    /// The blob a file's contents are in, if they are still unmodified.
    pub fn blob_of(&self, ino: Ino) -> Option<&str> {
        match &self.get(ino)?.content {
            Content::Blob(n) => Some(n),
            _ => None,
        }
    }

    /// Whole contents of a file, or the blob it waits on.
    pub fn read_all(&mut self, ino: Ino) -> Res<Vec<u8>> {
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
