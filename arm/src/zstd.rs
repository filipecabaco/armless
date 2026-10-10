//! zstd decompression exports, identical to v86's `src/rust/zstd.rs`, so the
//! unchanged JS (state.js restoring `.zst` snapshots, the 9p blob loader and
//! the zstd worker) can use armless.wasm exactly like v86.wasm. The C decoder
//! (`lib/zstd/zstddeclib.c`) is compiled to wasm32 and linked in by
//! `build-wasm.sh`.

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
    let layout = alloc::Layout::from_size_align(size as usize + MALLOC_ALIGN, MALLOC_ALIGN).unwrap();
    let addr = alloc::alloc(layout);
    *(addr as *mut u32) = size;
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
    let size = *(base as *mut u32);
    let layout = alloc::Layout::from_size_align(size as usize + MALLOC_ALIGN, MALLOC_ALIGN).unwrap();
    alloc::dealloc(base as *mut u8, layout)
}

pub struct ZstdContext {
    ctx: u32,
    src: *mut u8,
    src_size: u32,
    src_pos: u32,
}

/// # Safety
/// Exported to JS.
#[no_mangle]
pub unsafe extern "C" fn zstd_create_ctx(src_size: u32) -> *mut ZstdContext {
    let src = alloc::alloc(alloc::Layout::from_size_align(src_size.max(1) as usize, 1).unwrap());
    let ctx = ZSTD_createDStream();
    let result = alloc::alloc(alloc::Layout::new::<ZstdContext>()) as *mut ZstdContext;
    result.write(ZstdContext {
        ctx,
        src,
        src_size,
        src_pos: 0,
    });
    result
}

/// # Safety
/// `ctx` must come from `zstd_create_ctx`.
#[no_mangle]
pub unsafe extern "C" fn zstd_get_src_ptr(ctx: *mut ZstdContext) -> *mut u8 {
    (*ctx).src
}

/// # Safety
/// `ctx` must come from `zstd_create_ctx`.
#[no_mangle]
pub unsafe extern "C" fn zstd_free_ctx(ctx: *mut ZstdContext) {
    alloc::dealloc(
        (*ctx).src,
        alloc::Layout::from_size_align((*ctx).src_size.max(1) as usize, 1).unwrap(),
    );
    ZSTD_freeDStream((*ctx).ctx);
    alloc::dealloc(ctx as *mut u8, alloc::Layout::new::<ZstdContext>());
}

/// Decompress exactly `length` bytes; returns null on error or short input.
///
/// # Safety
/// `ctx` must come from `zstd_create_ctx`.
#[no_mangle]
pub unsafe extern "C" fn zstd_read(ctx: *mut ZstdContext, length: u32) -> *mut u8 {
    let dst = alloc::alloc(alloc::Layout::from_size_align(length.max(1) as usize, 1).unwrap());
    let mut dst_pos = 0;
    let result = ZSTD_decompressStream_simpleArgs(
        (*ctx).ctx,
        dst,
        length,
        &mut dst_pos,
        (*ctx).src,
        (*ctx).src_size,
        &mut (*ctx).src_pos,
    );
    if ZSTD_isError(result) || dst_pos != length {
        zstd_read_free(dst, length);
        return std::ptr::null_mut::<u8>();
    }
    dst
}

/// # Safety
/// `ptr`/`length` must come from `zstd_read`.
#[no_mangle]
pub unsafe extern "C" fn zstd_read_free(ptr: *mut u8, length: u32) {
    alloc::dealloc(ptr, alloc::Layout::from_size_align(length.max(1) as usize, 1).unwrap());
}
