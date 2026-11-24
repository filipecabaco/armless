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
//!
//! Addresses here are physical addresses. RAM past `board::LOW_RAM_MAX` is above 4 GiB
//! (`board::HIGH_RAM_BASE`), so RAM has a hole, the MMIO one, when there is that much of it;
//! on the host it is one buffer as long as RAM's end, whose part under the hole nothing touches
//! (it costs no memory: it is never written).

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

use crate::board;

pub struct Ram {
    ptr: *mut u8,
    /// Bytes of RAM, below and above the hole.
    len: u64,
    /// Bytes of RAM below 4 GiB (all of it on a machine of at most `board::LOW_RAM_MAX`).
    low: u64,
    /// One past RAM's last address.
    end: u64,
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

/// A RAM page's physical address as the CPU's fast tables keep it, in a u32 (the JIT reads 4-byte
/// entries): its low 32 bits, and its 4 GiB bank (`page >> 32`) in the low 12 bits, which a page's
/// address leaves zero. Pages below 4 GiB keep their address as it is.
#[inline(always)]
pub const fn fast_base(page: u64) -> u32 {
    page as u32 | (page >> 32) as u32
}

/// The physical address a fast table entry (`fast_base`) stands for.
#[inline(always)]
pub const fn fast_page(base: u32) -> u64 {
    ((base & 0xfff) as u64) << 32 | (base & !0xfff) as u64
}

const _: () = assert!(board::RAM_MAX <= 0xfff << 32);

// SAFETY: a Ram is a pointer and a length to memory that lives as long as its owner (aliases are
// held by things that keep the owner, the machine, alive); every access is a bounds-checked
// copy, a load or store of one width, or an atomic, so sharing it between threads is what
// sharing physical memory between cores is.
unsafe impl Send for Ram {}
// SAFETY: as for Send.
unsafe impl Sync for Ram {}

impl Ram {
    fn at(ptr: *mut u8, len: u64) -> Ram {
        Ram {
            ptr,
            len,
            low: board::ram_low(len),
            end: board::ram_end(len),
            alloc: None,
            mapped: false,
            image: false,
            fresh: Arc::new(AtomicBool::new(true)),
        }
    }

    /// The host bytes RAM of `len` bytes takes: up to its end, hole included.
    fn span(len: u64) -> usize {
        usize::try_from(board::ram_end(len)).expect("RAM larger than the host's address space")
    }

    pub fn zeroed(len: u64) -> Ram {
        #[cfg(target_arch = "wasm32")]
        {
            // Only RAM below 4 GiB lives in this module's memory; the host made the rest's
            assert!(
                crate::banks::call::banks() >= crate::banks::needed(len),
                "RAM above 4 GiB, and no memories for it"
            );
            let pages = (board::ram_low(len) as usize).div_ceil(65536).max(1);
            let old = core::arch::wasm32::memory_grow(0, pages);
            if old != usize::MAX {
                return Ram::at((old * 65536) as *mut u8, len);
            }
        }
        // Page-aligned, so naturally aligned guest addresses are aligned on the
        // host too (atomic accesses need that). Not by asking the allocator for
        // that alignment, though: it then zeroes every byte itself, committing
        // all of RAM up front. With its own alignment a zeroed allocation this
        // size is calloc's fresh zero pages, which cost nothing until the guest
        // touches them; one page more leaves room to align by hand.
        let layout =
            std::alloc::Layout::from_size_align(Self::span(len).max(1) + PAGE, 8).expect("RAM size");
        // SAFETY: the layout's size is at least one page, never zero.
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!raw.is_null(), "out of memory allocating guest RAM");
        // At most PAGE - 1 bytes in: the allocation has PAGE bytes to spare.
        let mut ram = Ram::at(raw.wrapping_add(raw.align_offset(PAGE)), len);
        ram.alloc = Some((raw, layout));
        ram
    }

    /// RAM on a private anonymous mapping of its own: every page reads as zero and is backed on
    /// demand (on Linux, without committing swap, and a read maps the kernel's one zero page), and
    /// `discard` can always map fresh pages over a range, macOS included, since no allocator owns
    /// it.
    #[cfg(unix)]
    pub fn mapped(len: u64) -> std::io::Result<Ram> {
        if len == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "RAM of 0 bytes"));
        }
        // SAFETY: a new anonymous mapping at an address the kernel picks; nothing else owns that
        // range, and `drop` unmaps it once.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::span(len),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let mut ram = Ram::at(ptr.cast(), len);
        ram.mapped = true;
        Ok(ram)
    }

    /// RAM whose pages are an image's: `file` holds RAM as it is, `len` bytes laid out by physical
    /// address (`end()` bytes: the hole under the MMIO window too, if RAM goes past it), with
    /// holes where it is zero (`split_state` writes one). It is mapped copy-on-write, private to this RAM:
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
    pub fn from_image(file: &std::fs::File, len: u64) -> std::io::Result<Ram> {
        use std::os::fd::AsRawFd;
        let size = file.metadata()?.len();
        if size != board::ram_end(len) || len == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the RAM image is {size} bytes, RAM {len}"),
            ));
        }
        // SAFETY: a new mapping, at an address the kernel picks, of the whole file (it is RAM's
        // span long); nothing else owns that range, and `drop` unmaps it once.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                Self::span(len),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let mut ram = Ram::at(ptr.cast(), len);
        ram.mapped = true;
        ram.image = true;
        ram.fresh = Arc::new(AtomicBool::new(false));
        Ok(ram)
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
            low: self.low,
            end: self.end,
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

    /// Bytes of RAM (the hole, if any, is not RAM).
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One past RAM's last physical address.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Where RAM is: (start, bytes) of each of its regions, below 4 GiB and above.
    pub fn regions(&self) -> impl Iterator<Item = (u64, u64)> {
        let high = self.len - self.low;
        [(0, self.low), (board::HIGH_RAM_BASE, high)].into_iter().filter(|r| r.1 != 0)
    }

    /// Where RAM starts, for code that addresses it itself: JIT-compiled blocks (which only
    /// access what the fast TLB maps into RAM) and the host's view of the wasm memory.
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Whether the `n` bytes at `at` are all RAM (in one of its regions).
    #[inline(always)]
    pub fn holds(&self, at: u64, n: u64) -> bool {
        at.checked_add(n)
            .is_some_and(|end| end <= self.low || (at >= board::HIGH_RAM_BASE && end <= self.end))
    }

    /// Where the byte at `at` (which `holds`) is on the host: below 4 GiB, or anywhere natively.
    #[inline(always)]
    fn host(&self, at: u64) -> *mut u8 {
        self.ptr.wrapping_add(at as usize)
    }

    /// In a browser, the bank and offset of `at` if it is above 4 GiB, in another memory than
    /// this module's (`crate::banks`). Never natively, where RAM is one buffer.
    #[inline(always)]
    fn banked(at: u64) -> Option<(u32, u32)> {
        if cfg!(target_arch = "wasm32") && at >= board::HIGH_RAM_BASE {
            Some(((at >> 32) as u32, at as u32))
        } else {
            None
        }
    }

    /// `f(bank, offset, done, len)` for each piece of the `n` bytes at `at` (which `banked` says
    /// are above 4 GiB) within one bank: `done` bytes of the range come before it.
    #[cfg(target_arch = "wasm32")]
    fn each_bank(at: u64, n: u64, mut f: impl FnMut(u32, u32, usize, u32)) {
        let mut done = 0;
        while done < n {
            let a = at + done;
            let len = (n - done).min((1 << 32) - (a & 0xffff_ffff));
            f((a >> 32) as u32, a as u32, done as usize, len as u32);
            done += len;
        }
    }

    /// Copy RAM at `at` into `buf`. False, and nothing copied, if that is not all RAM.
    pub fn read(&self, at: u64, buf: &mut [u8]) -> bool {
        if !self.holds(at, buf.len() as u64) {
            return false;
        }
        #[cfg(target_arch = "wasm32")]
        if Self::banked(at).is_some() {
            let to = buf.as_mut_ptr();
            Self::each_bank(at, buf.len() as u64, |b, o, done, len| {
                crate::banks::call::copy_out(b, o, to.wrapping_add(done), len)
            });
            return true;
        }
        // SAFETY: in bounds (above); `buf` is the caller's own memory, so they don't overlap.
        unsafe { std::ptr::copy_nonoverlapping(self.host(at), buf.as_mut_ptr(), buf.len()) };
        true
    }

    /// Copy `data` into RAM at `at`. False, and nothing written, if that is not all RAM.
    pub fn write(&self, at: u64, data: &[u8]) -> bool {
        if !self.holds(at, data.len() as u64) {
            return false;
        }
        self.mark_written();
        #[cfg(target_arch = "wasm32")]
        if Self::banked(at).is_some() {
            let from = data.as_ptr();
            Self::each_bank(at, data.len() as u64, |b, o, done, len| {
                crate::banks::call::copy_in(b, o, from.wrapping_add(done), len)
            });
            return true;
        }
        // SAFETY: as in `read`.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.host(at), data.len()) };
        true
    }

    /// Zero `n` bytes at `at`. False, and nothing written, if that is not all RAM.
    pub fn zero(&self, at: u64, n: u64) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        self.mark_written();
        #[cfg(target_arch = "wasm32")]
        if Self::banked(at).is_some() {
            Self::each_bank(at, n, |b, o, _, len| crate::banks::call::fill_zero(b, o, len));
            return true;
        }
        // SAFETY: in bounds.
        unsafe { std::ptr::write_bytes(self.host(at), 0, n as usize) };
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
    pub fn discard(&self, at: u64, n: u64) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        #[cfg(unix)]
        {
            // SAFETY: sysconf has no preconditions.
            let host =
                unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(PAGE as libc::c_long) as usize;
            let (start, end) = (self.host(at) as usize, self.host(at) as usize + n as usize);
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
                    self.zero(at, (hs - start) as u64);
                    self.zero(at + (he - start) as u64, (end - he) as u64);
                    self.mark_written();
                    return true;
                }
            }
        }
        self.zero(at, n)
    }

    /// Whether the `n` bytes at `at` are all zero (and RAM). Reading a page the guest never
    /// touched does not commit it (the host maps its zero page).
    pub fn is_zero(&self, at: u64, n: u64) -> bool {
        if !self.holds(at, n) {
            return false;
        }
        if Self::banked(at).is_some() {
            // Through a copy, a page at a time
            let mut page = [0u8; PAGE];
            return (0..n).step_by(PAGE).all(|i| {
                let k = (n - i).min(PAGE as u64) as usize;
                self.read(at + i, &mut page[..k]) && page[..k].iter().all(|&b| b == 0)
            });
        }
        let n = n as usize;
        let words = n / 8;
        let p = self.host(at);
        // SAFETY: words i < n / 8 lie within the n bytes at `p` (in bounds, above); unaligned
        // reads (`at` is usually a page)
        (0..words).all(|i| unsafe { (p as *const u64).add(i).read_unaligned() } == 0)
            // SAFETY: the bytes after the last word, below n
            && (words * 8..n).all(|i| unsafe { *p.add(i) } == 0)
    }

    /// A copy of all of RAM's span, by physical address (the hole, if any, reads as zeros).
    pub fn to_vec(&self) -> Vec<u8> {
        let mut v = vec![0; self.end as usize];
        for (at, n) in self.regions() {
            self.read(at, &mut v[at as usize..(at + n) as usize]);
        }
        v
    }

    /// `size` (1, 2, 4 or 8) bytes at `at`, little-endian, at any alignment. Callers have checked
    /// that the access is RAM (a page the fast tables map, or a range `holds`); one that isn't
    /// reads 0.
    #[inline(always)]
    pub fn load(&self, at: u64, size: u32) -> u64 {
        if !self.holds(at, size as u64) {
            debug_assert!(false, "load of {size} at {at:#x} outside RAM");
            return 0;
        }
        // SAFETY: in bounds (above).
        unsafe { self.load_in_page(at, size) }
    }

    /// `load` without the bounds check, for the CPU's fast-table hits.
    ///
    /// # Safety
    ///
    /// The `size` bytes at `at` must lie in RAM (`holds`).
    #[inline(always)]
    pub unsafe fn load_in_page(&self, at: u64, size: u32) -> u64 {
        debug_assert!(self.holds(at, size as u64), "load of {size} at {at:#x} outside RAM");
        #[cfg(target_arch = "wasm32")]
        if let Some((b, o)) = Self::banked(at) {
            return crate::banks::call::load(b, o, size);
        }
        // SAFETY: in bounds (the caller's promise); guest accesses need not be aligned, hence
        // read_unaligned.
        unsafe {
            let p = self.host(at);
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
    pub fn store(&self, at: u64, size: u32, value: u64) {
        if !self.holds(at, size as u64) {
            debug_assert!(false, "store of {size} at {at:#x} outside RAM");
            return;
        }
        self.mark_written();
        // SAFETY: in bounds (above), and RAM is marked written.
        unsafe { self.store_in_page(at, size, value) }
    }

    /// `store` without the bounds check and without `mark_written`, for the CPU's fast-table
    /// hits.
    ///
    /// # Safety
    ///
    /// The `size` bytes at `at` must lie in RAM (`holds`), and `mark_written` must have been
    /// called on this RAM (or an alias of it) before this call: the CPU does so when it fills the
    /// fast write entry the store goes through, and `fresh` never becomes true again.
    #[inline(always)]
    pub unsafe fn store_in_page(&self, at: u64, size: u32, value: u64) {
        debug_assert!(self.holds(at, size as u64), "store of {size} at {at:#x} outside RAM");
        debug_assert!(
            !self.fresh.load(Ordering::Relaxed),
            "store before RAM was marked written"
        );
        #[cfg(target_arch = "wasm32")]
        if let Some((b, o)) = Self::banked(at) {
            return crate::banks::call::store(b, o, size, value);
        }
        // SAFETY: in bounds (the caller's promise).
        unsafe {
            let p = self.host(at);
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
        if !at.is_multiple_of(size as u64) || !self.holds(at, size as u64) {
            return None;
        }
        // RAM is page-aligned on the host, so a naturally aligned guest address is aligned for an
        // atomic of its size too.
        Some(f(self.host(at)))
    }

    /// Atomic load of `size` bytes at a naturally aligned `at`. None if not aligned or not RAM.
    pub fn load_atomic(&self, at: u64, size: u32) -> Option<u64> {
        #[cfg(target_arch = "wasm32")]
        if let Some((b, o)) = Self::banked(at) {
            let ok = at.is_multiple_of(size as u64) && self.holds(at, size as u64);
            return ok.then(|| crate::banks::call::atomic_load(b, o, size));
        }
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
        self.mark_written();
        #[cfg(target_arch = "wasm32")]
        if let Some((b, o)) = Self::banked(at) {
            if !at.is_multiple_of(size as u64) || !self.holds(at, size as u64) {
                return None;
            }
            let mask = if size == 8 { u64::MAX } else { (1 << (8 * size)) - 1 };
            let old = crate::banks::call::cmpxchg(b, o, size, expected & mask, new & mask);
            return Some(old == expected & mask);
        }
        let o = Ordering::SeqCst;
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
            // SAFETY: the mapping `mapped` or `from_image` made, unmapped once (aliases have none).
            unsafe { libc::munmap(self.ptr.cast(), board::ram_end(self.len) as usize) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ram;
    const PAGE: u64 = super::PAGE as u64;

    #[test]
    fn ram_is_page_aligned_zero_and_writable() {
        let ram = Ram::zeroed(64 << 20);
        assert_eq!(ram.as_ptr() as u64 % PAGE, 0);
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
        assert!(!ram.write(u64::MAX, &buf), "no wrap-around");
        assert!(!ram.zero(end, 1));
        assert!(ram.zero(end, 0));
        assert!(ram.is_zero(0, end));
        assert_eq!(ram.load_atomic(end, 4), None);
        assert_eq!(ram.load_atomic(u64::MAX - 3, 4), None);
        assert_eq!(ram.cmpxchg(end - 8, 8, 0, 1), Some(true));
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
    fn image(len: u64, pages: &[(usize, u8)]) -> std::fs::File {
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
        f.set_len(len).unwrap();
        for &(page, b) in pages {
            f.write_all_at(&[b; super::PAGE], page as u64 * PAGE).unwrap();
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
        std::os::unix::fs::FileExt::read_exact_at(&f, &mut on_disk, 2 * PAGE).unwrap();
        assert_eq!(on_disk, [0x22]);
        assert_eq!(a.cmpxchg(5 * PAGE, 4, 0x5555_5555, 1), Some(true));
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
                ram.store(i * PAGE, 8, 0x1111 + i);
                ram.store(i * PAGE + PAGE - 8, 8, 0x2222 + i);
            }
            assert!(ram.discard(3 * PAGE, 40 * PAGE));
            for i in 0..64 {
                let kept = !(3..43).contains(&i);
                let (head, tail) = (ram.load(i * PAGE, 8), ram.load(i * PAGE + PAGE - 8, 8));
                assert_eq!((head, tail) != (0, 0), kept, "page {i}");
                if kept {
                    assert_eq!(
                        (head, tail),
                        (0x1111 + i, 0x2222 + i),
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
            check(Ram::mapped(64 * PAGE).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn mapped_ram_is_zero_fresh_and_no_image() {
        let ram = Ram::mapped(64 << 20).unwrap();
        assert_eq!(ram.as_ptr() as u64 % PAGE, 0);
        assert!(!ram.is_image());
        assert!(ram.is_zero(0, ram.len()));
        assert!(ram.take_fresh(), "nothing has written it");
        ram.store(ram.len() - 8, 8, 9);
        assert_eq!(ram.load(ram.len() - 8, 8), 9);
        assert!(Ram::mapped(0).is_err());
    }

    #[test]
    fn fast_entries_keep_a_page_and_its_bank() {
        for page in [0, 0x1000, 0xbfff_f000, 1 << 32, (1 << 32) + 0x7000, (15 << 32) + 0xffff_f000] {
            assert_eq!(super::fast_page(super::fast_base(page)), page);
        }
        assert_eq!(super::fast_base(0xbfff_f000), 0xbfff_f000, "below 4 GiB, the address itself");
        assert_eq!(super::fast_base((1 << 32) + 0x5000) & 0xfff, 1);
    }

    #[cfg(unix)]
    #[test]
    fn ram_past_3_gib_goes_on_above_4_gib() {
        use crate::board::{HIGH_RAM_BASE, LOW_RAM_MAX};
        let ram = Ram::mapped(LOW_RAM_MAX + (8 << 20)).unwrap();
        assert_eq!(ram.end(), HIGH_RAM_BASE + (8 << 20));
        assert_eq!(
            ram.regions().collect::<Vec<_>>(),
            [(0, LOW_RAM_MAX), (HIGH_RAM_BASE, 8 << 20)]
        );
        // RAM on either side of the hole; not the hole, nor a range across it or past the end
        assert!(ram.holds(LOW_RAM_MAX - 8, 8) && ram.holds(HIGH_RAM_BASE, 8 << 20));
        assert!(!ram.holds(LOW_RAM_MAX, 1) && !ram.holds(HIGH_RAM_BASE - 1, 1));
        assert!(!ram.holds(LOW_RAM_MAX - 4, 8) && !ram.holds(ram.end() - 4, 8));
        assert!(!ram.write(LOW_RAM_MAX, &[1]) && !ram.write(HIGH_RAM_BASE - 1, &[1, 2]));

        let top = HIGH_RAM_BASE + (8 << 20) - PAGE;
        ram.store(top + 8, 8, 0x1122_3344_5566_7788);
        assert_eq!(ram.load(top + 8, 8), 0x1122_3344_5566_7788);
        assert!(ram.write(HIGH_RAM_BASE + 3, b"above"));
        let mut b = [0u8; 5];
        assert!(ram.read(HIGH_RAM_BASE + 3, &mut b) && &b == b"above");
        assert_eq!(ram.cmpxchg(top + 8, 8, 0x1122_3344_5566_7788, 5), Some(true));
        assert_eq!(ram.load_atomic(top + 8, 8), Some(5));
        assert!(!ram.is_zero(top, PAGE));
        assert!(ram.discard(top, PAGE) && ram.is_zero(top, PAGE));
        assert!(ram.is_zero(HIGH_RAM_BASE + PAGE, PAGE), "untouched");
    }
}
