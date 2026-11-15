//! armless-exec, the command: what its options do with the files it is given.

mod prog;

use std::fs::File;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime};

use prog::*;

/// armless-exec with `args`
fn exec(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_armless-exec"))
        .args(args)
        .output()
        .unwrap()
}

/// process.bin in `dir` as modified at `t`
fn modified(dir: &Path, t: SystemTime) {
    let f = File::options()
        .write(true)
        .open(dir.join("process.bin"))
        .unwrap();
    f.set_modified(t).unwrap();
}

#[test]
fn a_restore_goes_on_by_the_age_of_process_bin() {
    // A program that sleeps an hour and prints "w", saved while it sleeps
    let mut p = Prog::default();
    p.put(0x300, &[3600u64, 0].map(u64::to_le_bytes).concat());
    p.sys(101, &[D(0x300), I(0)]).expect(0);
    p.out(0x380, 1);
    p.put(0x380, b"w");
    let dir = std::env::temp_dir().join(format!("armless-exec-restore-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let elf = dir.join("prog");
    std::fs::write(&elf, p.elf()).unwrap();
    let (d, e) = (dir.to_str().unwrap(), elf.to_str().unwrap());
    let out = exec(&["-m", "128", "--timeout", "1", "--save", d, e]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"");
    // Saved for less than the sleep (and a file from the future: no time): it still sleeps
    let now = SystemTime::now();
    for t in [
        now - Duration::from_secs(1800),
        now + Duration::from_secs(7200),
    ] {
        modified(&dir, t);
        let out = exec(&["-m", "128", "--timeout", "1", "--restore", d]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, b"");
    }
    // Saved for longer: the sleep ran out, and it goes on at once
    modified(&dir, now - Duration::from_secs(3601));
    let out = exec(&["-m", "128", "--timeout", "10", "--restore", d]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"w");
    let _ = std::fs::remove_dir_all(&dir);
}
