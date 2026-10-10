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
    /// Allocated with the global allocator (else taken from memory.grow, or an alias).
    boxed: bool,
}

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
            boxed: false,
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
                    boxed: false,
                };
            }
        }
        // Page-aligned, so naturally aligned guest addresses are aligned on
        // the host too (atomic accesses need that).
        let layout = std::alloc::Layout::from_size_align(len.max(1), 4096).expect("RAM size");
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory allocating guest RAM");
        Ram {
            ptr,
            len,
            boxed: true,
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
        if self.boxed {
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    self.ptr, self.len,
                )))
            }
        }
    }
}
