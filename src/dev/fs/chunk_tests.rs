use super::*;

/// /big: 10 bytes in chunks "a" (4), "b" (4), "c" (2); /old: one blob "o".
fn image() -> Fs {
    let json = br#"{"version":3,"fsroot":[
        ["big",10,0,33188,0,0,[["a",4],["b",4],["c",2]]],
        ["old",3,0,33188,0,0,"o"]]}"#;
    let mut fs = Fs::from_json(json).unwrap();
    // These tests look into the heap cache; the spill has its own (spill_tests)
    fs.spill = crate::spill::Spill::off();
    fs
}

fn provide_all(fs: &mut Fs) {
    for (n, d) in [
        ("a", &b"0123"[..]),
        ("b", b"4567"),
        ("c", b"89"),
        ("o", b"xyz"),
    ] {
        fs.provide_blob(n, d.to_vec()).unwrap();
    }
}

fn need(names: &[&str]) -> Fail {
    Fail::Need(names.iter().map(|s| s.to_string()).collect())
}

#[test]
fn reads_fetch_only_the_chunks_they_cover() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    assert_eq!(fs.inode(big).unwrap().size, 10);
    assert_eq!(fs.read(big, 0, 2), Err(need(&["a"])));
    assert_eq!(fs.read(big, 5, 2), Err(need(&["b"])));
    // Across a boundary: everything missing is asked for at once
    assert_eq!(fs.read(big, 2, 7), Err(need(&["a", "b", "c"])));
    fs.provide_blob("b", b"4567".to_vec()).unwrap();
    assert_eq!(fs.read(big, 2, 7), Err(need(&["a", "c"])));
    fs.provide_blob("a", b"0123".to_vec()).unwrap();
    assert_eq!(fs.read(big, 2, 4).unwrap(), b"2345");
    assert_eq!(fs.read(big, 2, 7), Err(need(&["c"])));
    fs.provide_blob("c", b"89".to_vec()).unwrap();
    assert_eq!(fs.read(big, 2, 100).unwrap(), b"23456789");
    assert_eq!(fs.read(big, 10, 5).unwrap(), b"");
    assert_eq!(fs.read_all(big).unwrap(), b"0123456789");
}

#[test]
fn old_single_blob_files_still_work() {
    let mut fs = image();
    let old = fs.lookup_path("/old").unwrap();
    assert_eq!(fs.blob_of(old), Some("o"));
    assert_eq!(fs.read(old, 0, 10), Err(need(&["o"])));
    provide_all(&mut fs);
    assert_eq!(fs.read(old, 1, 10).unwrap(), b"yz");
    fs.write(old, 3, b"!", 1).unwrap();
    assert_eq!(fs.read_all(old).unwrap(), b"xyz!");
}

#[test]
fn a_failed_or_short_chunk_is_an_io_error() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    fs.blob_failed("a");
    assert_eq!(fs.read(big, 0, 1), Err(Fail::Errno(EIO)));
    fs.provide_blob("b", b"45".to_vec()).unwrap();
    assert_eq!(fs.read(big, 4, 1), Err(Fail::Errno(EIO)));
}

#[test]
fn a_forgotten_failure_is_asked_for_again() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    fs.blob_failed("a");
    assert_eq!(fs.read(big, 0, 1), Err(Fail::Errno(EIO)));
    fs.forget_failure("a");
    assert_eq!(fs.read(big, 0, 1), Err(need(&["a"])));
}

#[test]
fn writes_copy_only_the_chunks_they_touch() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    // Overwriting part of "b" needs only "b"
    assert_eq!(fs.write(big, 5, b"XY", 1), Err(need(&["b"])));
    fs.provide_blob("b", b"4567".to_vec()).unwrap();
    assert_eq!(fs.write(big, 5, b"XY", 1).unwrap(), 2);
    let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
        panic!()
    };
    assert!(matches!(&c[0].data, Piece::Blob(n) if n == "a"));
    assert!(matches!(&c[1].data, Piece::Mem(d) if d == b"4XY7"));
    assert!(matches!(&c[2].data, Piece::Blob(n) if n == "c"));
    assert_eq!(fs.inode(big).unwrap().mtime, (1, 0));
    // The shared blob is untouched
    provide_all(&mut fs);
    assert_eq!(fs.read_all(big).unwrap(), b"01234XY789");
    assert_eq!(&**fs.blobs.get("b").unwrap(), b"4567");
    // Across a boundary, and past the end with a hole
    fs.write(big, 3, b"ab", 2).unwrap();
    fs.write(big, 12, b"Z", 2).unwrap();
    assert_eq!(fs.inode(big).unwrap().size, 13);
    assert_eq!(fs.read_all(big).unwrap(), b"012abXY789\0\0Z");
    // Appending right at the end extends the last in-memory chunk
    fs.write(big, 13, b"!", 3).unwrap();
    let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
        panic!()
    };
    assert_eq!(c.len(), 4);
    assert_eq!(fs.read(big, 10, 10).unwrap(), b"\0\0Z!");
}

#[test]
fn a_save_that_leaves_contents_out_writes_the_rest_where_it_was() {
    let mut fs = image();
    provide_all(&mut fs);
    // Written in memory: a whole file, and a chunk of a chunked one (past the end, with a hole)
    let old = fs.lookup_path("/old").unwrap();
    fs.write(old, 9000, b"!", 1).unwrap();
    let big = fs.lookup_path("/big").unwrap();
    fs.write(big, 20_000, b"Z", 1).unwrap();
    fs.write(big, 5, b"XY", 1).unwrap(); // a small chunk, copied
    let mut whole = W::new();
    fs.save(&mut whole);
    let mut left = W::leaving_contents();
    fs.save(&mut left);
    let later = left.later();
    assert_eq!(later.len(), 2, "{later:?}");
    assert!(left.0.len() + 25_000 < whole.0.len());
    // The contents put back where they were noted make what a whole save writes
    let mut spliced = Vec::new();
    let mut at = 0;
    for l in &later {
        spliced.extend_from_slice(&left.0[at..l.at]);
        spliced.extend_from_slice(fs.saved_contents(l.ino, l.chunk).unwrap());
        at = l.at;
    }
    spliced.extend_from_slice(&left.0[at..]);
    assert_eq!(spliced, whole.0);
    assert!(fs.save_size_hint(&W::leaving_contents()) + 25_000 < fs.save_size_hint(&W::new()));
}

#[test]
fn stats_tell_the_blobs_that_were_fetched_from_the_files_the_guest_wrote() {
    let mut fs = image();
    let before = fs.stats();
    assert_eq!((before.cached_blobs, before.written_buffers, before.written_bytes), (0, 0, 0));
    assert_eq!(before.cache_limit, DEFAULT_CACHE);
    provide_all(&mut fs);
    let fetched = fs.stats();
    assert!(fetched.cached_blobs > 0 && fetched.cached_bytes > 0, "{fetched:?}");
    assert_eq!(fetched.written_buffers, 0);
    // A file written is held in memory for as long as it lives, whatever the cache holds
    let old = fs.lookup_path("/old").unwrap();
    fs.write(old, 9000, b"!", 1).unwrap();
    let after = fs.stats();
    assert_eq!(after.written_buffers, 1);
    assert!(after.written_bytes >= 9001 && after.written_capacity >= after.written_bytes, "{after:?}");
    assert_eq!(after.inodes, before.inodes);
    fs.set_cache_limit(0);
    assert_eq!(fs.stats().cache_limit, 0);
    assert!(fs.stats().cached_blobs <= 1);
}

#[test]
fn truncate_keeps_a_prefix_without_fetching() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    fs.truncate(big, 6).unwrap();
    let Content::Chunks(c) = &fs.inode(big).unwrap().content else {
        panic!()
    };
    assert_eq!(c.len(), 2);
    assert_eq!(c[1].len, 2);
    assert!(fs.blobs.is_empty());
    // Growing again shows zeros, not the rest of the blob
    fs.truncate(big, 9).unwrap();
    provide_all(&mut fs);
    assert_eq!(fs.read_all(big).unwrap(), b"012345\0\0\0");
    // A cut inside an in-memory chunk
    fs.write(big, 0, b"Q", 1).unwrap();
    fs.truncate(big, 3).unwrap();
    assert_eq!(fs.read_all(big).unwrap(), b"Q12");
    assert_eq!(fs.inode(big).unwrap().size, 3);
    fs.truncate(big, 0).unwrap();
    assert_eq!(fs.read_all(big).unwrap(), b"");
    assert!(matches!(fs.inode(big).unwrap().content, Content::Mem(_)));
}

/// A file's contents, and each in-memory chunk's (length, room).
fn written(fs: &mut Fs, ino: Ino) -> (Vec<u8>, Vec<(usize, usize)>) {
    let Content::Chunks(c) = &fs.inode(ino).unwrap().content else {
        panic!("not in chunks")
    };
    let shape = c
        .iter()
        .map(|c| match &c.data {
            Piece::Mem(d) => (d.len(), d.capacity()),
            Piece::Blob(_) => (c.len as usize, 0),
        })
        .collect();
    (fs.read_all(ino).unwrap(), shape)
}

#[test]
fn a_growing_file_grows_a_chunk_at_a_time() {
    let mut fs = image();
    let ino = fs.create(ROOT, b"log", S_IFREG | 0o644, 0, 0, 1).unwrap();
    // Appended 8 KiB at a time, as a database writes, to 2.5 MiB
    let page: Vec<u8> = (0..8192u32).map(|i| i as u8).collect();
    let n = (5 * WRITE_CHUNK / 2) / page.len();
    for k in 0..n {
        fs.write(ino, (k * page.len()) as u64, &page, 1).unwrap();
    }
    let (data, shape) = written(&mut fs, ino);
    assert_eq!(data, page.repeat(n));
    // Two whole chunks and half of one, each made once at its size: no buffer doubled or moved
    let half = WRITE_CHUNK / 2;
    assert_eq!(shape, [(WRITE_CHUNK, WRITE_CHUNK), (WRITE_CHUNK, WRITE_CHUNK), (half, WRITE_CHUNK)]);
    // Writes inside still land where they belong
    fs.write(ino, WRITE_CHUNK as u64 - 1, b"AB", 2).unwrap();
    assert_eq!(fs.read(ino, WRITE_CHUNK as u64 - 1, 2).unwrap(), b"AB");
}

#[test]
fn whole_files_go_to_chunks_once_they_are_large() {
    let mut fs = image();
    provide_all(&mut fs);
    // A small file stays whole
    let old = fs.lookup_path("/old").unwrap();
    fs.write(old, 3, b"!", 1).unwrap();
    assert!(matches!(fs.inode(old).unwrap().content, Content::Mem(_)));
    // Grown past a chunk by a truncate, then by a write: in chunks, the contents kept
    fs.truncate(old, WRITE_CHUNK as u64 + 10).unwrap();
    let (data, shape) = written(&mut fs, old);
    assert_eq!(&data[..4], b"xyz!");
    assert!(data[4..].iter().all(|&b| b == 0));
    assert_eq!(shape, [(WRITE_CHUNK, WRITE_CHUNK), (10, WRITE_CHUNK)]);
    let new = fs.create(ROOT, b"new", S_IFREG | 0o644, 0, 0, 1).unwrap();
    let big = vec![7u8; 3 * WRITE_CHUNK / 2];
    fs.write(new, 0, &big, 1).unwrap();
    assert_eq!(written(&mut fs, new).0, big);
}

#[test]
fn hard_links_and_renames_share_the_chunks() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    fs.link(ROOT, b"also", big, 1).unwrap();
    fs.rename(ROOT, b"big", ROOT, b"moved", 1).unwrap();
    provide_all(&mut fs);
    fs.write(big, 0, b"!", 1).unwrap();
    let also = fs.lookup_path("/also").unwrap();
    assert_eq!(fs.read_all(also).unwrap(), b"!123456789");
    assert_eq!(fs.lookup_path("/moved"), Some(big));
}

#[test]
fn snapshots_keep_only_written_chunks() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    provide_all(&mut fs);
    fs.write(big, 4, b"W", 1).unwrap();
    fs.truncate(big, 9).unwrap();
    let mut w = W::new();
    fs.save(&mut w);
    // "a" stays a name; only the written chunk "b" carries bytes
    assert!(w.0.windows(4).any(|s| s == b"W567"));
    assert!(!w.0.windows(4).any(|s| s == b"0123"));
    let mut back = Fs::load(&mut R(&w.0)).unwrap();
    let big2 = back.lookup_path("/big").unwrap();
    assert_eq!(back.read(big2, 0, 100), Err(need(&["a", "c"])));
    provide_all(&mut back);
    assert_eq!(back.read_all(big2).unwrap(), b"0123W5678");
}

#[test]
fn snapshots_from_before_chunks_still_load() {
    // A root directory with a file in blob "o" and a written one, encoded by hand
    // the way format 3 always has.
    let mut w = W::new();
    let inode = |w: &mut W, mode: u32, nlink: u32, size: u64| {
        w.u8(1);
        for v in [mode, 0, 0, nlink] {
            w.u32(v);
        }
        w.u64(size);
        w.u64(0);
        for _ in 0..3 {
            w.u64(5);
            w.u32(0);
        }
    };
    w.u64(4);
    w.u8(0);
    inode(&mut w, S_IFDIR | 0o755, 2, 4096);
    w.u8(0);
    w.u64(ROOT);
    w.u64(2);
    w.bytes(b"new");
    w.u64(3);
    w.bytes(b"old");
    w.u64(2);
    inode(&mut w, S_IFREG | 0o644, 1, 3);
    w.u8(2);
    w.str("o");
    inode(&mut w, S_IFREG | 0o644, 1, 2);
    w.u8(3);
    w.bytes(b"hi");
    let mut fs = Fs::load(&mut R(&w.0)).unwrap();
    let old = fs.lookup_path("/old").unwrap();
    assert_eq!(fs.read(old, 0, 9), Err(need(&["o"])));
    fs.provide_blob("o", b"xyz".to_vec()).unwrap();
    assert_eq!(fs.read(old, 0, 9).unwrap(), b"xyz");
    let new = fs.lookup_path("/new").unwrap();
    assert_eq!(fs.read_all(new).unwrap(), b"hi");
}

#[test]
fn chunk_lengths_must_add_up() {
    let json = br#"{"version":3,"fsroot":[["f",9,0,33188,0,0,[["a",4],["b",4]]]]}"#;
    assert!(Fs::from_json(json).err().unwrap().contains("8 bytes"));
    let json = br#"{"version":3,"fsroot":[["f",4,0,33188,0,0,[["a"]]]]}"#;
    assert!(Fs::from_json(json).is_err());
}

#[test]
fn a_waiting_read_keeps_its_chunks_cached() {
    let mut fs = image();
    let big = fs.lookup_path("/big").unwrap();
    fs.set_cache_limit(0);
    assert!(fs.read(big, 0, 10).is_err());
    fs.provide_blob("a", b"0123".to_vec()).unwrap();
    fs.provide_blob("b", b"4567".to_vec()).unwrap();
    fs.provide_blob("c", b"89".to_vec()).unwrap();
    // Over budget, but the read still has everything it waited for
    assert_eq!(fs.read_all(big).unwrap(), b"0123456789");
    // Done: no longer pinned, so the next blob pushes them out
    fs.provide_blob("o", b"xyz".to_vec()).unwrap();
    assert_eq!(fs.blobs.len(), 1);
}
