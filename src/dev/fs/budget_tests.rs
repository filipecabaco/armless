use super::*;

/// /big: 10 bytes in chunks "a" (4), "b" (4), "c" (2); /old: one blob "o" (3 bytes); /tmp.
fn image() -> Fs {
    let json = br#"{"version":3,"fsroot":[
        ["big",10,0,33188,0,0,[["a",4],["b",4],["c",2]]],
        ["old",3,0,33188,0,0,"o"],
        ["tmp",0,0,16877,0,0,[]]]}"#;
    let mut fs = Fs::from_json(json).unwrap();
    fs.spill = crate::spill::Spill::off();
    for (n, d) in [("a", &b"0123"[..]), ("b", b"4567"), ("c", b"89"), ("o", b"xyz")] {
        fs.provide_blob(n, d.to_vec()).unwrap();
    }
    fs
}

fn used(fs: &Fs) -> i64 {
    fs.write_budget().unwrap().used()
}

/// What making a file costs: its inode and its name
fn file_cost(name: &str) -> i64 {
    (INODE_COST + ENTRY_COST + name.len() as u64) as i64
}

#[test]
fn no_budget_by_default_and_statfs_has_room_to_spare() {
    let mut fs = image();
    assert_eq!(fs.write_budget(), None);
    let tmp = fs.lookup_path("/tmp").unwrap();
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.write(f, 0, &vec![7u8; 1 << 20], 0).unwrap();
    let (blocks, free, _, ffree) = fs.space();
    assert!(blocks >= 2 << 20 && free >= 1 << 20, "{blocks} {free}");
    assert_eq!(ffree, 1 << 20);
}

#[test]
fn writes_past_the_budget_fail_with_enospc() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    fs.set_write_limit(Some(64 << 10));
    assert_eq!(used(&fs), 0, "counted from when it was set");
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    assert_eq!(used(&fs), file_cost("f"));
    let left = fs.write_budget().unwrap().left() as usize;
    // Up to the budget: fine; a byte more: no room
    assert_eq!(fs.write(f, 0, &vec![1u8; left], 0), Ok(left));
    assert_eq!(fs.write_budget().unwrap().left(), 0);
    assert!(!fs.write_budget().unwrap().hit);
    assert_eq!(fs.write(f, left as u64, b"x", 0), Err(Fail::Errno(ENOSPC)));
    assert!(fs.write_budget().unwrap().hit);
    // Nothing changed, and overwriting what is there costs nothing
    assert_eq!(fs.inode(f).unwrap().size, left as u64);
    assert_eq!(fs.write(f, 0, b"over", 0), Ok(4));
    // Nor can a file, a directory or a symlink be made
    assert_eq!(fs.create(tmp, b"g", S_IFREG | 0o644, 0, 0, 0), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.create(tmp, b"d", S_IFDIR | 0o755, 0, 0, 0), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.symlink(tmp, b"l", b"/x", 0, 0, 0), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.link(tmp, b"h", f, 0), Err(Fail::Errno(ENOSPC)));
    // Nor a file grown by truncation
    assert_eq!(fs.truncate(f, left as u64 + 4096), Err(Fail::Errno(ENOSPC)));
    // statfs: nothing left
    assert_eq!(fs.space().1, 0);
}

#[test]
fn removing_and_shrinking_give_the_room_back() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    fs.set_write_limit(Some(1 << 20));
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.write(f, 0, &vec![1u8; 100_000], 0).unwrap();
    assert_eq!(used(&fs), file_cost("f") + 100_000);
    fs.truncate(f, 1000).unwrap();
    assert_eq!(used(&fs), file_cost("f") + 1000);
    fs.unlink(tmp, b"f", false, 0).unwrap();
    assert_eq!(used(&fs), 0);
    // A file still open when it is removed holds its bytes until it is closed
    let g = fs.create(tmp, b"g", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.add_ref(g);
    fs.write(g, 0, &vec![1u8; 5000], 0).unwrap();
    fs.unlink(tmp, b"g", false, 0).unwrap();
    assert_eq!(used(&fs), INODE_COST as i64 + 5000);
    fs.drop_ref(g);
    assert_eq!(used(&fs), 0);
    // Removing what was there before gives room beyond the budget's start
    fs.unlink(ROOT, b"old", false, 0).unwrap();
    assert!(used(&fs) < 0);
}

#[test]
fn a_change_to_a_file_from_the_image_costs_what_it_copies() {
    let mut fs = image();
    fs.set_write_limit(Some(1 << 20));
    // A whole file is copied to be written
    let old = fs.lookup_path("/old").unwrap();
    fs.write(old, 0, b"X", 0).unwrap();
    assert_eq!(used(&fs), 3);
    // A chunked one, only the chunks written
    let big = fs.lookup_path("/big").unwrap();
    fs.write(big, 5, b"Y", 0).unwrap();
    assert_eq!(used(&fs), 3 + 4);
    // Grown: the chunks it touches and what it grows by
    fs.write(big, 9, b"ZZ", 0).unwrap();
    assert_eq!(used(&fs), 3 + 4 + 2 + 1);
    // A budget too small for the copy refuses it
    let mut fs = image();
    fs.set_write_limit(Some(2));
    let old = fs.lookup_path("/old").unwrap();
    assert_eq!(fs.write(old, 0, b"X", 0), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.read(old, 0, 3).unwrap(), b"xyz");
}

#[test]
fn renames_and_large_files_stay_counted() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    fs.set_write_limit(Some(16 << 20));
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    // Past WRITE_CHUNK, kept in chunks: counted all the same
    let mb = vec![3u8; WRITE_CHUNK];
    for k in 0..3 {
        fs.write(f, (k * WRITE_CHUNK) as u64, &mb, 0).unwrap();
    }
    assert_eq!(used(&fs), file_cost("f") + 3 * WRITE_CHUNK as i64);
    let g = fs.create(tmp, b"g", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.write(g, 0, b"g", 0).unwrap();
    // Over another file: the one replaced is freed
    fs.rename(tmp, b"g", tmp, b"f", 0).unwrap();
    assert_eq!(used(&fs), file_cost("f") + 1);
    // Its count, recounted from scratch, is the same
    let b = fs.write_budget().unwrap();
    let held: u64 = fs.inodes.iter().flatten().map(Fs::held_by).sum();
    assert_eq!(held, b.held);
}

#[test]
fn statfs_says_what_is_left() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    fs.set_write_limit(Some(1 << 20));
    let (_, free, _, ffree) = fs.space();
    assert_eq!(free, 256);
    assert_eq!(ffree, (1 << 20) / (INODE_COST + ENTRY_COST));
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.write(f, 0, &vec![0u8; 512 << 10], 0).unwrap();
    let (blocks, free, _, _) = fs.space();
    assert!(free < 128, "{free}");
    assert!(blocks >= free);
    // Lifted: room to spare again
    fs.set_write_limit(None);
    assert!(fs.space().1 >= 1 << 20);
    assert_eq!(fs.write(f, 0, &vec![0u8; 4 << 20], 0), Ok(4 << 20));
}

#[test]
fn a_write_past_the_budget_is_refused_whole() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    fs.set_write_limit(Some(10_000));
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    let left = fs.write_budget().unwrap().left() as usize;
    // Nothing of it is kept: a guest's kernel retries what a short count leaves
    assert_eq!(fs.write(f, 0, &vec![1u8; left + 500], 0), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.inode(f).unwrap().size, 0);
    // What fits still does
    assert_eq!(fs.write(f, 0, &vec![1u8; left], 0), Ok(left));
}

#[test]
fn a_file_with_an_allowance_of_its_own_is_outside_the_budget() {
    let mut fs = image();
    let tmp = fs.lookup_path("/tmp").unwrap();
    let log = fs.create(tmp, b"log", S_IFREG | 0o644, 0, 0, 0).unwrap();
    fs.set_write_limit(Some(8192));
    fs.set_allowance(log, Some(20_000));
    assert_eq!(used(&fs), 0, "into or out of the budget, nothing used changes");
    // The budget used up by another file
    let f = fs.create(tmp, b"f", S_IFREG | 0o644, 0, 0, 0).unwrap();
    let left = fs.write_budget().unwrap().left() as usize;
    fs.write(f, 0, &vec![1u8; left], 0).unwrap();
    assert_eq!(fs.write(f, left as u64, b"x", 0), Err(Fail::Errno(ENOSPC)));
    // The log still takes writes, up to its own limit: one that starts within it whole (never a
    // short count), then none, answered as written (never an error)
    assert_eq!(fs.write(log, 0, &vec![2u8; 15_000], 0), Ok(15_000));
    assert_eq!(fs.write(log, 15_000, &vec![2u8; 10_000], 0), Ok(10_000));
    assert_eq!(fs.write(log, 25_000, b"x", 0), Ok(1));
    assert_eq!(fs.write(log, 20_000, &[9u8; 100], 0), Ok(100));
    assert_eq!(fs.inode(log).unwrap().size, 25_000, "dropped");
    assert_eq!(fs.truncate(log, 30_000), Err(Fail::Errno(ENOSPC)));
    assert_eq!(fs.write_budget().unwrap().left(), 0, "none of it counted");
    // Truncated and written again (a shell's `>`): its allowance holds
    fs.truncate(log, 0).unwrap();
    assert_eq!(fs.write(log, 0, &vec![3u8; 25_000], 0), Ok(25_000));
    // Removed: forgotten, and nothing of it is taken from the budget
    let before = used(&fs);
    fs.unlink(tmp, b"log", false, 0).unwrap();
    assert_eq!(used(&fs), before - (ENTRY_COST + 3) as i64);
    assert!(fs.allowances.is_empty());
}
