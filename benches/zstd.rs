//! What decoding zstd costs the host, timed with criterion: frames of 64 KiB, 4 MiB and 32 MiB
//! (an 8 MiB window, as the blobs of an image have), made by libzstd here, decoded whole
//! (`zstd::decompress`) and in pieces (`zstd::decompress_each`, as blobs are spilled).
//!
//!   cargo bench --bench zstd
//!   cargo bench --bench zstd -- decompress_each   # one of the two

use std::hint::black_box;

use armless::zstd;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// `len` bytes that compress about as well as a program's: runs, repeats of what came before, and
/// noise (xorshift, so every run is the same).
fn contents(len: usize) -> Vec<u8> {
    let mut x = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move |n: u64| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x % n
    };
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let n = (1 + next(64) as usize).min(len - v.len());
        match next(3) {
            0 => v.extend((0..n).map(|_| b'a' + next(16) as u8)),
            1 if v.len() > 4096 => {
                let from = v.len() - 1 - next(4096) as usize;
                for i in 0..n {
                    v.push(v[from + i % (v.len() - from)]);
                }
            },
            _ => v.extend(std::iter::repeat_n(next(256) as u8, n)),
        }
    }
    v
}

/// `data` as one zstd frame: level 3, a window of at most 8 MiB, no content size in the header (as
/// a streaming compressor writes it: the decoder allocates its whole window).
fn compress(data: &[u8]) -> Vec<u8> {
    use zstd_sys::ZSTD_cParameter::*;
    use zstd_sys::*;
    // SAFETY: takes no pointers.
    let mut out = vec![0u8; unsafe { ZSTD_compressBound(data.len()) }];
    // SAFETY: a context of our own, freed once; `out` and `data` with their lengths.
    unsafe {
        let cctx = ZSTD_createCCtx();
        assert!(!cctx.is_null());
        for (param, value) in [
            (ZSTD_c_compressionLevel, 3),
            (ZSTD_c_windowLog, 23),
            (ZSTD_c_contentSizeFlag, 0),
        ] {
            assert_eq!(ZSTD_isError(ZSTD_CCtx_setParameter(cctx, param, value)), 0);
        }
        let n = ZSTD_compress2(
            cctx,
            out.as_mut_ptr().cast(),
            out.len(),
            data.as_ptr().cast(),
            data.len(),
        );
        ZSTD_freeCCtx(cctx);
        assert_eq!(ZSTD_isError(n), 0);
        out.truncate(n);
    }
    out
}

fn decode(c: &mut Criterion) {
    let mut g = c.benchmark_group("zstd");
    for (name, len) in [
        ("64 KiB", 64 << 10),
        ("4 MiB", 4 << 20),
        ("32 MiB", 32 << 20),
    ] {
        let data = contents(len);
        let frame = compress(&data);
        assert_eq!(zstd::decompress(&frame).as_deref(), Some(&data[..]));
        g.throughput(Throughput::Bytes(len as u64));
        if len >= 32 << 20 {
            g.sample_size(20); // (tens of milliseconds each)
        }
        g.bench_with_input(BenchmarkId::new("decompress", name), &frame, |b, f| {
            b.iter(|| black_box(zstd::decompress(black_box(f))))
        });
        g.bench_with_input(BenchmarkId::new("decompress_each", name), &frame, |b, f| {
            b.iter(|| {
                let mut sum = 0usize;
                let ended = zstd::decompress_each(black_box(f), &mut |piece| {
                    sum += piece.len();
                    true
                });
                black_box((ended, sum))
            })
        });
    }
    g.finish();
}

criterion_group!(benches, decode);
criterion_main!(benches);
