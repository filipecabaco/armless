use super::Fs;

#[test]
fn the_cache_stays_within_its_budget() {
    let mut fs = Fs::empty();
    fs.spill = crate::spill::Spill::off();
    fs.set_cache_limit(20);
    for name in ["a", "b", "c"] {
        fs.provide_blob(name, vec![1; 8]).unwrap();
    }
    // 24 bytes is over 20: the oldest went first
    assert!(!fs.blobs.contains_key("a"));
    assert!(fs.blobs.contains_key("b") && fs.blobs.contains_key("c"));
    assert_eq!(fs.cached, 16);

    // Lowering the budget drops what is over it now; the newest always stays
    fs.set_cache_limit(0);
    assert_eq!(fs.blobs.keys().collect::<Vec<_>>(), ["c"]);
    assert_eq!(fs.cached, 8);
}

#[test]
fn a_blob_the_host_decompressed_is_kept_as_it_is() {
    // The bytes of a zstd frame that are a file's content: `provide_blob` would decompress them
    let frame = vec![0x28, 0xb5, 0x2f, 0xfd, 1, 2, 3, 4];
    let mut fs = Fs::empty();
    fs.spill = crate::spill::Spill::off();
    fs.provide_raw("a", frame.clone()).unwrap();
    assert_eq!(fs.blobs.get("a").map(|b| b.as_slice()), Some(&frame[..]));
    assert_eq!(fs.cached, frame.len());
}
