//! Where a native host keeps the decompressed contents of the filesystem's blobs: one unlinked
//! temporary file, written once per blob and read with pread, not a copy of every blob in the
//! heap (a container image's files are mostly never read, and what is is copied into frames).
//!
//! Wasm has no file system, and a temporary directory that is memory (tmpfs) would only move the
//! bytes out of the process's own accounting: there the blobs stay in the heap (`Spilled::Memory`).

/// Where a blob lies in the file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Extent {
    pub off: u64,
    pub len: u64,
}

/// What `Spill::keep_zstd` did.
#[cfg_attr(
    not(all(unix, not(target_arch = "wasm32"))),
    allow(
        dead_code,
        reason = "only the native spill holds or finds corrupt data"
    )
)]
#[derive(Debug, PartialEq)]
pub enum Spilled {
    /// Decompressed into the file
    Held(Extent),
    /// Not zstd data
    Corrupt,
    /// Not in the file (there is none, or it could not take the blob): decompress it in memory
    Memory,
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
pub use native::Spill;
#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
pub use none::Spill;

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod native {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{FileExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{Extent, Spilled};
    use crate::zstd::{self, Ended};

    /// `f_type` of the file systems that are memory: tmpfs and ramfs.
    #[cfg(target_os = "linux")]
    const MEMORY_FS: [u64; 2] = [0x0102_1994, 0x8584_58f6];

    /// The file blobs are appended to, made when the first one comes. Once it cannot take a blob
    /// (the disk is full) nothing more is appended; what is in it stays readable.
    pub struct Spill {
        /// Where the file is made: the temporary directory, unless a test says otherwise
        dir: Option<PathBuf>,
        file: Option<File>,
        tried: bool,
        stopped: bool,
        /// Bytes of blobs in the file: where the next one goes
        end: u64,
        /// The most bytes the file may hold (a write past it fails as a full disk does)
        limit: u64,
        /// Whether the file is made without asking what file system the directory is on (tests)
        forced: bool,
    }

    impl Spill {
        pub fn new() -> Spill {
            Spill {
                dir: None,
                file: None,
                tried: false,
                stopped: false,
                end: 0,
                limit: u64::MAX,
                forced: false,
            }
        }

        /// A spill of a file in `dir` that holds at most `limit` bytes.
        #[cfg(test)]
        pub fn in_dir(dir: &Path, limit: u64) -> Spill {
            Spill {
                dir: Some(dir.to_path_buf()),
                limit,
                ..Spill::new()
            }
        }

        /// Like `in_dir`, but the file is made whatever file system the directory is on.
        #[cfg(test)]
        pub fn in_dir_forced(dir: &Path, limit: u64) -> Spill {
            Spill {
                forced: true,
                ..Spill::in_dir(dir, limit)
            }
        }

        /// A spill that keeps no blob: they stay in memory.
        #[cfg(test)]
        pub fn off() -> Spill {
            Spill {
                stopped: true,
                ..Spill::new()
            }
        }

        /// A file whose contents are gone (the disk took them back): what is read from it fails.
        #[cfg(test)]
        pub fn lose_contents(&self) {
            if let Some(f) = &self.file {
                f.set_len(0).unwrap();
            }
        }

        /// Forget every blob: the file is emptied.
        pub fn clear(&mut self) {
            if let Some(f) = &self.file {
                let _ = f.set_len(0);
            }
            self.end = 0;
        }

        /// The file, made now if it is not yet. None when there is none: it cannot be made, or the
        /// directory is memory.
        fn file(&mut self) -> Option<&File> {
            if self.stopped {
                return None;
            }
            if !self.tried {
                self.tried = true;
                let dir = self.dir.clone().unwrap_or_else(std::env::temp_dir);
                if !self.forced && memory_or_unknown(&dir) {
                    self.stopped = true;
                    return None;
                }
                match create(&dir) {
                    Ok(f) => self.file = Some(f),
                    Err(e) => {
                        self.stop(&format!("cannot make a file in {}: {e}", dir.display()));
                        return None;
                    },
                }
            }
            self.file.as_ref()
        }

        /// No more blobs go to the file (once, with the reason).
        fn stop(&mut self, why: &str) {
            if !self.stopped {
                self.stopped = true;
                eprintln!("armless: file contents stay in memory: {why}");
            }
        }

        /// `raw` appended to the file, or None (it stays in memory).
        pub fn keep(&mut self, raw: &[u8]) -> Option<Extent> {
            let (start, limit) = (self.end, self.limit);
            let file = self.file()?;
            match put(file, start, limit, raw) {
                Ok(()) => {
                    self.end = start + raw.len() as u64;
                    Some(Extent {
                        off: start,
                        len: raw.len() as u64,
                    })
                },
                Err(e) => {
                    self.give_up(start, &e);
                    None
                },
            }
        }

        /// The contents of zstd data `data` appended to the file, decompressed in pieces that are
        /// written as they come. The blob is part of the file only once all of it is written.
        pub fn keep_zstd(&mut self, data: &[u8]) -> Spilled {
            let (start, limit) = (self.end, self.limit);
            let Some(file) = self.file() else {
                return Spilled::Memory;
            };
            let mut at = start;
            let mut failed = None;
            let ended = zstd::decompress_each(data, &mut |piece| {
                match put(file, at, limit, piece) {
                    Ok(()) => at += piece.len() as u64,
                    Err(e) => failed = Some(e),
                }
                failed.is_none()
            });
            match (ended, failed) {
                (Ended::Done, None) => {
                    self.end = at;
                    Spilled::Held(Extent {
                        off: start,
                        len: at - start,
                    })
                },
                (Ended::Corrupt, None) => {
                    self.rewind(start);
                    Spilled::Corrupt
                },
                (_, e) => {
                    self.give_up(start, &e.unwrap_or_else(|| io::Error::other("stopped")));
                    Spilled::Memory
                },
            }
        }

        /// A write failed: what was written of the blob is dropped, and nothing more is appended.
        fn give_up(&mut self, start: u64, e: &io::Error) {
            self.rewind(start);
            self.stop(&format!("writing the spill file failed: {e}"));
        }

        /// The file ends at `end` again (the disk blocks of a blob that was not kept are freed).
        fn rewind(&self, end: u64) {
            if let Some(f) = &self.file {
                let _ = f.set_len(end);
            }
        }

        /// `dst.len()` bytes of the blob at `e`, from `at` in it.
        pub fn read_at(&self, e: Extent, at: u64, dst: &mut [u8]) -> io::Result<()> {
            let file = self
                .file
                .as_ref()
                .ok_or_else(|| io::Error::other("no spill file"))?;
            if at
                .checked_add(dst.len() as u64)
                .is_none_or(|end| end > e.len)
            {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            file.read_exact_at(dst, e.off + at)
        }
    }

    /// `bytes` at `at`, if that stays within `limit` bytes of file.
    fn put(file: &File, at: u64, limit: u64, bytes: &[u8]) -> io::Result<()> {
        if at.saturating_add(bytes.len() as u64) > limit {
            return Err(io::Error::from_raw_os_error(libc::ENOSPC));
        }
        file.write_all_at(bytes, at)
    }

    /// A file of mode 0600 in `dir` that has no name: made new (never one that was there) and
    /// unlinked at once.
    fn create(dir: &Path) -> io::Result<File> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = dir.join(format!(
            ".armless-spill-{}-{nanos:x}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        std::fs::remove_file(&path)?;
        Ok(file)
    }

    /// Whether `dir` is on a file system that is memory, where a file would take as much of it as
    /// the heap does, or one that cannot be told (the probe failed): only a file system known to be
    /// disk enables the spill, so an unknown one keeps the heap cache and its eviction limit.
    #[cfg(target_os = "linux")]
    fn memory_or_unknown(dir: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else {
            return true;
        };
        let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `path` is a NUL-terminated string and `st` has the room statfs writes; it is read
        // only when statfs returned 0, which is when it has filled it in.
        let st = unsafe {
            if libc::statfs(path.as_ptr(), st.as_mut_ptr()) != 0 {
                return true;
            }
            st.assume_init()
        };
        // f_type is 32 bits wide in the kernel, whatever the C type is
        MEMORY_FS.contains(&(st.f_type as u64 & 0xffff_ffff))
    }

    #[cfg(not(target_os = "linux"))]
    fn memory_or_unknown(_dir: &Path) -> bool {
        false
    }
}

#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
mod none {
    use std::io;

    use super::{Extent, Spilled};

    /// No file to spill to: every blob stays in memory.
    pub struct Spill;

    impl Spill {
        pub fn new() -> Spill {
            Spill
        }
        pub fn clear(&mut self) {}
        #[cfg(test)]
        pub fn off() -> Spill {
            Spill
        }
        pub fn keep(&mut self, _raw: &[u8]) -> Option<Extent> {
            None
        }
        pub fn keep_zstd(&mut self, _data: &[u8]) -> Spilled {
            Spilled::Memory
        }
        pub fn read_at(&self, _e: Extent, _at: u64, _dst: &mut [u8]) -> io::Result<()> {
            Err(io::Error::other("no spill file"))
        }
    }
}
