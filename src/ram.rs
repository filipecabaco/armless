//! Guest RAM: one zero-filled byte buffer, and the only place that touches it through pointers.
//!
//! RAM is shared by the vCPU threads (and the device thread) the way physical memory is shared by
//! cores, so it is never handed out as a Rust slice: a `&mut [u8]` would promise the compiler that
//! nothing else writes it while it lives, which another vCPU does at any time. Everything goes
//! through the methods here instead: bounds-checked copies and loads and stores of a given width,
//! plus atomic ones for the guest's exclusives and LSE atomics. Concurrent plain accesses are the
//! guest's business, as on hardware (it orders them with barriers, acquire/release and
//! exclusives). Code that addresses RAM itself (JIT-compiled blocks, the page's wasm view) gets
//! its base from `as_ptr`.
//!
//! On wasm32 the buffer comes straight from `memory.grow`, whose pages are
//! guaranteed to be zero, so a large RAM costs nothing to set up (zeroing it
//! through the allocator took ~0.5 ms per MiB). Grown memory cannot be given
//! back, so dropping such a buffer leaks it; the wasm build creates one CPU.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

pub struct Ram {
    ptr: *mut u8,
    len: usize,
    /// What the global allocator returned, to free it with (none for memory.grow and aliases).
    alloc: Option<(*mut u8, std::alloc::Layout)>,
    /// A mapping of `len` bytes to unmap when this is dropped (`from_image`; aliases have none).
    #[cfg_attr(not(unix), allow(dead_code))]
    mapped: bool,
    /// The pages came from an image (`from_image`): a restore keeps those its snapshot lacks,
    /// rather than zeroing them.
    image: bool,
    /// Nothing has written RAM since it was allocated (shared with aliases): a restore can skip
    /// zeroing the pages its snapshot lacks. Cleared by every write here and by a CPU's `run`
    /// (compiled code stores through `as_ptr`).
    fresh: Arc<AtomicBool>,
}

pub const PAGE: usize = 4096;

// SAFETY: a Ram is a pointer and a length to memory that lives as long as its owner (aliases are
// held by things that keep the owner, the machine, alive); every access is a bounds-checked
// copy, a load or store of one width, or an atomic, so sharing it between threads is what
// sharing physical memory between cores is.
unsafe impl Send for Ram {}
// SAFETY: as for Send.
unsafe impl Sync for Ram {}

impl Ram {
    pub fn zeroed(len: usize) -> Ram {
        #[cfg(target_arch = "wasm32")]
        {
            let pages = len.div_ceil(65536).max(1);
            let old = core::arch::wasm32::memory_grow(0, pages);
            if old != usize::MAX {
                return Ram {
                    ptr: (old * 65536) as *mut u8,
                    len,
                    alloc: None,
                    mapped: false,
                    image: false,
                    fresh: Arc::new(AtomicBool::new(true)),
                };
            }
        }
        // Page-aligned, so naturally aligned guest addresses are aligned on the
        // host too (atomic accesses need that). Not by asking the allocator for
        // that alignment, though: it then zeroes every byte itself, committing
        // all of RAM up front. With its own alignment a zeroed allocation this
        // size is calloc's fresh zero pages, which cost nothing until the guest
        // touches them; one page more leaves room to align by hand.
        let layout = std::alloc::Layout::from_size_align(len.max(1) + PAGE, 8).expect("RAM size");
        // SAFETY: the layout's size is at least one page, never zero.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!raw.is_null(), "out of memory allocating guest RAM");
        Ram {
            // At most PAGE - 1 bytes in: the allocation has PAGE bytes to spare.
            ptr: raw.wrapping_add(raw.align_offset(PAGE)),
            len,
            alloc: Some((raw, layout)),
            mapped: false,
            image: false,
            fresh: Arc::new(AtomicBool::new(true)),
        }
    }

    /// RAM whose pages are an image's: `file` holds RAM as it is, `len` bytes, with holes where it
    /// is zero (`split_state` writes one). It is mapped copy-on-write, private to this RAM:
    /// machines made from one image share the pages none of them writes (the host's page cache),
    /// a page a machine writes becomes its own, and nothing is ever written to the file.
    ///
    /// A snapshot split from the image (`split_state`) restores onto it keeping its pages; any
    /// other snapshot zeroes those it lacks, as on any RAM. Pair a split snapshot with its own
    /// image: on another it would run on another machine's memory.
    ///
    /// A page no machine has written yet is still the file's, so the file must not change while
    /// it is mapped: make it read-only, where only its owner can reach it, and replace it rather
    /// than write to it. A file cut shorter than RAM would crash the machine at the first touch of
    /// a page past its end.
    #[cfg(unix)]
    pub fn from_image(file: &std::fs::File, len: usize) -> std::io::Result<Ram> {
        use std::os::fd::AsRawFd;
        let size = file.metadata()?.len();
        if size != len as u64 || len == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the RAM image is {size} bytes, RAM {len}"),
            ));
        }
        // SAFETY: a new mapping, at an address the kernel picks, of the whole file (it is `len`
        // long); nothing else owns that range, and `drop` unmaps it once.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Ram {
            ptr: ptr.cast(),
            len,
            alloc: None,
            mapped: true,
            image: true,
            fresh: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Whether the pages came from an image (`from_image`).
    pub fn is_image(&self) -> bool {
        self.image
    }

    /// Another handle on the same memory; it does not free it when dropped. Whoever holds one
    /// keeps the owner alive (the machine, through an `Arc`).
    pub fn alias(&self) -> Ram {
        Ram {
            ptr: self.ptr,
            len: self.len,
            alloc: None,
            mapped: false,
            image: self.image,
            fresh: self.fresh.clone(),
        }
    }

    /// Note that RAM may no longer be all zero.
    #[inline(always)]
    pub fn mark_written(&self) {
        if self.fresh.load(Ordering::Relaxed) {
            self.fresh.store(false, Ordering::Relaxed);
        }
    }

    /// Whether RAM is still all zero as allocated (nothing has written it), and from now on
    /// counts as written: for a restore, which then needn't zero what its snapshot lacks.
    pub fn take_fresh(&self) -> bool {
        self.fresh.swap(false, Ordering::SeqCst)
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where RAM starts, for code that addresses it itself: JIT-compiled blocks (which only
    /// access what the fast TLB maps into RAM) and the host's view of the wasm memory.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    #[inline(always)]
    fn holds(&self, at: usize, n: usize) -> bool {
        at.checked_add(n).is_some_and(|end| end <= self.len)
    }

    /// Copy RAM at `at` into `buf`. False, and nothing copied, if that is not all RAM.
    pub fn read(&self, at: usize, buf: &mut [u8]) -> bool {
        if !self.holds(at, buf.len()) {
            return false;
        }
        // SAFETY: in bounds (above); `buf` is the caller's own memory, so they don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(at), buf.as_mut_ptr(), buf.len()) };
        true
    }

    /// Copy `data` into RAM at `at`. False, and nothing written, if that is not all RAM.
    pub fn write(&self, at: usize, data: &[u8]) -> bool {
        if !self.holds(at, data.len()) {
            return false;
        }
        self.mark_written();
        // SAFETY: as in `read`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(at), data.len()) };
        true
    }

    /// Zero `n` bytes at `at`. False, and nothing written, if that is not all RAM.
    pub fn zero(&self, at: usize, n: usize) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        self.mark_written();
        // SAFETY: in bounds.
        unsafe { std::ptr::write_bytes(self.ptr.add(at), 0, n) };
        true
    }

    /// Zero `n` bytes at `at` and give the host back the memory under them where it can. False,
    /// and nothing done, if that is not all RAM.
    ///
    /// `zero` writes zeros, which keeps (or, for a page never touched, commits) the host's page.
    /// Here the whole host pages in the range are replaced with fresh anonymous ones instead,
    /// which read as zero and hold no memory until written; the guest pages at its edges, inside
    /// a host page that is bigger than the guest's (16 KiB on Apple silicon), are written with
    /// zeros. Where the host won't map over RAM (an allocation of its allocator's, on macOS)
    /// it is all written with zeros, as `zero` does.
    ///
    /// Nothing here drops what a CPU decoded or compiled from the range: discard data, not code
    /// a CPU may still run.
    pub fn discard(&self, at: usize, n: usize) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        #[cfg(unix)]
        {
            // SAFETY: sysconf has no preconditions.
            let host =
                unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(PAGE as libc::c_long) as usize;
            let (start, end) = (self.ptr as usize + at, self.ptr as usize + at + n);
            let (hs, he) = (start.next_multiple_of(host), end / host * host);
            if hs < he {
                // SAFETY: [hs, he) is whole host pages inside RAM (`holds`, above). MAP_FIXED
                // replaces just those pages with a private anonymous mapping, readable and
                // writable as RAM is, so the range stays mapped, now zero; what was there (part
                // of the allocation or the mapping RAM came from) is still freed or unmapped as
                // a whole when RAM is dropped.
                let r = unsafe {
                    libc::mmap(
                        hs as *mut libc::c_void,
                        he - hs,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANON,
                        -1,
                        0,
                    )
                };
                if r as usize == hs {
                    self.zero(at, hs - start);
                    self.zero(he - self.ptr as usize, end - he);
                    self.mark_written();
                    return true;
                }
            }
        }
        self.zero(at, n)
    }

    /// Whether the `n` bytes at `at` are all zero (and RAM). Reading a page the guest never
    /// touched does not commit it (the host maps its zero page).
    pub fn is_zero(&self, at: usize, n: usize) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        let words = n / 8;
        // In bounds (above), so `add` would do; `wrapping_add` needs no promise
        let p = self.ptr.wrapping_add(at);
        // SAFETY: words i < n / 8 lie within the n bytes at `p` (in bounds, above); unaligned
        // reads (`at` is usually a page)
        (0..words).all(|i| unsafe { (p as *const u64).add(i).read_unaligned() } == 0)
            // SAFETY: the bytes after the last word, below n
            && (words * 8..n).all(|i| unsafe { *p.add(i) } == 0)
    }

    /// A copy of all of RAM.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = vec![0; self.len];
        self.read(0, &mut v);
        v
    }

    /// `size` (1, 2, 4 or 8) bytes at `at`, little-endian, at any alignment. Callers have checked
    /// that the access is RAM (a page the fast tables map, or a range compared with the RAM
    /// size); one that isn't reads 0.
    #[inline(always)]
    pub fn load(&self, at: usize, size: u32) -> u64 {
        if !self.holds(at, size as usize) {
            debug_assert!(false, "load of {size} at {at:#x} outside RAM");
            return 0;
        }
        // SAFETY: in bounds; guest accesses need not be aligned, hence read_unaligned.
        unsafe {
            let p = self.ptr.add(at);
            match size {
                1 => *p as u64,
                2 => u16::from_le((p as *const u16).read_unaligned()) as u64,
                4 => u32::from_le((p as *const u32).read_unaligned()) as u64,
                _ => u64::from_le((p as *const u64).read_unaligned()),
            }
        }
    }

    /// Store the low `size` (1, 2, 4 or 8) bytes of `value` at `at`; see `load`. One outside RAM
    /// is dropped.
    #[inline(always)]
    pub fn store(&self, at: usize, size: u32, value: u64) {
        if !self.holds(at, size as usize) {
            debug_assert!(false, "store of {size} at {at:#x} outside RAM");
            return;
        }
        self.mark_written();
        // SAFETY: as in `load`.
        unsafe {
            let p = self.ptr.add(at);
            match size {
                1 => *p = value as u8,
                2 => (p as *mut u16).write_unaligned((value as u16).to_le()),
                4 => (p as *mut u32).write_unaligned((value as u32).to_le()),
                _ => (p as *mut u64).write_unaligned(value.to_le()),
            }
        }
    }

    /// The atomic at `at` (naturally aligned, in RAM), or None.
    #[inline(always)]
    fn atomic<T>(&self, at: u64, size: u32, f: impl FnOnce(*mut u8) -> T) -> Option<T> {
        let at = usize::try_from(at).ok()?;
        if !at.is_multiple_of(size as usize) || !self.holds(at, size as usize) {
            return None;
        }
        // SAFETY: in bounds. RAM is page-aligned on the host, so a naturally aligned guest
        // address is aligned for an atomic of its size too.
        Some(f(unsafe { self.ptr.add(at) }))
    }

    /// Atomic load of `size` bytes at a naturally aligned `at`. None if not aligned or not RAM.
    pub fn load_atomic(&self, at: u64, size: u32) -> Option<u64> {
        let o = Ordering::SeqCst;
        // SAFETY: `atomic` hands over an aligned pointer into RAM, which lives as long as `self`.
        self.atomic(at, size, |p| unsafe {
            match size {
                1 => AtomicU8::from_ptr(p).load(o) as u64,
                2 => AtomicU16::from_ptr(p as *mut u16).load(o) as u64,
                4 => AtomicU32::from_ptr(p as *mut u32).load(o) as u64,
                _ => AtomicU64::from_ptr(p as *mut u64).load(o),
            }
        })
    }

    /// Atomic compare-and-swap of `size` bytes at a naturally aligned `at`: whether `new` was
    /// stored. None if not aligned or not RAM.
    pub fn cmpxchg(&self, at: u64, size: u32, expected: u64, new: u64) -> Option<bool> {
        let o = Ordering::SeqCst;
        self.mark_written();
        // SAFETY: as in `load_atomic`.
        self.atomic(at, size, |p| unsafe {
            match size {
                1 => AtomicU8::from_ptr(p)
                    .compare_exchange(expected as u8, new as u8, o, o)
                    .is_ok(),
                2 => AtomicU16::from_ptr(p as *mut u16)
                    .compare_exchange(expected as u16, new as u16, o, o)
                    .is_ok(),
                4 => AtomicU32::from_ptr(p as *mut u32)
                    .compare_exchange(expected as u32, new as u32, o, o)
                    .is_ok(),
                _ => AtomicU64::from_ptr(p as *mut u64)
                    .compare_exchange(expected, new, o, o)
                    .is_ok(),
            }
        })
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        if let Some((raw, layout)) = self.alloc {
            // SAFETY: what `alloc_zeroed` returned for this layout, freed once (aliases have none).
            unsafe { std::alloc::dealloc(raw, layout) }
        }
        #[cfg(unix)]
        if self.mapped {
            // SAFETY: the mapping `from_image` made, unmapped once (aliases have none).
            unsafe { libc::munmap(self.ptr.cast(), self.len) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Ram, PAGE};

    #[test]
    fn ram_is_page_aligned_zero_and_writable() {
        let ram = Ram::zeroed(64 << 20);
        assert_eq!(ram.as_ptr() as usize % PAGE, 0);
        assert_eq!(ram.len(), 64 << 20);
        assert!(ram.is_zero(0, ram.len()));
        ram.store(0, 1, 1);
        let last = ram.len() - 1;
        ram.store(last, 1, 2);
        assert_eq!((ram.load(0, 1), ram.load(last, 1)), (1, 2));
        assert!(!ram.is_zero(0, PAGE));
    }

    #[test]
    fn accesses_past_the_end_do_nothing() {
        let ram = Ram::zeroed(2 * PAGE);
        let end = ram.len();
        let mut buf = [7u8; 8];
        assert!(!ram.read(end - 4, &mut buf));
        assert_eq!(buf, [7; 8], "nothing copied");
        assert!(!ram.write(end - 4, &buf));
        assert!(!ram.write(usize::MAX, &buf), "no wrap-around");
        assert!(!ram.zero(end, 1));
        assert!(ram.zero(end, 0));
        assert!(ram.is_zero(0, end));
        assert_eq!(ram.load_atomic(end as u64, 4), None);
        assert_eq!(ram.load_atomic(u64::MAX - 3, 4), None);
        assert_eq!(ram.cmpxchg(end as u64 - 8, 8, 0, 1), Some(true));
        assert_eq!(ram.load(end - 8, 8), 1);
    }

    #[test]
    fn widths_are_little_endian_and_unaligned_plain_accesses_work() {
        let ram = Ram::zeroed(PAGE);
        ram.store(3, 8, 0x0807_0605_0403_0201);
        let mut b = [0u8; 8];
        assert!(ram.read(3, &mut b));
        assert_eq!(b, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(ram.load(4, 2), 0x0302);
        assert_eq!(ram.load(5, 4), 0x0605_0403);
        // Atomics need natural alignment
        assert_eq!(ram.load_atomic(3, 8), None);
        assert_eq!(ram.cmpxchg(2, 4, 0, 1), None);
        assert_eq!(ram.load_atomic(4, 4), Some(0x0504_0302));
        assert_eq!(ram.cmpxchg(4, 4, 0x0504_0302, 9), Some(true));
        assert_eq!(ram.cmpxchg(4, 4, 0x0504_0302, 10), Some(false));
        assert_eq!(ram.load(4, 4), 9);
    }

    #[cfg(unix)]
    fn image(len: usize, pages: &[(usize, u8)]) -> std::fs::File {
        use std::os::unix::fs::FileExt;
        // Tests run in parallel, several with the same length: a path of its own for each call,
        // or one would remove the file another just made
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("armless-image-{}-{len}-{n}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        f.set_len(len as u64).unwrap();
        for &(page, b) in pages {
            f.write_all_at(&[b; PAGE], (page * PAGE) as u64).unwrap();
        }
        f
    }

    #[cfg(unix)]
    #[test]
    fn ram_from_an_image_is_its_pages_and_writes_stay_private() {
        let f = image(8 * PAGE, &[(2, 0x22), (5, 0x55)]);
        let a = Ram::from_image(&f, 8 * PAGE).unwrap();
        let b = Ram::from_image(&f, 8 * PAGE).unwrap();
        assert!(a.is_image() && a.alias().is_image() && !Ram::zeroed(PAGE).is_image());
        assert!(!a.take_fresh(), "an image's RAM isn't all zero");
        assert_eq!((a.load(2 * PAGE + 9, 1), a.load(5 * PAGE, 1)), (0x22, 0x55));
        assert!(a.is_zero(3 * PAGE, PAGE), "a hole is zero");

        // A write is a's own: b and the file still have the image's page
        a.store(2 * PAGE, 1, 0xaa);
        a.store(7 * PAGE, 1, 0x77);
        assert_eq!((a.load(2 * PAGE, 1), a.load(7 * PAGE, 1)), (0xaa, 0x77));
        assert_eq!((b.load(2 * PAGE, 1), b.load(7 * PAGE, 1)), (0x22, 0));
        let mut on_disk = [0u8; 1];
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut on_disk, (2 * PAGE) as u64).unwrap();
        assert_eq!(on_disk, [0x22]);
        assert_eq!(a.cmpxchg((5 * PAGE) as u64, 4, 0x5555_5555, 1), Some(true));
        assert_eq!(b.load(5 * PAGE, 4), 0x5555_5555);
    }

    #[cfg(unix)]
    #[test]
    fn a_fork_has_the_parents_ram_from_an_image() {
        let f = image(8 * PAGE, &[(2, 0x22), (5, 0x55)]);
        let a = Ram::from_image(&f, 8 * PAGE).unwrap();
        a.store(2 * PAGE, 1, 0xaa);
        // SAFETY: the child only reads and writes this RAM, then exits without unwinding.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            let ok = a.load(2 * PAGE, 1) == 0xaa
                && a.load(2 * PAGE + 1, 1) == 0x22
                && a.load(5 * PAGE, 1) == 0x55
                && a.is_zero(3 * PAGE, PAGE);
            a.store(5 * PAGE, 1, 0x11);
            let ok = ok && a.load(5 * PAGE, 1) == 0x11;
            // SAFETY: ends the child at once.
            unsafe { libc::_exit(if ok { 0 } else { 1 }) };
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        assert!(
            libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
            "the child saw other memory: {status}"
        );
        assert_eq!(a.load(5 * PAGE, 1), 0x55, "the child's write is its own");
    }

    #[cfg(unix)]
    #[test]
    fn an_image_of_another_size_is_refused() {
        let f = image(4 * PAGE, &[]);
        assert!(Ram::from_image(&f, 8 * PAGE).is_err());
        assert!(Ram::from_image(&f, 2 * PAGE).is_err());
        assert!(Ram::from_image(&f, 4 * PAGE).is_ok());
    }

    #[test]
    fn discard_zeroes_exactly_its_range() {
        // A range that starts and ends inside host pages (16 KiB on Apple silicon), on both
        // kinds of RAM: the allocator's, and a mapping
        let check = |ram: Ram| {
            for i in 0..64 {
                ram.store(i * PAGE, 8, 0x1111 + i as u64);
                ram.store(i * PAGE + PAGE - 8, 8, 0x2222 + i as u64);
            }
            assert!(ram.discard(3 * PAGE, 40 * PAGE));
            for i in 0..64 {
                let kept = !(3..43).contains(&i);
                let (head, tail) = (ram.load(i * PAGE, 8), ram.load(i * PAGE + PAGE - 8, 8));
                assert_eq!((head, tail) != (0, 0), kept, "page {i}");
                if kept {
                    assert_eq!(
                        (head, tail),
                        (0x1111 + i as u64, 0x2222 + i as u64),
                        "page {i}"
                    );
                }
            }
            // Still RAM: written again, read back
            ram.store(20 * PAGE, 8, 7);
            assert_eq!(ram.load(20 * PAGE, 8), 7);
            assert!(!ram.discard(60 * PAGE, 8 * PAGE));
        };
        check(Ram::zeroed(64 * PAGE));
        #[cfg(unix)]
        {
            let f = image(64 * PAGE, &[]);
            check(Ram::from_image(&f, 64 * PAGE).unwrap());
        }
    }
}
