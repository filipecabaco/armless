//! zstd decompression: 9p blobs (`.bin.zst`), compressed snapshots and
//! zstd-compressed kernels.
//!
//! In wasm this is the C decoder (`lib/zstd/zstddeclib.c`, linked in by
//! `build-wasm.sh`), which is much faster there; natively it is `ruzstd`.

#[cfg(target_arch = "wasm32")]
mod imp {
    use std::alloc;

    extern "C" {
        fn ZSTD_createDStream() -> u32;
        fn ZSTD_freeDStream(ctx: u32) -> i32;
        fn ZSTD_decompressStream_simpleArgs(
            ctx: u32,
            dst: *mut u8,
            dst_capacity: u32,
            dst_pos: *mut u32,
            src: *const u8,
            src_size: u32,
            src_pos: *mut u32,
        ) -> i32;
        fn ZSTD_isError(err: i32) -> bool;
    }

    const MALLOC_ALIGN: usize = 16;

    /// malloc for the zstd library (`#define malloc v86_malloc`).
    ///
    /// # Safety
    /// Called by C code only.
    #[no_mangle]
    pub unsafe extern "C" fn v86_malloc(size: u32) -> u32 {
        let Ok(layout) = alloc::Layout::from_size_align(size as usize + MALLOC_ALIGN, MALLOC_ALIGN)
        else {
            return 0;
        };
        // SAFETY: the layout is never zero-sized (MALLOC_ALIGN bytes of header at least).
        let addr = unsafe { alloc::alloc(layout) };
        if addr.is_null() {
            return 0; // as malloc: zstd checks
        }
        // SAFETY: the header, at the start of what was just allocated, 16-byte aligned.
        unsafe { *(addr as *mut u32) = size };
        addr as u32 + MALLOC_ALIGN as u32
    }

    /// # Safety
    /// `addr` must come from `v86_malloc`.
    #[no_mangle]
    pub unsafe extern "C" fn v86_free(addr: u32) {
        if addr == 0 {
            return;
        }
        let base = addr - MALLOC_ALIGN as u32;
        // SAFETY: `addr` came from `v86_malloc` (the caller's promise), so its header is before it.
        let size = unsafe { *(base as *mut u32) };
        let layout = alloc::Layout::from_size_align(size as usize + MALLOC_ALIGN, MALLOC_ALIGN)
            .expect("the layout v86_malloc made");
        // SAFETY: allocated by `v86_malloc` with this layout.
        unsafe { alloc::dealloc(base as *mut u8, layout) }
    }

    /// Streaming decoder: compressed bytes in, decompressed chunks out.
    pub struct Stream {
        ctx: u32,
        buf: Vec<u8>,
    }

    impl Default for Stream {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Stream {
        pub fn new() -> Stream {
            Stream {
                // SAFETY: no arguments; 0 if it could not allocate, which `feed` checks.
                ctx: unsafe { ZSTD_createDStream() },
                buf: vec![0; 1 << 17],
            }
        }

        /// Decompress `input`, passing output to `sink`. False on corrupt data.
        pub fn feed(&mut self, input: &[u8], sink: &mut dyn FnMut(&[u8])) -> bool {
            if self.ctx == 0 {
                return false;
            }
            let mut src_pos = 0u32;
            loop {
                let mut dst_pos = 0u32;
                // SAFETY: a live stream; the output buffer and the input are ours, with their
                // lengths, and the positions are this frame's own variables.
                let r = unsafe {
                    ZSTD_decompressStream_simpleArgs(
                        self.ctx,
                        self.buf.as_mut_ptr(),
                        self.buf.len() as u32,
                        &mut dst_pos,
                        input.as_ptr(),
                        input.len() as u32,
                        &mut src_pos,
                    )
                };
                // SAFETY: only inspects the number.
                if unsafe { ZSTD_isError(r) } {
                    return false;
                }
                if dst_pos > 0 {
                    sink(&self.buf[..dst_pos as usize]);
                }
                // Done with this input once it is consumed and the output
                // buffer was not filled (nothing more buffered inside).
                if src_pos as usize >= input.len() && (dst_pos as usize) < self.buf.len() {
                    return true;
                }
            }
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this stream's own context, freed once (zstd ignores 0).
            unsafe {
                ZSTD_freeDStream(self.ctx);
            }
        }
    }

    pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut s = Stream::new();
        s.feed(data, &mut |b| out.extend_from_slice(b))
            .then_some(out)
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::io::Read;

    /// Every frame of `data`, decompressed.
    pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let mut rest = data;
        while !rest.is_empty() {
            let mut d = ruzstd::decoding::StreamingDecoder::new(&mut rest).ok()?;
            d.read_to_end(&mut out).ok()?;
        }
        Some(out)
    }

    /// Native stand-in for the wasm streaming decoder: each `feed` must hold
    /// whole frames (only the wasm exports, type-checked natively, use it).
    #[derive(Default)]
    pub struct Stream;

    impl Stream {
        pub fn new() -> Stream {
            Stream
        }
        pub fn feed(&mut self, input: &[u8], sink: &mut dyn FnMut(&[u8])) -> bool {
            match decompress(input) {
                Some(d) => {
                    sink(&d);
                    true
                },
                None => false,
            }
        }
    }

    /// A reader of the decompressed contents of a zstd stream.
    pub fn reader<'a, R: Read + 'a>(r: R) -> std::io::Result<Box<dyn Read + 'a>> {
        let d = ruzstd::decoding::StreamingDecoder::new(r)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        Ok(Box::new(d))
    }
}

pub use imp::*;

/// Whether `data` starts with the zstd frame magic.
pub fn is_zstd(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x28, 0xb5, 0x2f, 0xfd]
}
