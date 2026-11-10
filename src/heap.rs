//! The allocator of the wasm build: std's (dlmalloc), and the pages of a big block that is freed go back to the host.
//!
//! The linear memory of a wasm instance never shrinks and dlmalloc cannot return what it frees, so every byte the
//! machine held at once stayed in the host's memory for as long as the machine lives: the compressed bytes of a blob
//! and their copies, the decompressed file, the parse of a filesystem, the window of the zstd decoder. When a block of
//! `RELEASE_MIN` bytes or more is freed (or shrunk), the whole pages inside it go to the host (`env.heap_release`),
//! which discards them (`MADV_DONTNEED`): they read zeros afterwards and are in physical memory again only when
//! something writes to them. A host that cannot discard pages of its memory imports a function that does nothing.
//!
//! What dlmalloc still reads of a block after it is freed. A block at `ptr` of `size` bytes (the `Layout` of the
//! caller) lies in a chunk whose header (`prev_foot`, `head`) is the 8 bytes before `ptr`; `free` and `realloc` assert
//! `chunk size >= size + 4` (`validate_size`), so the chunk ends at `ptr + size - 4` or later. A free chunk keeps its
//! bookkeeping in its first 32 bytes (`head`, `prev_foot`, `fd`, `bk`, and for a tree chunk `child[2]`, `parent`,
//! `index`), and its footer, which is the `prev_foot` of the next chunk, at the chunk's end. A free chunk is joined to
//! the free chunks around it, and its first 32 bytes are the only ones that are read again (when the chunk is taken
//! out of its bin), so the first 32 bytes of the first chunk of the join, which are at or before `ptr - 8 + 32`, and
//! the footer and header at the end, which are at `ptr + size - 4` or later. Nothing else of a free chunk is read:
//! when it is split, the allocator writes the header of the rest before it reads it (`memalign` reads one bit of the
//! old header of the rest first, and that bit does not change what it hands out), and `calloc` clears the whole
//! block it hands out (`calloc_must_clear` is true unless the chunk was mapped, and nothing is mapped in wasm). So
//! the bytes from `ptr + KEEP_HEAD` to `ptr + size - KEEP_TAIL` are never read before they are written again, and
//! those are the ones that are given back.
//!
//! The pages are discarded before the block is handed to dlmalloc, while the caller still owns it: nothing can
//! allocate it in between, whichever thread allocates.
//!
//! Some of the big blocks are not freed at once but kept (see `Cache`), because the decoder of a blob allocates and
//! frees a window of 8 MiB for each blob it reads, and giving the pages back and faulting them in again for each of
//! them is what the host would spend its time on. A block that is kept is an allocation of dlmalloc that is still
//! live, so its contents and its chunk are untouched, and `flush` gives all of them back (the host calls it when the
//! machine is up and its memory is measured).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// What the allocator has handed out and not had back: the bytes (as asked for, not as dlmalloc rounds them), the most
/// it was, and how much of it is in blocks of `RELEASE_MIN` or more. `armless_mem_report` shows them, to tell what the
/// heap of a machine holds apart from the memory of the guest.
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LARGE: AtomicUsize = AtomicUsize::new(0);

fn count_up(size: usize) {
    let now = LIVE.fetch_add(size, Relaxed) + size;
    PEAK.fetch_max(now, Relaxed);
    if size >= RELEASE_MIN {
        LARGE.fetch_add(size, Relaxed);
    }
}

fn count_down(size: usize) {
    LIVE.fetch_sub(size, Relaxed);
    if size >= RELEASE_MIN {
        LARGE.fetch_sub(size, Relaxed);
    }
}

/// What the heap holds (see `LIVE`). `kept` is in `live` too: freed big blocks held to be handed out again.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub live: usize,
    pub peak: usize,
    pub large: usize,
    pub kept: usize,
}

pub fn stats() -> Stats {
    Stats {
        live: LIVE.load(Relaxed),
        peak: PEAK.load(Relaxed),
        large: LARGE.load(Relaxed),
        // Not counted when another thread holds the cache at this instant
        kept: lock().map_or(0, |c| c.bytes),
    }
}

#[link(wasm_import_module = "env")]
unsafe extern "C" {
    /// The host may discard the whole pages of `ptr..ptr + len` (they read zeros afterwards).
    fn heap_release(ptr: u32, len: u32);
}

/// The size from which a freed block is given back (the blocks below it are small enough for the allocator to reuse).
const RELEASE_MIN: usize = 1 << 20;
/// The bytes of freed blocks that are kept to be handed out again.
const CACHE_BUDGET: usize = 10 << 20;
/// The bytes of a freed block that dlmalloc may read again, at its start.
const KEEP_HEAD: usize = 64;
/// The same, at its end.
const KEEP_TAIL: usize = 32;
/// dlmalloc's alignment on wasm32: a block that asks for more is made by `memalign`, and a `realloc` of it is a
/// copy that can fail.
const DLMALLOC_ALIGN: usize = 8;

/// Blocks of a size that is given back, freed, that are kept (not given back, not freed to dlmalloc) to be handed out
/// again to the next request of exactly that size and alignment: a block that is freed and allocated again in a loop
/// (a decoder's window, the buffer of a read) is not discarded and faulted in again each time. The smallest block
/// goes first when the bytes kept would pass the budget, and `flush` gives them all back.
const CACHE_SLOTS: usize = 8;

struct Cache {
    ptr: [usize; CACHE_SLOTS],
    /// 0: the slot is empty.
    size: [usize; CACHE_SLOTS],
    align: [usize; CACHE_SLOTS],
    bytes: usize,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    ptr: [0; CACHE_SLOTS],
    size: [0; CACHE_SLOTS],
    align: [0; CACHE_SLOTS],
    bytes: 0,
});

/// The cache, unless another thread holds it: the allocator then goes without it. A browser's main thread allocates
/// (the blobs it hands to the machine) and must not wait (`memory.atomic.wait32` traps there, and std's `Mutex` waits
/// when it is contended), which is also why std's own lock of dlmalloc on wasm spins.
fn lock() -> Option<std::sync::MutexGuard<'static, Cache>> {
    match CACHE.try_lock() {
        Ok(c) => Some(c),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// Gives the interior of a block that nobody holds to the host and the block to dlmalloc.
unsafe fn drop_block(ptr: usize, size: usize, align: usize) {
    give(ptr + KEEP_HEAD, (ptr + size).saturating_sub(KEEP_TAIL));
    // SAFETY: the block was allocated by `System` with this size and alignment (`dealloc` and `put` record exactly
    // the layout they were given) and nobody holds it.
    unsafe {
        System.dealloc(
            ptr as *mut u8,
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

impl Cache {
    fn take(&mut self, size: usize, align: usize) -> Option<usize> {
        let i = (0..CACHE_SLOTS).find(|&i| self.size[i] == size && self.align[i] == align)?;
        self.size[i] = 0;
        self.bytes -= size;
        Some(self.ptr[i])
    }

    /// The slot of the smallest block kept.
    fn smallest(&self) -> Option<usize> {
        (0..CACHE_SLOTS)
            .filter(|&i| self.size[i] != 0)
            .min_by_key(|&i| self.size[i])
    }

    /// Keeps the block, unless it is smaller than all that are kept and they fill the budget. False: the caller
    /// gives it back.
    unsafe fn put(&mut self, ptr: usize, size: usize, align: usize) -> bool {
        loop {
            let slot = (0..CACHE_SLOTS).find(|&i| self.size[i] == 0);
            if self.bytes + size <= CACHE_BUDGET {
                if let Some(i) = slot {
                    self.ptr[i] = ptr;
                    self.size[i] = size;
                    self.align[i] = align;
                    self.bytes += size;
                    return true;
                }
            }
            match self.smallest() {
                Some(i) if self.size[i] < size => {
                    let (p, s, a) = (self.ptr[i], self.size[i], self.align[i]);
                    self.size[i] = 0;
                    self.bytes -= s;
                    // SAFETY: the block was kept with this layout and has just left the cache.
                    unsafe { drop_block(p, s, a) };
                },
                _ => return false,
            }
        }
    }
}

/// Gives back every block that is kept.
pub fn flush() {
    // Spins (see `lock`): another thread holds the cache only for the few instructions of a `take` or a `put`.
    let mut c = loop {
        if let Some(c) = lock() {
            break c;
        }
        std::hint::spin_loop();
    };
    for i in 0..CACHE_SLOTS {
        if c.size[i] != 0 {
            let (p, s, a) = (c.ptr[i], c.size[i], c.align[i]);
            c.size[i] = 0;
            c.bytes -= s;
            // SAFETY: as in `put`: the block was kept with this layout and has just left the cache.
            unsafe { drop_block(p, s, a) };
        }
    }
}

/// Gives the whole pages of `start..end` (an interior that dlmalloc does not read) to the host.
fn give(start: usize, end: usize) {
    if end > start + 4096 {
        // SAFETY: `start..end` is the interior of a block that is being freed: nothing reads it again.
        unsafe { heap_release(start as u32, (end - start) as u32) }
    }
}

/// A block that was kept and has the size and alignment asked for.
unsafe fn kept(layout: Layout) -> Option<*mut u8> {
    let size = layout.size();
    if size < RELEASE_MIN {
        return None;
    }
    let p = lock()?.take(size, layout.align())?;
    Some(p as *mut u8)
}

/// Makes this thread's instance know of the whole block it is about to use.
///
/// A shared memory that another thread grew is bigger for every thread, but the instance of a thread holds the size it
/// last learned of, and V8 updates that size only when the thread handles an interrupt (the stack checks at the entry
/// of a function and at the head of a loop, which a loop of baseline code without calls reaches only now and then), or
/// when it calls into JS or runs `memory.grow` itself. The bulk instructions (`memory.fill`, `memory.copy`) and, in
/// baseline code, the atomic ones check their bounds against that size, and `memory.size` and an atomic read do not
/// change it. A thread that takes a block out of the part of the heap that another thread just grew, and clears or
/// fills it before it knows of the growth, traps, and when that happens inside dlmalloc (the `memset` of `calloc`,
/// the copy of `realloc`) the trap leaves std's lock of the allocator held, and every other thread then spins on it.
///
/// `memory.grow` by 0 pages is the instruction that reads the current size of the shared memory and hands it to this
/// instance at once, so it is run when (and only when) a block that was just handed out ends past what the instance
/// holds. The thread that carved the block had grown the memory before it did, so one call covers the block. The
/// check is made after the allocator returns, where no lock is held: `alloc_zeroed` and `realloc` therefore do the
/// clearing and the copying themselves. A block that goes from one thread to another is not covered (the thread that
/// receives it can still hold a smaller size than the one that allocated it); nothing the machine does is known to
/// do that, and the hosts that run it with a native memory (wasmtime) read the size of the memory live.
#[inline]
fn learn(p: *mut u8, size: usize) {
    let end = p as usize as u64 + size as u64;
    let known = (core::arch::wasm32::memory_size::<0>() as u64) << 16;
    if !p.is_null() && end > known {
        refresh();
    }
}

#[cold]
#[inline(never)]
fn refresh() {
    let _ = core::arch::wasm32::memory_grow::<0>(0);
}

pub struct Heap;

// SAFETY: every method hands its arguments, under the contract of `GlobalAlloc`, to `System` (dlmalloc) or to a block
// that `System` allocated with the same layout.
unsafe impl GlobalAlloc for Heap {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `GlobalAlloc::alloc`'s contract is the one of `System.alloc`; a kept block has exactly this layout.
        let p = match unsafe { kept(layout) } {
            Some(p) => p,
            // SAFETY: as above.
            None => unsafe { System.alloc(layout) },
        };
        learn(p, layout.size());
        if !p.is_null() {
            count_up(layout.size());
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Not `System.alloc_zeroed`: dlmalloc clears inside its lock (see `learn`).
        // SAFETY: as in `alloc`; the block holds `layout.size()` bytes.
        let p = unsafe { self.alloc(layout) };
        if !p.is_null() {
            // SAFETY: `p` is a block of `layout.size()` bytes.
            unsafe { std::ptr::write_bytes(p, 0, layout.size()) };
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = layout.size();
        count_down(size);
        if size >= RELEASE_MIN {
            if size <= CACHE_BUDGET {
                if let Some(mut c) = lock() {
                    // SAFETY: the block is the caller's to free, with this layout, and is not used again.
                    if unsafe { c.put(ptr as usize, size, layout.align()) } {
                        return;
                    }
                }
            }
            let at = ptr as usize;
            give(at + KEEP_HEAD, (at + size).saturating_sub(KEEP_TAIL));
        }
        // SAFETY: the caller's contract of `dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old = layout.size();
        // A block that grows is moved by an allocation, a copy and a free of this wrapper, not by dlmalloc: when it
        // cannot grow where it is, dlmalloc copies inside its lock into a new block (see `learn`), and std's own
        // fallback for an alignment above dlmalloc's allocates without `learn`.
        if new_size > old || layout.align() > DLMALLOC_ALIGN {
            // SAFETY: the caller's contract of `realloc`: `new_size` is nonzero and does not overflow when rounded up to
            // the alignment, `ptr` is a live block of `layout`, and the new block does not overlap it.
            unsafe {
                let new = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
                if !new.is_null() {
                    std::ptr::copy_nonoverlapping(ptr, new, old.min(new_size));
                    self.dealloc(ptr, layout);
                }
                return new;
            }
        }
        // A block that shrinks gives back the end it loses, before dlmalloc takes it: a `realloc` that shrinks a
        // block of dlmalloc's alignment is done in place and cannot fail, so the contents are never wanted again.
        if old >= RELEASE_MIN && new_size < old {
            let at = ptr as usize;
            give(at + new_size + KEEP_HEAD, (at + old).saturating_sub(KEEP_TAIL));
        }
        // SAFETY: the caller's contract of `realloc`.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        // A block that shrinks stays where it is (see above), so the bytes it holds change by the difference
        if !p.is_null() {
            count_down(old);
            count_up(new_size);
        }
        p
    }
}
