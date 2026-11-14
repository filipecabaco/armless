use super::*;

/// `zstd -19 --no-check` of `pattern()`: one frame.
const BIG: [u8; 297] = [
    0x28, 0xb5, 0x2f, 0xfd, 0xa0, 0xe0, 0x93, 0x04, 0x00, 0x2c, 0x08, 0x00, 0xb4, 0x0f, 0x00, 0x01,
    0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11,
    0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21,
    0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31,
    0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41,
    0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51,
    0x52, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f, 0x60, 0x61,
    0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71,
    0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f, 0x80, 0x81,
    0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91,
    0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f, 0xa0, 0xa1,
    0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad, 0xae, 0xaf, 0xb0, 0xb1,
    0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xbb, 0xbc, 0xbd, 0xbe, 0xbf, 0xc0, 0xc1,
    0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xcb, 0xcc, 0xcd, 0xce, 0xcf, 0xd0, 0xd1,
    0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xdb, 0xdc, 0xdd, 0xde, 0xdf, 0xe0, 0xe1,
    0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xeb, 0xec, 0xed, 0xee, 0xef, 0xf0, 0xf1,
    0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa, 0x01, 0x00, 0x7b, 0x81, 0x7f, 0x7f, 0x6e,
    0xa4, 0x54, 0x00, 0x00, 0x00, 0x01, 0x00, 0xfd, 0xff, 0xd1, 0xff, 0xb9, 0x06, 0x02, 0x45, 0x00,
    0x00, 0x00, 0x01, 0x00, 0xdd, 0x13, 0x1d, 0x00, 0x01,
];

/// `zstd -19 --no-check` of `HELLO`: one frame.
const SMALL: [u8; 21] = [
    0x28, 0xb5, 0x2f, 0xfd, 0x20, 0x0c, 0x61, 0x00, 0x00, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20,
    0x7a, 0x73, 0x74, 0x64, 0x0a,
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
    raw_frame_in(data, 0x38) // exponent 7, mantissa 0: a 128 KiB window
}

/// `raw_frame`, with a window of `window` (a Window_Descriptor), and no content size: the decoder
/// allocates a buffer of the whole window.
fn raw_frame_in(data: &[u8], window: u8) -> Vec<u8> {
    let mut f = vec![0x28, 0xb5, 0x2f, 0xfd]; // magic
    f.push(0x00); // Frame_Header_Descriptor: no size, no checksum, no dictionary
    f.push(window);
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
fn the_wasm_malloc_serves_small_and_large_blocks() {
    use super::heap_block::{free, malloc, ALIGN};
    for size in [
        0,
        1,
        100,
        (128 << 10) - 1,
        128 << 10,
        8 << 20,
        (8 << 20) + 1,
    ] {
        let p = malloc(size);
        // SAFETY: blocks from `malloc`, written within their size, freed once.
        unsafe {
            assert!(
                !p.is_null() && (p as usize).is_multiple_of(ALIGN),
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

// ---------------------------------------------------------------------------------------------
// Frames from the C compressor (zstd-sys has it too; only the tests use it)

/// How `compress` makes a frame.
#[derive(Clone, Copy, Debug)]
struct Params {
    level: i32,
    /// log2 of the window (0: the level's own)
    window_log: i32,
    content_size: bool,
    checksum: bool,
}

const PLAIN: Params = Params {
    level: 3,
    window_log: 0,
    content_size: true,
    checksum: false,
};

/// `data` as one zstd frame, made by libzstd.
fn compress(data: &[u8], p: Params) -> Vec<u8> {
    use zstd_sys::ZSTD_cParameter::*;
    use zstd_sys::*;
    // SAFETY: takes no pointers.
    let mut out = vec![0u8; unsafe { ZSTD_compressBound(data.len()) }];
    // SAFETY: a context of our own, freed once; `out` and `data` with their lengths.
    unsafe {
        let cctx = ZSTD_createCCtx();
        assert!(!cctx.is_null());
        for (param, value) in [
            (ZSTD_c_compressionLevel, p.level),
            (ZSTD_c_windowLog, p.window_log),
            (ZSTD_c_contentSizeFlag, i32::from(p.content_size)),
            (ZSTD_c_checksumFlag, i32::from(p.checksum)),
        ] {
            assert_eq!(
                ZSTD_isError(ZSTD_CCtx_setParameter(cctx, param, value)),
                0,
                "{p:?}"
            );
        }
        let n = ZSTD_compress2(
            cctx,
            out.as_mut_ptr().cast(),
            out.len(),
            data.as_ptr().cast(),
            data.len(),
        );
        ZSTD_freeCCtx(cctx);
        assert_eq!(ZSTD_isError(n), 0, "{p:?}");
        out.truncate(n);
    }
    out
}

/// A skippable frame (RFC 8878 3.1.2) of `payload`.
fn skippable(n: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = (0x184d_2a50u32 | u32::from(n & 15)).to_le_bytes().to_vec();
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// xorshift64*: the tests' own reproducible randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// `len` bytes that compress somewhat: runs, repeats of what came before, and noise.
fn text(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut v = Vec::with_capacity(len);
    while v.len() < len {
        let n = (1 + rng.below(64)).min(len - v.len());
        match rng.below(3) {
            0 => v.extend((0..n).map(|_| b'a' + rng.below(16) as u8)),
            1 if v.len() > 64 => {
                let from = rng.below(v.len() - n.min(v.len()));
                for i in 0..n {
                    v.push(v[from + i % (v.len() - from)]);
                }
            },
            _ => v.extend(std::iter::repeat_n(rng.next() as u8, n)),
        }
    }
    v
}

/// The decoded contents of `input` through every way in: `decompress`, `decompress_each`, the
/// reader (in pieces of `step`) and `Stream::feed` (in chunks of `step`). They must agree, on the
/// contents and on whether `input` is good: the result is theirs.
fn decode_all_ways(input: &[u8], step: usize) -> Option<Vec<u8>> {
    let whole = decompress(input);

    let mut got = Vec::new();
    let each = match decompress_each(input, &mut |piece| {
        assert!(!piece.is_empty() && piece.len() <= EACH);
        got.extend_from_slice(piece);
        true
    }) {
        Ended::Done => Some(got),
        Ended::Corrupt => None,
        Ended::Stopped => unreachable!("the sink never stops"),
    };

    let mut got = Vec::new();
    let read = reader(Dribble(input, step, false))
        .unwrap()
        .read_to_end(&mut got)
        .map(|_| got)
        .map_err(|e| assert_eq!(e.kind(), std::io::ErrorKind::InvalidData))
        .ok();

    let mut s = Stream::new();
    let mut got = Vec::new();
    let fed = input
        .chunks(step)
        .all(|chunk| s.feed(chunk, &mut |b| got.extend_from_slice(b)));
    let streamed = (fed && !s.mid_frame).then_some(got);

    assert_eq!(whole, each, "decompress and decompress_each, step {step}");
    assert_eq!(whole, read, "decompress and the reader, step {step}");
    assert_eq!(whole, streamed, "decompress and Stream::feed, step {step}");
    whole
}

/// Every combination of content size and checksum, at two levels.
fn param_sets() -> Vec<Params> {
    let mut v = Vec::new();
    for level in [1, 19] {
        for content_size in [false, true] {
            for checksum in [false, true] {
                v.push(Params {
                    level,
                    window_log: 0,
                    content_size,
                    checksum,
                });
            }
        }
    }
    v
}

#[test]
fn round_trip_over_sizes_and_frame_kinds() {
    let k = 128 << 10;
    let sizes = [
        0,
        1,
        2,
        100,
        4095,
        4096,
        k - 1,
        k,
        k + 1,
        2 * k,
        3 * k + 7,
        3 << 20,
    ];
    for (i, &len) in sizes.iter().enumerate() {
        let data = text(len, i as u64);
        // level 19 is slow on megabytes, and finds nothing more to test there
        let sets: Vec<Params> = param_sets()
            .into_iter()
            .filter(|p| p.level < 19 || len <= 4 * k)
            .collect();
        for p in sets {
            let frame = compress(&data, p);
            for step in [1000, 64 << 10, frame.len().max(1)] {
                if step == 1000 && len > 4 * k {
                    continue; // (enough of small steps below)
                }
                let got = decode_all_ways(&frame, step);
                assert!(
                    got.as_deref() == Some(&data[..]),
                    "{len} bytes, {p:?}, step {step}"
                );
            }
        }
        let raw = raw_frame(&data);
        assert_eq!(
            decode_all_ways(&raw, 4096).as_deref(),
            Some(&data[..]),
            "raw, {len} bytes"
        );
    }
}

#[test]
fn a_large_window_round_trips() {
    // 8 MiB window: the decoder's largest block is a mapping of 8 MiB and a bit
    let data = text(12 << 20, 7);
    let p = Params {
        window_log: 23,
        content_size: false,
        ..PLAIN
    };
    let frame = compress(&data, p);
    assert_eq!(decode_all_ways(&frame, 1 << 20).as_deref(), Some(&data[..]));
}

/// Frames of every kind one after another, with skippable frames among them, and what they
/// decompress to.
fn mixed_frames() -> (Vec<u8>, Vec<u8>) {
    let mut input = skippable(0, b"before");
    let mut want = Vec::new();
    for (i, p) in param_sets().into_iter().enumerate() {
        let data = text(1000 + 40_000 * i, 100 + i as u64);
        input.extend(compress(&data, p));
        want.extend(&data);
        input.extend(skippable(i as u8, &vec![i as u8; i * 3]));
    }
    input.extend(raw_frame(&bytes(200_000)));
    want.extend(bytes(200_000));
    input.extend(compress(b"", PLAIN));
    input.extend(SMALL);
    want.extend(HELLO);
    input.extend(skippable(15, b""));
    (input, want)
}

#[test]
fn concatenated_and_skippable_frames() {
    let (input, want) = mixed_frames();
    for step in [1, 4096, input.len()] {
        assert_eq!(
            decode_all_ways(&input, step).as_deref(),
            Some(&want[..]),
            "step {step}"
        );
    }
    // skippable frames alone decompress to nothing
    let only = [skippable(3, b"abc"), skippable(4, b"")].concat();
    assert_eq!(decode_all_ways(&only, 1).as_deref(), Some(&[][..]));
    // a skippable frame cut off is not a whole stream
    let cut = skippable(1, b"payload");
    assert_eq!(decode_all_ways(&cut[..cut.len() - 1], 3), None);
}

#[test]
fn a_bad_checksum_is_refused() {
    let data = text(50_000, 9);
    let mut frame = compress(
        &data,
        Params {
            checksum: true,
            ..PLAIN
        },
    );
    let last = frame.len() - 1;
    frame[last] ^= 1; // (the last 4 bytes are the checksum)
    assert_eq!(decode_all_ways(&frame, 4096), None);
}

#[test]
fn stream_takes_every_chunking() {
    // a few frames that decompress to more than one output buffer, small enough compressed to feed
    // a byte at a time again and again
    let data = text(300_000, 3);
    let input = [
        compress(
            &data,
            Params {
                checksum: true,
                ..PLAIN
            },
        ),
        skippable(2, b"in between"),
        SMALL.to_vec(),
        compress(
            &data[..5000],
            Params {
                content_size: false,
                ..PLAIN
            },
        ),
    ]
    .concat();
    let want = [&data[..], HELLO, &data[..5000]].concat();
    let steps = (1..=64).chain([100, 1000, 4097, input.len() - 1, input.len()]);
    for step in steps {
        let mut s = Stream::new();
        let mut got = Vec::new();
        for chunk in input.chunks(step) {
            assert!(
                s.feed(chunk, &mut |b| got.extend_from_slice(b)),
                "step {step}"
            );
        }
        assert!(!s.mid_frame, "step {step}");
        assert!(got == want, "step {step}");
    }
}

#[test]
fn each_stops_after_any_piece() {
    let (input, want) = mixed_frames();
    let mut pieces = 0;
    assert_eq!(
        decompress_each(&input, &mut |_| {
            pieces += 1;
            true
        }),
        Ended::Done
    );
    assert!(pieces > 3);
    for stop in 1..=pieces {
        let (mut calls, mut got) = (0, Vec::new());
        let ended = decompress_each(&input, &mut |piece| {
            calls += 1;
            got.extend_from_slice(piece);
            calls < stop
        });
        // stopping on the last piece is still a stop: the caller asked for it
        assert_eq!(ended, Ended::Stopped, "stop at {stop}");
        assert_eq!(calls, stop, "nothing after the stop");
        assert!(want.starts_with(&got), "stop at {stop}");
    }
}

#[test]
fn the_reader_resumes_wherever_it_is_left() {
    let (input, want) = mixed_frames();
    let mut rng = Rng(5);
    let mut r = reader(Dribble(&input, 777, false)).unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0u8; 300_000];
    loop {
        // empty reads, tiny ones, and ones larger than the output buffer, in any order
        let len = match rng.below(4) {
            0 => 0,
            1 => 1 + rng.below(16),
            2 => 1 + rng.below(5000),
            _ => buf.len(),
        };
        match r.read(&mut buf[..len]).unwrap() {
            0 if len == 0 => {},
            0 => break,
            n => got.extend_from_slice(&buf[..n]),
        }
    }
    assert!(got == want);
    // at the end it stays at the end
    assert_eq!(r.read(&mut buf).unwrap(), 0);
    assert_eq!(r.read(&mut buf).unwrap(), 0);
}

#[test]
fn damaged_input_is_an_error_never_a_panic() {
    let k = 128 << 10;
    let data = text(40_000, 11);
    let bases = [
        compress(&data, PLAIN),
        compress(
            &data,
            Params {
                checksum: true,
                content_size: false,
                ..PLAIN
            },
        ),
        compress(
            &data,
            Params {
                level: 19,
                checksum: true,
                ..PLAIN
            },
        ),
        [
            compress(&data[..1000], PLAIN),
            skippable(0, b"x"),
            raw_frame(&bytes(k + 5)),
        ]
        .concat(),
    ];
    let mut rng = Rng(0x5eed);
    for i in 0..3000 {
        let base = &bases[i % bases.len()];
        let mut bad = base.clone();
        match rng.below(5) {
            // a bit flipped, anywhere
            0 => {
                let at = rng.below(bad.len());
                bad[at] ^= 1 << rng.below(8);
            },
            // a few bytes replaced
            1 => {
                for _ in 0..1 + rng.below(4) {
                    let at = rng.below(bad.len());
                    bad[at] = rng.next() as u8;
                }
            },
            // cut off
            2 => bad.truncate(rng.below(bad.len())),
            // a byte gone, or one more
            3 => {
                bad.remove(rng.below(bad.len()));
            },
            _ => bad.insert(rng.below(bad.len()), rng.next() as u8),
        }
        // the ways in agree; whatever they say, nothing panics
        let got = decode_all_ways(&bad, 1 + rng.below(9000));
        // a frame cut off is never taken for a whole one
        if !bad.is_empty()
            && bad.len() < base.len()
            && base.starts_with(&bad)
            && i % bases.len() < 3
        {
            assert_eq!(got, None, "iteration {i}: cut at {}", bad.len());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The native decoder's allocator

use super::sys::{Block, Blocks, MAP_FROM};

/// The kind and length of the block at `addr`.
fn block_at(blocks: &Blocks, addr: usize) -> (bool, usize) {
    match &blocks.map()[&addr] {
        Block::Heap(v) => (false, v.len() * 16),
        Block::Map(m) => (true, m.len()),
    }
}

#[test]
fn blocks_are_aligned_and_writable_on_both_sides_of_the_threshold() {
    let blocks = Blocks::default();
    let sizes = [
        0,
        0,
        1,
        15,
        16,
        17,
        100,
        MAP_FROM - 1,
        MAP_FROM,
        MAP_FROM + 1,
        (8 << 20) + 1,
    ];
    let mut addrs = Vec::new();
    for size in sizes {
        let p = blocks.alloc(size) as usize;
        assert!(p != 0 && p.is_multiple_of(16), "size {size}");
        let (mapped, len) = block_at(&blocks, p);
        assert_eq!(mapped, size >= MAP_FROM, "size {size}");
        assert!(len >= size, "size {size}");
        if mapped {
            assert!(
                p.is_multiple_of(4096),
                "size {size}: a mapping starts on a page"
            );
        }
        addrs.push(p);
    }
    // every block its own (size 0 too), written all over without touching the others
    assert_eq!(blocks.map().len(), sizes.len());
    for (i, &p) in addrs.iter().enumerate() {
        match blocks.map().get_mut(&p).unwrap() {
            Block::Heap(v) => v.fill(u128::from_ne_bytes([i as u8; 16])),
            Block::Map(m) => m.fill(i as u8),
        }
    }
    for (i, &p) in addrs.iter().enumerate() {
        let ok = match &blocks.map()[&p] {
            Block::Heap(v) => v.iter().all(|&x| x == u128::from_ne_bytes([i as u8; 16])),
            Block::Map(m) => m.iter().all(|&b| b == i as u8),
        };
        assert!(ok, "block {i}");
    }
    for p in addrs {
        blocks.free(p as *mut _);
    }
    blocks.free(std::ptr::null_mut()); // (fine)
    assert!(blocks.map().is_empty());
}

/// An 8 MiB window, no content size: the decoder allocates its whole window.
const WINDOW_8M: u8 = 13 << 3; // exponent 13: 2^(10+13)

#[test]
fn the_decoder_window_is_a_mapping_and_goes_when_the_decoder_does() {
    let data = text(9 << 20, 4);
    let frame = raw_frame_in(&data, WINDOW_8M);
    let mut s = Stream::new();
    let blocks = s.ctx.as_ref().unwrap().blocks();
    let made = blocks.map().len();
    assert!(made > 0, "the context itself is a block of ours");
    let mut seen = 0;
    let mut largest = (false, 0);
    let ok = s.feed(&frame, &mut |piece| {
        seen += piece.len();
        // (a look at the blocks while the decoder holds them)
        let map = blocks.map();
        largest = map
            .keys()
            .map(|&a| match &map[&a] {
                Block::Heap(v) => (false, v.len() * 16),
                Block::Map(m) => (true, m.len()),
            })
            .max_by_key(|b| b.1)
            .unwrap();
    });
    assert!(ok && seen == data.len());
    assert!(largest.0 && largest.1 >= 8 << 20, "{largest:?}");
    assert!(blocks.map().values().all(|b| match b {
        Block::Heap(v) => v.len() * 16 < MAP_FROM,
        Block::Map(m) => m.len() >= MAP_FROM,
    }));
    drop(s);
    assert!(
        blocks.map().is_empty(),
        "every block freed with the decoder"
    );
}

#[test]
fn decoders_made_and_dropped_over_and_over_leave_nothing() {
    let (input, want) = mixed_frames();
    let big = raw_frame_in(&bytes(300_000), WINDOW_8M);
    for i in 0..200 {
        let mut s = Stream::new();
        let blocks = s.ctx.as_ref().unwrap().blocks();
        let frame = if i % 2 == 0 { &input[..] } else { &big[..] };
        let mut n = 0;
        assert!(s.feed(frame, &mut |b| n += b.len()));
        drop(s);
        assert!(blocks.map().is_empty(), "round {i}");
        // abandoned in the middle of a frame
        let mut s = Stream::new();
        let blocks = s.ctx.as_ref().unwrap().blocks();
        assert!(s.feed(&big[..big.len() / 2], &mut |_| {}));
        assert!(s.mid_frame);
        drop(s);
        assert!(blocks.map().is_empty(), "round {i}, abandoned");
    }
    let mut got = Vec::new();
    reader(&input[..]).unwrap().read_to_end(&mut got).unwrap();
    assert!(got == want);
}

/// Decoding frames with an 8 MiB window again and again leaves the process no larger: each window
/// goes back to the system when its decoder is dropped. Run in a process of its own (this test
/// binary, this test alone), so that what the other tests allocate meanwhile is not counted.
#[cfg(target_os = "linux")]
#[test]
fn freed_windows_go_back_to_the_system() {
    const CHILD: &str = "ARMLESS_ZSTD_RSS_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "zstd::tests::freed_windows_go_back_to_the_system",
                "--exact",
            ])
            .args(["--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{log}");
        assert!(log.contains("1 passed"), "the child ran no test: {log}");
        log.lines()
            .filter(|l| l.starts_with("RSS"))
            .for_each(|l| eprintln!("{l}"));
        return;
    }
    /// Resident memory of this process, in KiB
    fn rss() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }
    let data = text(8 << 20, 6);
    let frame = raw_frame_in(&data, WINDOW_8M);
    drop(data);
    // the decoder's small blocks, and the heap's own bookkeeping, before the baseline
    assert!(decompress(&SMALL).is_some());
    let before = rss();
    for _ in 0..16 {
        let mut n = 0;
        assert_eq!(
            decompress_each(&frame, &mut |b| {
                n += b.len();
                true
            }),
            Ended::Done
        );
        assert_eq!(n, 8 << 20);
    }
    let after = rss();
    eprintln!("RSS before {before} KiB, after 16 decodes {after} KiB");
    // a window kept resident would be 8 MiB more; noise is far less than 3 MiB
    assert!(
        after < before + 3 * 1024,
        "RSS grew from {before} to {after} KiB"
    );
}
