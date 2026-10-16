//! Guest RAM: one zero-filled byte buffer.
//!
//! On wasm32 the buffer comes straight from `memory.grow`, whose pages are
//! guaranteed to be zero, so a large RAM costs nothing to set up (zeroing it
//! through the allocator took ~0.5 ms per MiB). Grown memory cannot be given
//! back, so dropping such a buffer leaks it; the wasm build creates one CPU.

use std::ops::{Deref, DerefMut};

pub struct Ram {
    ptr: *mut u8,
    len: usize,
    /// What the global allocator returned, to free it with (none for memory.grow and aliases).
    alloc: Option<(*mut u8, std::alloc::Layout)>,
}

const PAGE: usize = 4096;

// Guest RAM is shared by the vCPU threads like physical memory is shared by
// cores: concurrent accesses are the guest's business (it orders them with
// barriers, acquire/release and exclusives, which are atomic operations here).
unsafe impl Send for Ram {}
unsafe impl Sync for Ram {}

impl Ram {
    /// Another handle on the same memory; it does not free it when dropped.
    pub fn alias(&self) -> Ram {
        Ram {
            ptr: self.ptr,
            len: self.len,
            alloc: None,
        }
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

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
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!raw.is_null(), "out of memory allocating guest RAM");
        Ram {
            ptr: unsafe { raw.add(raw.align_offset(PAGE)) },
            len,
            alloc: Some((raw, layout)),
        }
    }
}

impl Deref for Ram {
    type Target = [u8];
    #[inline(always)]
    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl DerefMut for Ram {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Ram {
    fn drop(&mut self) {
        if let Some((raw, layout)) = self.alloc {
            unsafe { std::alloc::dealloc(raw, layout) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ram;

    #[test]
    fn ram_is_page_aligned_zero_and_writable() {
        let mut ram = Ram::zeroed(64 << 20);
        assert_eq!(ram.as_mut_ptr() as usize % 4096, 0);
        assert_eq!(ram.len(), 64 << 20);
        assert!(ram.iter().step_by(4096).all(|&b| b == 0));
        ram[0] = 1;
        let last = ram.len() - 1;
        ram[last] = 2;
        assert_eq!((ram[0], ram[last]), (1, 2));
    }
}
