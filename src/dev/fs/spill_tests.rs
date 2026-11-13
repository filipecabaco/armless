//! Blobs in the spill file: the file system's reads and writes give what the heap cache gave.

use std::path::{Path, PathBuf};

use super::*;

/// A directory for the spill files (the spill is forced to files there, on any file system).
fn disk() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("spill-tests");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `n` bytes that tell where they are.
fn bytes(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31) ^ (i >> 8) as u8 ^ seed)
        .collect()
}

/// `data` as one zstd frame of raw blocks (the least a decoder accepts), in blocks of 128 KiB.
fn zstd(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    let mut blocks = data.chunks(128 << 10).peekable();
    while let Some(b) = blocks.next() {
        let head = (b.len() as u32) << 3 | blocks.peek().is_none() as u32;
        out.extend_from_slice(&head.to_le_bytes()[..3]);
        out.extend_from_slice(b);
    }
    out
}

const A: usize = 200_000;
const B: usize = 150_000;
const C: usize = 1_000;
const SMALL: usize = 3_000;

/// /big in chunks "a", "b", "c" (sizes above), /small in one blob "s".
fn image(spill: Spill) -> Fs {
    let json = format!(
        r#"{{"version":3,"fsroot":[
            ["big",{},0,33188,0,0,[["a",{A}],["b",{B}],["c",{C}]]],
            ["small",{SMALL},0,33188,0,0,"s"]]}}"#,
        A + B + C
    );
    let mut fs = Fs::from_json(json.as_bytes()).unwrap();
    fs.spill = spill;
    fs
}

fn contents() -> (Vec<u8>, Vec<u8>) {
    let big = [bytes(A, 1), bytes(B, 2), bytes(C, 3)].concat();
    (big, bytes(SMALL, 4))
}

fn provide(fs: &mut Fs) {
    fs.provide_blob("a", zstd(&bytes(A, 1))).unwrap();
    fs.provide_blob("b", zstd(&bytes(B, 2))).unwrap();
    fs.provide_blob("c", bytes(C, 3)).unwrap();
    fs.provide_blob("s", zstd(&bytes(SMALL, 4))).unwrap();
}

#[test]
fn blobs_are_decompressed_into_the_file_and_read_from_it() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let st = fs.stats();
    // Nothing is held in memory for them: the four blobs are in the file
    assert_eq!(
        (st.cached_blobs, st.cached_bytes, st.spilled_blobs),
        (0, 0, 4),
        "{st:?}"
    );
    assert_eq!(st.spilled_bytes, (A + B + C + SMALL) as u64);
    assert!(fs.blobs.is_empty() && fs.has_blob("a") && fs.has_blob("s"));

    let (big_bytes, small_bytes) = contents();
    let big = fs.lookup_path("/big").unwrap();
    let small = fs.lookup_path("/small").unwrap();
    assert_eq!(fs.read_all(big).unwrap(), big_bytes);
    assert_eq!(fs.read_all(small).unwrap(), small_bytes);
    // Reads inside a chunk, across each boundary, over all three, and past the end
    for (off, n) in [
        (0, 10),
        (A - 5, 10),
        (A + B - 3, 7),
        (A - 100, B + 200),
        (5, A + B + C - 5),
        (A + B + C - 2, 50),
    ] {
        let end = (off + n).min(big_bytes.len());
        assert_eq!(
            fs.read(big, off as u64, n).unwrap(),
            &big_bytes[off..end],
            "{off}+{n}"
        );
    }
    assert_eq!(fs.read(small, 2_990, 100).unwrap(), &small_bytes[2_990..]);
    assert_eq!(fs.read(small, 10_000, 5).unwrap(), b"");
}

#[test]
fn a_read_handed_to_a_sink_or_a_slice_is_in_pieces_that_add_up() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let (big_bytes, _) = contents();
    let big = fs.lookup_path("/big").unwrap();
    // Over a boundary and past the 64 KiB a sink is given at once
    let (off, n) = (A as u64 - 70_000, 160_000usize);
    let mut dst = vec![0u8; n];
    fs.read_into(big, off, &mut dst).unwrap();
    assert_eq!(dst, &big_bytes[off as usize..off as usize + n]);
    let mut out = vec![0u8; n];
    let mut calls = 0;
    fs.read_with(big, off, n as u64, |at, d| {
        out[at..at + d.len()].copy_from_slice(d);
        calls += 1;
    })
    .unwrap();
    assert_eq!(out, dst);
    assert!(calls >= 3, "{calls}");
    assert_eq!(
        fs.read_into(big, big_bytes.len() as u64 - 1, &mut [0; 2]),
        Err(EINVAL.into())
    );
}

#[test]
fn a_guest_write_copies_only_the_pieces_it_touches() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let (mut big_bytes, mut small_bytes) = contents();
    let big = fs.lookup_path("/big").unwrap();
    let small = fs.lookup_path("/small").unwrap();
    // A write over the boundary of "a" and "b", then one past the end
    fs.write(big, A as u64 - 3, b"XXXXXX", 1).unwrap();
    big_bytes[A - 3..A + 3].copy_from_slice(b"XXXXXX");
    fs.write(big, big_bytes.len() as u64 + 2, b"!", 1).unwrap();
    big_bytes.extend_from_slice(b"\0\0!");
    assert_eq!(fs.read_all(big).unwrap(), big_bytes);
    // A whole small file made writable
    fs.write(small, 10, b"hello", 1).unwrap();
    small_bytes[10..15].copy_from_slice(b"hello");
    assert_eq!(fs.read_all(small).unwrap(), small_bytes);
    assert!(matches!(fs.inode(small).unwrap().content, Content::Mem(_)));
    // The blobs are untouched: another file made of the same ones reads them as they were
    let json = format!(r#"{{"version":3,"fsroot":[["again",{SMALL},0,33188,0,0,"s"]]}}"#);
    let (spill, spilled) = (
        std::mem::replace(&mut fs.spill, Spill::off()),
        fs.spilled.clone(),
    );
    let mut other = Fs::from_json(json.as_bytes()).unwrap();
    (other.spill, other.spilled) = (spill, spilled);
    let again = other.lookup_path("/again").unwrap();
    assert_eq!(other.read_all(again).unwrap(), contents().1);
}

#[test]
fn a_file_grown_past_a_chunk_is_copied_out_of_the_file() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let (_, small_bytes) = contents();
    let small = fs.lookup_path("/small").unwrap();
    // Past WRITE_CHUNK it is kept in chunks from now on, the first made from the blob
    fs.write(small, WRITE_CHUNK as u64 + 5, b"end", 1).unwrap();
    let all = fs.read_all(small).unwrap();
    assert_eq!(&all[..SMALL], &small_bytes[..]);
    assert_eq!(
        &all[SMALL..WRITE_CHUNK + 5],
        &vec![0; WRITE_CHUNK + 5 - SMALL][..]
    );
    assert_eq!(&all[WRITE_CHUNK + 5..], b"end");
}

#[test]
fn a_full_disk_leaves_the_blob_in_memory() {
    // 200 KB of room: "a" (200 KB, zstd) goes in the file; the blob after it fails half way
    let mut fs = image(Spill::in_dir_forced(&disk(), A as u64 + 100));
    fs.provide_blob("a", zstd(&bytes(A, 1))).unwrap();
    assert_eq!((fs.stats().spilled_blobs, fs.stats().cached_blobs), (1, 0));
    // "b" starts to be written (a 128 KiB block fits? no: all of it is over the limit)
    fs.provide_blob("b", zstd(&bytes(B, 2))).unwrap();
    // A blob that would fit is not tried once the file refused one: one warning, and memory
    fs.provide_blob("s", zstd(&bytes(50, 4))).unwrap();
    fs.provide_blob("c", bytes(C, 3)).unwrap();
    let st = fs.stats();
    assert_eq!((st.spilled_blobs, st.cached_blobs), (1, 3), "{st:?}");
    assert_eq!(st.cached_bytes, B + 50 + C);
    let (big_bytes, _) = contents();
    let big = fs.lookup_path("/big").unwrap();
    // The blob in the file and the ones in memory read as one file
    assert_eq!(fs.read_all(big).unwrap(), big_bytes);
}

#[test]
fn a_blob_that_fails_in_the_middle_of_the_file_is_not_published() {
    // The first block (128 KiB) fits, the second does not
    let mut fs = image(Spill::in_dir_forced(&disk(), 150_000));
    fs.provide_blob("a", zstd(&bytes(A, 1))).unwrap();
    assert_eq!((fs.stats().spilled_blobs, fs.stats().cached_blobs), (0, 1));
    assert!(fs.spilled.is_empty());
    let big = fs.lookup_path("/big").unwrap();
    assert_eq!(
        fs.read(big, A as u64, 10),
        Err(Fail::Need(vec!["b".into()]))
    );
    assert_eq!(fs.blobs["a"].as_slice(), &bytes(A, 1)[..]);
}

#[test]
fn an_unusable_directory_keeps_the_memory_cache() {
    let mut fs = image(Spill::in_dir_forced(
        Path::new("/nonexistent-armless-dir"),
        u64::MAX,
    ));
    provide(&mut fs);
    let st = fs.stats();
    assert_eq!((st.spilled_blobs, st.cached_blobs), (0, 4), "{st:?}");
    let (big_bytes, _) = contents();
    let big = fs.lookup_path("/big").unwrap();
    assert_eq!(fs.read_all(big).unwrap(), big_bytes);
}

#[cfg(target_os = "linux")]
#[test]
fn a_memory_file_system_keeps_the_memory_cache() {
    let dir = Path::new("/dev/shm");
    if !dir.exists() {
        return;
    }
    let mut fs = image(Spill::in_dir(dir, u64::MAX));
    provide(&mut fs);
    let st = fs.stats();
    assert_eq!((st.spilled_blobs, st.cached_blobs), (0, 4), "{st:?}");
    // and leaves no file behind
    assert!(std::fs::read_dir(dir).unwrap().flatten().all(|e| {
        !e.file_name()
            .to_string_lossy()
            .starts_with(".armless-spill")
    }));
}

#[test]
fn a_read_of_a_file_that_lost_its_contents_is_eio_not_zeros() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let (big, small) = (
        fs.lookup_path("/big").unwrap(),
        fs.lookup_path("/small").unwrap(),
    );
    fs.spill.lose_contents();
    assert_eq!(fs.read(big, 0, 10), Err(Fail::Errno(EIO)));
    assert_eq!(fs.read(big, A as u64 - 2, 10), Err(Fail::Errno(EIO)));
    assert_eq!(fs.read_all(small), Err(Fail::Errno(EIO)));
    assert_eq!(fs.read_with(small, 0, 10, |_, _| {}), Err(Fail::Errno(EIO)));
    assert_eq!(fs.write(small, 0, b"x", 1), Err(Fail::Errno(EIO)));
    // Nothing was made writable by the failed write
    assert!(matches!(fs.inode(small).unwrap().content, Content::Blob(_)));
}

#[test]
fn corrupt_data_is_not_a_blob() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    let mut bad = zstd(&bytes(1000, 1));
    bad.truncate(500);
    assert!(fs.provide_blob("s", bad).unwrap_err().contains("corrupt"));
    assert!(!fs.has_blob("s"));
    // The file takes the next blob where the corrupt one began
    fs.provide_blob("s", zstd(&bytes(SMALL, 4))).unwrap();
    let small = fs.lookup_path("/small").unwrap();
    assert_eq!(fs.read_all(small).unwrap(), contents().1);
}

#[test]
fn forgetting_blobs_empties_the_file_and_they_come_again() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let big = fs.lookup_path("/big").unwrap();
    // Forgetting what a boot copied elsewhere leaves a blob that is not in the heap
    fs.forget_file_before(big, (A + B + C) as u64);
    assert_eq!(fs.stats().spilled_blobs, 4);
    fs.forget_blobs();
    assert_eq!(fs.stats().spilled_blobs, 0);
    assert_eq!(fs.read(big, 0, 4), Err(Fail::Need(vec!["a".into()])));
    provide(&mut fs);
    assert_eq!(fs.read_all(big).unwrap(), contents().0);
}

#[test]
fn a_snapshot_carries_no_spilled_blobs() {
    let mut fs = image(Spill::in_dir_forced(&disk(), u64::MAX));
    provide(&mut fs);
    let small = fs.lookup_path("/small").unwrap();
    fs.write(small, 0, b"mine", 1).unwrap();
    let mut w = W::new();
    fs.save(&mut w);
    // The blobs are not in it: the snapshot is the size of the tree and the files the guest wrote
    assert!(w.0.len() < 4_000 + 500, "{}", w.0.len());
    let mut back = Fs::load(&mut R(&w.0)).unwrap();
    back.spill = Spill::in_dir_forced(&disk(), u64::MAX);
    let (big, small) = (
        back.lookup_path("/big").unwrap(),
        back.lookup_path("/small").unwrap(),
    );
    assert_eq!(back.stats().spilled_blobs, 0);
    assert_eq!(back.read(big, 0, 4), Err(Fail::Need(vec!["a".into()])));
    assert_eq!(&back.read_all(small).unwrap()[..4], b"mine");
    provide(&mut back);
    assert_eq!(back.read_all(big).unwrap(), contents().0);
}

#[test]
fn the_file_has_no_name_once_it_is_made() {
    let dir = disk().join("unlinked");
    std::fs::create_dir_all(&dir).unwrap();
    let mut fs = image(Spill::in_dir_forced(&dir, u64::MAX));
    provide(&mut fs);
    assert_eq!(fs.stats().spilled_blobs, 4);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn the_default_spill_serves_the_same_bytes_wherever_the_temporary_directory_is() {
    let mut fs = image(Spill::new());
    provide(&mut fs);
    // On a host whose temporary directory is tmpfs the blobs stay in the heap cache
    let st = fs.stats();
    assert_eq!(st.spilled_blobs + st.cached_blobs, 4, "{st:?}");
    let big = fs.lookup_path("/big").unwrap();
    assert_eq!(fs.read_all(big).unwrap(), contents().0);
}
