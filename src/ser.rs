//! Little-endian byte writer/reader for snapshots.

pub struct W(pub Vec<u8>, Option<Vec<Later>>);

/// Contents of a file the guest wrote that a snapshot's writer left out: `len` bytes that go at `at`
/// of its bytes, read from the file system (inode `ino`, or chunk `chunk` of it) when the snapshot
/// is given out (`vm::StateSaver`), so they are never held twice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Later {
    pub at: usize,
    pub ino: u64,
    pub chunk: Option<usize>,
    pub len: usize,
}

/// Contents smaller than this are copied: leaving them out saves nothing worth the bookkeeping.
const LATER_MIN: usize = 4096;

impl W {
    pub fn new() -> W {
        W(Vec::new(), None)
    }
    /// A writer that leaves the contents of the files the guest wrote out (see `contents`).
    pub fn leaving_contents() -> W {
        W(Vec::new(), Some(Vec::new()))
    }
    /// Whether `contents` of `len` bytes would be left out.
    pub fn leaves(&self, len: usize) -> bool {
        self.1.is_some() && len >= LATER_MIN
    }
    /// The contents of a file the guest wrote (inode `ino`, or chunk `chunk` of it), as `bytes`
    /// writes them; a writer made with `leaving_contents` writes their length only, and notes where
    /// they go (`later`).
    pub fn contents(&mut self, ino: u64, chunk: Option<usize>, v: &[u8]) {
        if !self.leaves(v.len()) {
            return self.bytes(v);
        }
        self.u64(v.len() as u64);
        let at = self.0.len();
        if let Some(later) = &mut self.1 {
            later.push(Later {
                at,
                ino,
                chunk,
                len: v.len(),
            });
        }
    }
    /// The contents left out, in the order they go in.
    pub fn later(&mut self) -> Vec<Later> {
        self.1.take().unwrap_or_default()
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn bool(&mut self, v: bool) {
        self.u8(v as u8);
    }
    /// Length-prefixed bytes.
    pub fn bytes(&mut self, v: &[u8]) {
        self.u64(v.len() as u64);
        self.0.extend_from_slice(v);
    }
    pub fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

impl Default for W {
    fn default() -> Self {
        W::new()
    }
}

pub struct R<'a>(pub &'a [u8]);

impl<'a> R<'a> {
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    pub fn bytes(&mut self) -> Option<&'a [u8]> {
        let n = self.u64()?;
        if n > self.0.len() as u64 {
            return None;
        }
        self.take(n as usize)
    }
    pub fn str(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
}
