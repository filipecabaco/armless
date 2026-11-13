//! zstd decompression: 9p blobs (`.bin.zst`), compressed snapshots and
//! zstd-compressed kernels.
//!
//! Wasm and native hosts decode with the same C decoder (`lib/zstd/zstddeclib.c`): `build-wasm.sh`
//! links it into the wasm module, `build.rs` compiles it into native hosts. `sys` is the only part
//! that differs (the wasm ABI passes pointers and sizes as `u32`); everything else is shared.

use std::alloc;
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
        // SAFETY: as malloc: any size.
        unsafe { super::malloc(size as usize) as u32 }
    }

    /// # Safety
    /// `addr` must come from `v86_malloc`.
    #[no_mangle]
    pub unsafe extern "C" fn v86_free(addr: u32) {
        // SAFETY: `addr` came from `v86_malloc` (the caller's promise).
        unsafe { super::free(addr as *mut u8) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod sys {
    use std::ffi::{c_uint, c_void};

    // SAFETY: the C decoder `build.rs` compiles, with these signatures (size_t is usize). Making a
    // stream and telling an error code from a size take no pointers and have no preconditions.
    unsafe extern "C" {
        safe fn ZSTD_createDStream() -> *mut c_void;
        fn ZSTD_freeDStream(ctx: *mut c_void) -> usize;
        fn ZSTD_decompressStream_simpleArgs(
            ctx: *mut c_void,
            dst: *mut u8,
            dst_capacity: usize,
            dst_pos: *mut usize,
            src: *const u8,
            src_size: usize,
            src_pos: *mut usize,
        ) -> usize;
        safe fn ZSTD_isError(code: usize) -> c_uint;
    }

    /// A decompression context of the C decoder.
    pub struct Ctx(*mut c_void);

    // SAFETY: a context holds no thread-local state and is used by one thread at a time (`&mut`).
    unsafe impl Send for Ctx {}

    impl Ctx {
        /// None if the decoder could not allocate one.
        pub fn new() -> Option<Ctx> {
            let ctx = ZSTD_createDStream();
            (!ctx.is_null()).then_some(Ctx(ctx))
        }

        /// One call of the streaming decoder: `(written, read, hint)`, or None on corrupt data.
        pub fn step(&mut self, dst: &mut [u8], src: &[u8]) -> Option<(usize, usize, usize)> {
            let (mut dst_pos, mut src_pos) = (0usize, 0usize);
            // SAFETY: a live context; `dst` and `src` are ours with their lengths, and the positions
            // are this frame's own variables.
            let r = unsafe {
                ZSTD_decompressStream_simpleArgs(
                    self.0,
                    dst.as_mut_ptr(),
                    dst.len(),
                    &mut dst_pos,
                    src.as_ptr(),
                    src.len(),
                    &mut src_pos,
                )
            };
            (ZSTD_isError(r) == 0).then_some((dst_pos, src_pos, r))
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
    pub unsafe extern "C" fn v86_malloc(size: usize) -> *mut c_void {
        // SAFETY: as malloc: any size.
        unsafe { super::malloc(size) as *mut c_void }
    }

    /// # Safety
    /// `ptr` must come from `v86_malloc`, or be null.
    #[no_mangle]
    pub unsafe extern "C" fn v86_free(ptr: *mut c_void) {
        // SAFETY: `ptr` came from `v86_malloc` (the caller's promise).
        unsafe { super::free(ptr as *mut u8) }
    }
}

const MALLOC_ALIGN: usize = 16;

/// What `malloc` records in the header before a block: its size, and how it was allocated.
const KIND_HEAP: usize = 0;
#[cfg(unix)]
const KIND_MMAP: usize = 1;

/// From this size a block is its own anonymous mapping, which `free` unmaps: the pages go back to
/// the system (glibc's `free` keeps what a decoder window touched resident).
#[cfg(unix)]
const MMAP_FROM: usize = 128 << 10;

/// The size of the mapping of a block of `size` bytes (header included, whole pages).
#[cfg(unix)]
fn mapping_len(size: usize) -> Option<usize> {
    // SAFETY: sysconf takes no pointers.
    let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    size.checked_add(MALLOC_ALIGN)?
        .checked_next_multiple_of(page)
}

/// malloc for the C decoder: a 16-byte-aligned block, a header before it with its size and how it
/// was allocated (`std::alloc`, or on unix an anonymous mapping from `MMAP_FROM` bytes). Null if it
/// could not allocate (zstd checks).
///
/// # Safety
/// The result is for `free` only.
unsafe fn malloc(size: usize) -> *mut u8 {
    #[cfg(unix)]
    let mut kind = KIND_HEAP;
    #[allow(unused_mut)]
    let mut base: *mut u8 = std::ptr::null_mut();
    #[cfg(unix)]
    if size >= MMAP_FROM {
        let Some(len) = mapping_len(size) else {
            return std::ptr::null_mut();
        };
        // SAFETY: a new private anonymous mapping, at an address the system chooses.
        let addr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if addr == libc::MAP_FAILED {
            return std::ptr::null_mut();
        }
        kind = KIND_MMAP;
        base = addr as *mut u8; // (page-aligned)
    }
    #[cfg(not(unix))]
    let kind = KIND_HEAP;
    if base.is_null() {
        let Some(total) = size.checked_add(MALLOC_ALIGN) else {
            return std::ptr::null_mut();
        };
        let Ok(layout) = alloc::Layout::from_size_align(total, MALLOC_ALIGN) else {
            return std::ptr::null_mut();
        };
        // SAFETY: the layout is never zero-sized (MALLOC_ALIGN bytes of header at least).
        base = unsafe { alloc::alloc(layout) };
        if base.is_null() {
            return base;
        }
    }
    // SAFETY: the header (two usizes), at the start of what was just allocated, aligned for a
    // usize; the block the caller gets starts MALLOC_ALIGN bytes in, inside the allocation.
    unsafe {
        (base as *mut usize).write(size);
        (base as *mut usize).add(1).write(kind);
        base.add(MALLOC_ALIGN)
    }
}

/// # Safety
/// `ptr` must come from `malloc`, or be null.
unsafe fn free(ptr: *mut u8) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` came from `malloc` (the caller's promise), so its header is before it.
    let (base, size, kind) = unsafe {
        let base = ptr.sub(MALLOC_ALIGN);
        let header = base as *const usize;
        (base, header.read(), header.add(1).read())
    };
    #[cfg(unix)]
    if kind == KIND_MMAP {
        let len = mapping_len(size).expect("the mapping malloc made");
        // SAFETY: the mapping `malloc` made at `base`, with this length, unmapped once.
        unsafe { libc::munmap(base as *mut libc::c_void, len) };
        return;
    }
    debug_assert_eq!(kind, KIND_HEAP);
    let layout = alloc::Layout::from_size_align(size + MALLOC_ALIGN, MALLOC_ALIGN)
        .expect("the layout malloc made");
    // SAFETY: allocated by `malloc` with this layout.
    unsafe { alloc::dealloc(base, layout) }
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
mod tests {
    use super::*;

    /// `zstd -19 --no-check` of `pattern()`: one frame.
    const BIG: [u8; 297] = [
        0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0xe0, 0x93, 0x04, 0x00, 0x2c, 0x08, 0x00, 0xb4, 0x0f, 0x00,
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e,
        0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
        0x2e, 0x2f, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c,
        0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b,
        0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a,
        0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
        0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78,
        0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
        0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92, 0x93, 0x94, 0x95, 0x96,
        0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5,
        0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1, 0xb2, 0xb3, 0xb4,
        0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xc1, 0xc2, 0xc3,
        0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0xd0, 0xd1, 0xd2,
        0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf, 0xe0, 0xe1,
        0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef, 0xf0,
        0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0x01, 0x00, 0x7b, 0x81, 0x7f,
        0x7f, 0x6e, 0xa4, 0x54, 0x00, 0x00, 0x00, 0x01, 0x00, 0xfd, 0xff, 0xd1, 0xff, 0xb9, 0x06,
        0x02, 0x45, 0x00, 0x00, 0x00, 0x01, 0x00, 0xdd, 0x13, 0x1d, 0x00, 0x01,
    ];

    /// `zstd -19 --no-check` of `HELLO`: one frame.
    const SMALL: [u8; 21] = [
        0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x0c, 0x61, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x2c,
        0x20, 0x7a, 0x73, 0x74, 0x64, 0x0a,
    ];

    const HELLO: &[u8] = b"hello, zstd\n";

    /// 300000 bytes (more than two output pieces) that compress to a few hundred.
    fn pattern() -> Vec<u8> {
        (0..300_000).map(|i| (i % 251) as u8).collect()
    }

    fn frames() -> (Vec<u8>, Vec<u8>) {
        let input = [&BIG[..], &SMALL, &BIG].concat();
        let want = [pattern(), HELLO.to_vec(), pattern()].concat();
        (input, want)
    }

    #[test]
    fn round_trip_and_multi_frame() {
        assert!(is_zstd(&BIG) && !is_zstd(b"nope"));
        assert_eq!(decompress(&BIG).unwrap(), pattern());
        assert_eq!(decompress(&SMALL).unwrap(), HELLO);
        let (input, want) = frames();
        assert_eq!(decompress(&input).unwrap(), want);
        assert_eq!(decompress(&[]).unwrap(), b"");
    }

    /// One zstd frame (RFC 8878) of raw blocks of at most 128 KiB each, no checksum: a window of
    /// 128 KiB, so the largest block is 128 KiB.
    fn raw_frame(data: &[u8]) -> Vec<u8> {
        let mut f = vec![0x28, 0xb5, 0x2f, 0xfd]; // magic
        f.push(0x00); // Frame_Header_Descriptor: no size, no checksum, no dictionary
        f.push(0x38); // Window_Descriptor: exponent 7, mantissa 0: a 128 KiB window
        let blocks: Vec<&[u8]> =
            if data.is_empty() { vec![data] } else { data.chunks(128 << 10).collect() };
        for (i, block) in blocks.iter().enumerate() {
            let last = u32::from(i + 1 == blocks.len());
            // Block_Header: Last_Block bit, Block_Type 0 (raw) in 2 bits, Block_Size in 21 bits
            let header = last | ((block.len() as u32) << 3);
            f.extend_from_slice(&header.to_le_bytes()[..3]);
            f.extend_from_slice(block);
        }
        f
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Frames whose decompressed size is a multiple of the output buffer, alone and followed by
    /// another frame, with what they decompress to.
    fn boundary_cases() -> Vec<(String, Vec<u8>, Vec<u8>)> {
        let k = 128 << 10;
        let mut cases = Vec::new();
        for len in [k, 2 * k, 3 * k] {
            cases.push((format!("{len} bytes"), raw_frame(&bytes(len)), bytes(len)));
        }
        let tails = [
            ("raw 100", raw_frame(&bytes(100)), bytes(100)),
            ("small", SMALL.to_vec(), HELLO.to_vec()),
            ("raw 128 KiB", raw_frame(&bytes(k)), bytes(k)),
        ];
        for (name, frame, want) in tails {
            for len in [k, 2 * k] {
                cases.push((
                    format!("{len} bytes, then {name}"),
                    [raw_frame(&bytes(len)), frame.clone()].concat(),
                    [bytes(len), want.clone()].concat(),
                ));
            }
        }
        cases
    }

    #[test]
    fn a_frame_that_ends_on_a_full_output_buffer_is_complete() {
        for (name, input, want) in boundary_cases() {
            assert_eq!(decompress(&input).as_deref(), Some(&want[..]), "{name}");

            let mut got = Vec::new();
            let ended = decompress_each(&input, &mut |piece| {
                assert!(piece.len() <= 128 << 10);
                got.extend_from_slice(piece);
                true
            });
            assert_eq!(ended, Ended::Done, "{name}");
            assert_eq!(got, want, "{name}");

            for step in [1000, 128 << 10, input.len()] {
                let mut r = reader(Dribble(&input, step, false)).unwrap();
                let mut got = Vec::new();
                r.read_to_end(&mut got)
                    .unwrap_or_else(|e| panic!("{name}, step {step}: {e}"));
                assert_eq!(got, want, "{name}, step {step}");

                let mut s = Stream::new();
                let mut got = Vec::new();
                for chunk in input.chunks(step) {
                    assert!(
                        s.feed(chunk, &mut |b| got.extend_from_slice(b)),
                        "{name}, step {step}"
                    );
                }
                assert_eq!(got, want, "{name}, step {step}");
                assert!(!s.mid_frame, "{name}, step {step}");
            }
        }
    }

    #[test]
    fn a_frame_cut_off_on_a_full_output_buffer_is_refused() {
        let k = 128 << 10;
        let two = raw_frame(&bytes(2 * k));
        let first_block = 6 + 3 + k; // header of the frame, then the first block
        let boundary = raw_frame(&bytes(k));
        let cuts = [
            // the first of two blocks is out (a full buffer), the frame goes on
            two[..first_block].to_vec(),
            // the last block's header, none of its data
            two[..first_block + 3].to_vec(),
            // inside the last block
            two[..two.len() - 1].to_vec(),
            // a frame that ended on a full buffer, then the start of the next one
            [&boundary[..], &SMALL[..2]].concat(),
            [&boundary[..], &SMALL[..SMALL.len() - 1]].concat(),
            // a single block frame cut off inside its data
            boundary[..boundary.len() - 1].to_vec(),
        ];
        for (i, bad) in cuts.iter().enumerate() {
            assert_eq!(decompress(bad), None, "cut {i}");
            assert_eq!(
                decompress_each(bad, &mut |_| true),
                Ended::Corrupt,
                "cut {i}"
            );
            let mut r = reader(Dribble(bad, 4096, false)).unwrap();
            let e = r.read_to_end(&mut Vec::new()).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "cut {i}");
        }
    }

    #[test]
    fn malloc_serves_small_and_large_blocks() {
        for size in [
            0,
            1,
            100,
            (128 << 10) - 1,
            128 << 10,
            8 << 20,
            (8 << 20) + 1,
        ] {
            // SAFETY: blocks from `malloc`, written within their size, freed once.
            unsafe {
                let p = malloc(size);
                assert!(
                    !p.is_null() && (p as usize).is_multiple_of(MALLOC_ALIGN),
                    "size {size}"
                );
                std::ptr::write_bytes(p, 0xa5, size);
                assert!(std::slice::from_raw_parts(p, size)
                    .iter()
                    .all(|&b| b == 0xa5));
                free(p);
            }
        }
        // SAFETY: null is allowed.
        unsafe { free(std::ptr::null_mut()) };
    }

    #[test]
    fn corrupt_input_is_refused() {
        assert_eq!(decompress(b"not zstd at all"), None);
        // cut off inside a frame, and a frame after a good one cut off
        assert_eq!(decompress(&BIG[..BIG.len() - 1]), None);
        assert_eq!(decompress(&BIG[..100]), None);
        assert_eq!(decompress(&[&SMALL[..], &BIG[..100]].concat()), None);
        // garbage after a good frame
        assert_eq!(decompress(&[&SMALL[..], b"garbage!"].concat()), None);
        // a flipped byte, anywhere: an error or other output (the last bytes of a frame with no
        // checksum may not matter), never UB
        for at in 0..BIG.len() {
            let mut bad = BIG;
            bad[at] = !bad[at];
            let _ = decompress(&bad);
            let _ = decompress_each(&bad, &mut |_| true);
        }
        let mut sink = |_: &[u8]| true;
        assert_eq!(
            decompress_each(b"not zstd at all", &mut sink),
            Ended::Corrupt
        );
        assert_eq!(
            decompress_each(&BIG[..BIG.len() - 1], &mut sink),
            Ended::Corrupt
        );
        assert_eq!(decompress_each(&[], &mut sink), Ended::Done);
    }

    #[test]
    fn each_gives_pieces_of_at_most_128_kib() {
        let (input, want) = frames();
        let mut got = Vec::new();
        let mut sizes = Vec::new();
        let ended = decompress_each(&input, &mut |piece| {
            sizes.push(piece.len());
            got.extend_from_slice(piece);
            true
        });
        assert_eq!(ended, Ended::Done);
        assert_eq!(got, want);
        assert!(sizes.iter().all(|&n| n > 0 && n <= 128 << 10), "{sizes:?}");
        // 300000 bytes are three pieces (two full ones), twice, and the small frame's one
        assert_eq!(
            sizes.iter().filter(|&&n| n == 128 << 10).count(),
            4,
            "{sizes:?}"
        );
        assert_eq!(sizes.len(), 7, "{sizes:?}");
    }

    #[test]
    fn each_stops_when_the_sink_says_so() {
        let (input, _) = frames();
        let mut calls = 0;
        let ended = decompress_each(&input, &mut |_| {
            calls += 1;
            calls < 2
        });
        assert_eq!(ended, Ended::Stopped);
        assert_eq!(calls, 2);
    }

    #[test]
    fn stream_takes_input_split_anywhere() {
        let (input, want) = frames();
        for step in [1, 7, 100, input.len()] {
            let mut s = Stream::new();
            let mut got = Vec::new();
            for chunk in input.chunks(step) {
                assert!(
                    s.feed(chunk, &mut |b| got.extend_from_slice(b)),
                    "step {step}"
                );
            }
            assert_eq!(got, want, "step {step}");
        }
        let mut s = Stream::new();
        assert!(!s.feed(b"not zstd at all", &mut |_| {}));
    }

    /// A reader that gives at most `n` bytes per read, and fails with Interrupted once first.
    struct Dribble<'a>(&'a [u8], usize, bool);

    impl Read for Dribble<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !std::mem::replace(&mut self.2, true) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            let n = buf.len().min(self.1).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn reader_reads_every_frame() {
        let (input, want) = frames();
        for step in [1, 13, 4096, input.len()] {
            let mut r = reader(Dribble(&input, step, false)).unwrap();
            let mut got = Vec::new();
            r.read_to_end(&mut got).unwrap();
            assert_eq!(got, want, "step {step}");
        }
        // small reads from the caller too
        let mut r = reader(&input[..]).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 1000];
        loop {
            match r.read(&mut buf).unwrap() {
                0 => break,
                n => got.extend_from_slice(&buf[..n]),
            }
        }
        assert_eq!(got, want);
    }

    #[test]
    fn reader_reports_corrupt_and_cut_off_input() {
        for bad in [&b"not zstd at all"[..], &BIG[..BIG.len() - 1], &BIG[..100]] {
            let mut r = reader(bad).unwrap();
            let e = r.read_to_end(&mut Vec::new()).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
        }
    }
}
