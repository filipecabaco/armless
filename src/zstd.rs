//! zstd decompression: 9p blobs (`.bin.zst`), compressed snapshots and
//! zstd-compressed kernels.
//!
//! Wasm and native hosts decode with the C decoder: `build-wasm.sh` links `lib/zstd/zstddeclib.c`
//! into the wasm module, native hosts use the `zstd-sys` crate (built from its bundled libzstd).
//! `sys` is the only part that differs (the wasm ABI passes pointers and sizes as `u32`, and the
//! native decoder is given its own allocator as a `ZSTD_customMem`); everything else is shared.

use std::io::Read;

/// What the C decoder is asked for and gives back, over the target's ABI.
#[cfg(target_arch = "wasm32")]
mod sys {
    // SAFETY: the C decoder linked in by build-wasm.sh, with these signatures. Making a stream and
    // telling an error code from a size take no pointers and have no preconditions.
    unsafe extern "C" {
        safe fn ZSTD_createDStream() -> u32;
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
        safe fn ZSTD_isError(err: i32) -> bool;
    }

    /// A decompression context of the C decoder.
    pub struct Ctx(u32);

    impl Ctx {
        /// None if the decoder could not allocate one.
        pub fn new() -> Option<Ctx> {
            match ZSTD_createDStream() {
                0 => None,
                ctx => Some(Ctx(ctx)),
            }
        }

        /// One call of the streaming decoder: `(written, read, hint)`, or None on corrupt data.
        pub fn step(&mut self, dst: &mut [u8], src: &[u8]) -> Option<(usize, usize, usize)> {
            let (mut dst_pos, mut src_pos) = (0u32, 0u32);
            // SAFETY: a live context; `dst` and `src` are ours with their lengths, and the positions
            // are this frame's own variables.
            let r = unsafe {
                ZSTD_decompressStream_simpleArgs(
                    self.0,
                    dst.as_mut_ptr(),
                    dst.len() as u32,
                    &mut dst_pos,
                    src.as_ptr(),
                    src.len() as u32,
                    &mut src_pos,
                )
            };
            (!ZSTD_isError(r)).then_some((dst_pos as usize, src_pos as usize, r as usize))
        }
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            // SAFETY: this context, freed once.
            unsafe {
                ZSTD_freeDStream(self.0);
            }
        }
    }

    /// malloc for the zstd library (`#define malloc v86_malloc`).
    ///
    /// # Safety
    /// Called by C code only.
    #[no_mangle]
    pub unsafe extern "C" fn v86_malloc(size: u32) -> u32 {
        super::heap_block::malloc(size as usize) as u32
    }

    /// # Safety
    /// `addr` must come from `v86_malloc`.
    #[no_mangle]
    pub unsafe extern "C" fn v86_free(addr: u32) {
        // SAFETY: `addr` came from `v86_malloc` (the caller's promise).
        unsafe { super::heap_block::free(addr as *mut u8) }
    }
}

/// The native decoder, and the memory it allocates.
///
/// Every block the decoder allocates is owned by its context's `Blocks`, a map from the address
/// handed to the decoder to the block: `free` takes the block out of the map, which drops it. From
/// `MAP_FROM` bytes (the decoder's windows, up to 8 MiB) a block is an anonymous mapping of its own,
/// which dropping unmaps, so a freed window goes back to the system; glibc's `free` would keep the
/// pages it touched resident (its mmap threshold grows to the size of the blocks it frees). Smaller
/// blocks are `Vec<u128>`s: 16-byte-aligned, as `malloc` would give (zstd needs no more), and
/// moving the `Vec` into the map moves none of its memory. memmap2 maps anonymous memory on every
/// native target (Windows too), so this is the same everywhere.
#[cfg(not(target_arch = "wasm32"))]
mod sys {
    use memmap2::MmapMut;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex, PoisonError};
    use zstd_sys::{
        ZSTD_DStream, ZSTD_createDStream_advanced, ZSTD_customMem, ZSTD_decompressStream,
        ZSTD_freeDStream, ZSTD_inBuffer, ZSTD_isError, ZSTD_outBuffer,
    };

    /// From this size a block is its own anonymous mapping.
    pub const MAP_FROM: usize = 128 << 10;

    /// A block of the decoder's.
    pub enum Block {
        Heap(Vec<u128>),
        Map(MmapMut),
    }

    impl Block {
        /// At least `size` bytes, 16-byte-aligned (a mapping is page-aligned). None if it could not
        /// allocate them.
        fn new(size: usize) -> Option<Block> {
            if size >= MAP_FROM {
                return MmapMut::map_anon(size).ok().map(Block::Map);
            }
            // (one element at least: every block has an address of its own)
            let n = size.div_ceil(size_of::<u128>()).max(1);
            let mut v = Vec::new();
            v.try_reserve_exact(n).ok()?;
            v.resize(n, 0);
            Some(Block::Heap(v))
        }

        fn as_mut_ptr(&mut self) -> *mut c_void {
            match self {
                Block::Heap(v) => v.as_mut_ptr().cast(),
                Block::Map(m) => m.as_mut_ptr().cast(),
            }
        }
    }

    /// The blocks a context's decoder holds, by their address. A mutex rather than a `RefCell` so
    /// that `Blocks` is `Sync` (and an `Arc` of it `Send`): the decoder calls in on the thread that
    /// is using the context, so it is never contended.
    #[derive(Default)]
    pub struct Blocks(Mutex<HashMap<usize, Block>>);

    impl Blocks {
        pub fn alloc(&self, size: usize) -> *mut c_void {
            let Some(mut block) = Block::new(size) else {
                return std::ptr::null_mut(); // (zstd checks)
            };
            let ptr = block.as_mut_ptr();
            self.map().insert(ptr as usize, block);
            ptr
        }

        pub fn free(&self, ptr: *mut c_void) {
            if ptr.is_null() {
                return;
            }
            let block = self.map().remove(&(ptr as usize));
            debug_assert!(
                block.is_some(),
                "zstd freed a block it was not given: {ptr:?}"
            );
            drop(block); // (outside the lock)
        }

        pub fn map(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Block>> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    /// The decoder's malloc: a block of `Blocks`.
    ///
    /// # Safety
    /// `opaque` is the `Blocks` of the context, which outlives the decoder (zstd passes back the
    /// `opaque` of the `ZSTD_customMem` it was made with).
    unsafe extern "C" fn alloc(opaque: *mut c_void, size: usize) -> *mut c_void {
        // SAFETY: a live `Blocks` (the caller's promise), only ever shared.
        let blocks = unsafe { &*(opaque as *const Blocks) };
        blocks.alloc(size)
    }

    /// The decoder's free (null is fine).
    ///
    /// # Safety
    /// As `alloc`.
    unsafe extern "C" fn dealloc(opaque: *mut c_void, ptr: *mut c_void) {
        // SAFETY: a live `Blocks` (the caller's promise), only ever shared.
        let blocks = unsafe { &*(opaque as *const Blocks) };
        blocks.free(ptr);
    }

    /// A decompression context of the C decoder.
    pub struct Ctx {
        stream: *mut ZSTD_DStream,
        /// What the decoder allocated: dropped after `stream` is freed (`drop` frees it, and the
        /// fields are dropped after `drop`). An `Arc` because the decoder holds its address: what an
        /// `Arc` points at does not move, and moving the `Arc` asserts nothing about it (as moving a
        /// `Box` would).
        blocks: Arc<Blocks>,
    }

    // SAFETY: a context holds no thread-local state and is used by one thread at a time (`&mut`);
    // `Blocks` is Send and Sync.
    unsafe impl Send for Ctx {}

    impl Ctx {
        /// None if the decoder could not allocate one.
        pub fn new() -> Option<Ctx> {
            let blocks = Arc::new(Blocks::default());
            let mem = ZSTD_customMem {
                customAlloc: Some(alloc),
                customFree: Some(dealloc),
                opaque: Arc::as_ptr(&blocks) as *mut c_void,
            };
            // SAFETY: `alloc` and `dealloc` with the context's `Blocks`, which lives until the
            // stream is freed (`Ctx`'s field, or dropped here if there is no stream).
            let stream = unsafe { ZSTD_createDStream_advanced(mem) };
            (!stream.is_null()).then_some(Ctx { stream, blocks })
        }

        /// One call of the streaming decoder: `(written, read, hint)`, or None on corrupt data.
        pub fn step(&mut self, dst: &mut [u8], src: &[u8]) -> Option<(usize, usize, usize)> {
            let mut output = ZSTD_outBuffer {
                dst: dst.as_mut_ptr() as *mut c_void,
                size: dst.len(),
                pos: 0,
            };
            let mut input = ZSTD_inBuffer {
                src: src.as_ptr() as *const c_void,
                size: src.len(),
                pos: 0,
            };
            // SAFETY: a live context; the buffers describe `dst` and `src` with their lengths, and
            // are this frame's own variables.
            let r = unsafe { ZSTD_decompressStream(self.stream, &mut output, &mut input) };
            // SAFETY: takes no pointers and has no preconditions.
            (unsafe { ZSTD_isError(r) } == 0).then_some((output.pos, input.pos, r))
        }

        /// The context's blocks, which outlive it in the `Arc` (the tests look at them).
        #[cfg(test)]
        pub fn blocks(&self) -> Arc<Blocks> {
            self.blocks.clone()
        }
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            // SAFETY: this context, freed once, while `blocks` is alive.
            unsafe {
                ZSTD_freeDStream(self.stream);
            }
            // (what it did not free would go with `blocks` all the same)
            debug_assert!(
                self.blocks.map().is_empty(),
                "the decoder kept blocks it was given"
            );
        }
    }
}

/// The wasm decoder's malloc and free (`v86_malloc`, `v86_free`): a block of `std::alloc` with a
/// 16-byte header that holds its size. (Built natively for the tests.)
#[cfg(any(target_arch = "wasm32", test))]
mod heap_block {
    use std::alloc::{self, Layout};

    /// The alignment of a block, and the size of its header.
    pub const ALIGN: usize = 16;

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(ALIGN)?, ALIGN).ok()
    }

    /// A 16-byte-aligned block of `size` bytes, for `free`. Null if it could not allocate it (zstd
    /// checks).
    pub fn malloc(size: usize) -> *mut u8 {
        let Some(layout) = layout(size) else {
            return std::ptr::null_mut();
        };
        // SAFETY: the layout is never zero-sized (ALIGN bytes of header at least).
        let base = unsafe { alloc::alloc(layout) };
        if base.is_null() {
            return base;
        }
        // SAFETY: the header, at the start of what was just allocated, aligned for a usize; the
        // block starts ALIGN bytes in, inside the allocation.
        unsafe {
            (base as *mut usize).write(size);
            base.add(ALIGN)
        }
    }

    /// # Safety
    /// `ptr` must come from `malloc` (and not be freed yet), or be null.
    pub unsafe fn free(ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: `ptr` came from `malloc` (the caller's promise), so its header is before it.
        let (base, size) = unsafe {
            let base = ptr.sub(ALIGN);
            (base, (base as *const usize).read())
        };
        let layout = layout(size).expect("the layout malloc made");
        // SAFETY: allocated by `malloc` with this layout.
        unsafe { alloc::dealloc(base, layout) }
    }
}

/// Bytes decompressed before each call of the sink, at most (and the size of the output buffer).
const EACH: usize = 128 << 10;

/// How a `feed_each` ended.
enum Fed {
    /// All of the input was consumed
    Done,
    /// `input` is not valid zstd
    Corrupt,
    /// the sink returned false
    Stopped,
}

/// Streaming decoder: compressed bytes in, decompressed chunks out. Frames follow one another, and
/// one may be split between `feed`s at any byte.
pub struct Stream {
    /// None if the decoder could not allocate it (`feed` fails)
    ctx: Option<sys::Ctx>,
    buf: Vec<u8>,
    /// The input so far ends inside a frame
    mid_frame: bool,
}

impl Default for Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream {
    pub fn new() -> Stream {
        Stream {
            ctx: sys::Ctx::new(),
            buf: vec![0; EACH],
            mid_frame: false,
        }
    }

    /// Decompress `input`, passing output to `sink`. False on corrupt data.
    pub fn feed(&mut self, input: &[u8], sink: &mut dyn FnMut(&[u8])) -> bool {
        matches!(
            self.feed_each(input, &mut |piece| {
                sink(piece);
                true
            }),
            Fed::Done
        )
    }

    /// `feed`, until `sink` returns false.
    fn feed_each(&mut self, input: &[u8], sink: &mut dyn FnMut(&[u8]) -> bool) -> Fed {
        let Some(ctx) = self.ctx.as_mut() else {
            return Fed::Corrupt;
        };
        let mut rest = input;
        if rest.is_empty() {
            return Fed::Done;
        }
        loop {
            let Some((written, read, hint)) = ctx.step(&mut self.buf, rest) else {
                return Fed::Corrupt;
            };
            // (a hint of 0: a frame ended and all of it was written out)
            self.mid_frame = hint != 0;
            rest = &rest[read..];
            if written > 0 && !sink(&self.buf[..written]) {
                return Fed::Stopped;
            }
            if read == 0 && written == 0 && !rest.is_empty() {
                return Fed::Corrupt; // no progress with input left (zstd does not do that)
            }
            // Done with this input once it is consumed and nothing more is buffered inside: the
            // output buffer was not filled, or the frame ended (a hint of 0: all of it is out; a
            // call with no input would start reading the next frame's header).
            if rest.is_empty() && (written < self.buf.len() || hint == 0) {
                return Fed::Done;
            }
        }
    }
}

/// Every frame of `data`, decompressed.
pub fn decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut s = Stream::new();
    let fed = s.feed_each(data, &mut |piece| {
        out.extend_from_slice(piece);
        true
    });
    if !matches!(fed, Fed::Done) || s.mid_frame {
        return None; // corrupt, or cut off inside a frame
    }
    Some(out)
}

/// How `decompress_each` ended.
#[derive(Debug, PartialEq)]
pub enum Ended {
    Done,
    /// `data` is not valid zstd (what `sink` was given before that is of no use)
    Corrupt,
    /// `sink` returned false
    Stopped,
}

/// Every frame of `data`, decompressed, given to `sink` in pieces of at most 128 KiB: the
/// decompressed contents are never all in memory (as `decompress` has them).
pub fn decompress_each(data: &[u8], sink: &mut dyn FnMut(&[u8]) -> bool) -> Ended {
    let mut s = Stream::new();
    match s.feed_each(data, sink) {
        Fed::Done if s.mid_frame => Ended::Corrupt, // cut off inside a frame
        Fed::Done => Ended::Done,
        Fed::Corrupt => Ended::Corrupt,
        Fed::Stopped => Ended::Stopped,
    }
}

/// A reader of the decompressed contents of a zstd stream (of every frame of it). Corrupt or cut-off
/// data is an `InvalidData` error.
pub fn reader<'a, R: Read + 'a>(r: R) -> std::io::Result<Box<dyn Read + 'a>> {
    let ctx =
        sys::Ctx::new().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
    Ok(Box::new(Reader {
        inner: r,
        ctx,
        input: vec![0; EACH],
        in_pos: 0,
        in_len: 0,
        buf: vec![0; EACH],
        out_pos: 0,
        out_len: 0,
        mid_frame: false,
        eof: false,
    }))
}

struct Reader<R> {
    inner: R,
    ctx: sys::Ctx,
    /// Compressed bytes read from `inner`: `input[in_pos..in_len]` are not yet decoded
    input: Vec<u8>,
    in_pos: usize,
    in_len: usize,
    /// Decompressed bytes: `buf[out_pos..out_len]` are not yet given to the caller
    buf: Vec<u8>,
    out_pos: usize,
    out_len: usize,
    /// The input so far ends inside a frame
    mid_frame: bool,
    /// `inner` is at its end
    eof: bool,
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, dst: &mut [u8]) -> std::io::Result<usize> {
        let invalid =
            |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_string());
        loop {
            if self.out_pos < self.out_len {
                let n = dst.len().min(self.out_len - self.out_pos);
                dst[..n].copy_from_slice(&self.buf[self.out_pos..self.out_pos + n]);
                self.out_pos += n;
                return Ok(n);
            }
            if dst.is_empty() {
                return Ok(0);
            }
            // The last output filled the buffer in the middle of a frame: more may be inside, even
            // with no input left (not so if the frame ended: the next call would start the next).
            let drain = self.out_len == self.buf.len() && self.mid_frame;
            if self.in_pos == self.in_len && !drain {
                if self.eof {
                    return Ok(0);
                }
                let n = loop {
                    match self.inner.read(&mut self.input) {
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {},
                        r => break r?,
                    }
                };
                if n == 0 {
                    self.eof = true;
                    if self.mid_frame {
                        return Err(invalid("zstd stream cut off inside a frame"));
                    }
                    return Ok(0);
                }
                (self.in_pos, self.in_len) = (0, n);
            }
            let Some((written, read, hint)) = self
                .ctx
                .step(&mut self.buf, &self.input[self.in_pos..self.in_len])
            else {
                return Err(invalid("corrupt zstd data"));
            };
            if read == 0 && written == 0 && self.in_pos < self.in_len {
                return Err(invalid("corrupt zstd data")); // no progress with input left
            }
            self.mid_frame = hint != 0;
            self.in_pos += read;
            (self.out_pos, self.out_len) = (0, written);
        }
    }
}

/// Whether `data` starts with the zstd frame magic.
pub fn is_zstd(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == [0x28, 0xb5, 0x2f, 0xfd]
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
