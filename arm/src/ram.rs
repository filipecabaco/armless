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
    /// Allocated as a `Box<[u8]>` (else taken from memory.grow).
    boxed: bool,
}

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
                    boxed: false,
                };
            }
        }
        let b = vec![0u8; len].into_boxed_slice();
        Ram {
            ptr: Box::into_raw(b) as *mut u8,
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
