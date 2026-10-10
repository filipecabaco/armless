//! Builds the bare-metal guest (if clang + lld are available) and runs it.

use std::path::PathBuf;
use std::process::Command;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use armless::dev::Event;
use armless::native::{HostOptions, NativeHost};
use armless::runner::{self, Runner, Stopped};
use armless::vm::Config;

fn build_guest() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::temp_dir().join(format!("armless-guest-{}", std::process::id()));
    let status = Command::new("sh")
        .arg(root.join("guest/build.sh"))
        .env("OUT", &out)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    Some(out.join("tests.bin"))
}

#[test]
fn bare_metal_suite() {
    let Some(bin) = build_guest() else {
        eprintln!("skipping: could not build the guest (needs clang, ld.lld, llvm-objcopy)");
        return;
    };
    let image = std::fs::read(bin).unwrap();
    let (mut vm, clock) = runner::new_vm(Config::default()).unwrap();
    armless::boot_raw(&mut vm.cpus[0], &image, armless::board::KERNEL_OFFSET).unwrap();
    let output = Arc::new(Mutex::new(Vec::new()));
    let o = output.clone();
    let _host = NativeHost::spawn(
        vm.board.clone(),
        HostOptions::default(),
        Box::new(move |e| {
            if let Event::Serial(b) = e {
                o.lock().unwrap().extend_from_slice(b);
            }
        }),
    );
    let mut runner = Runner::new(vm, clock, true);
    runner.start();
    let exit = runner.wait(Some(Duration::from_secs(300)));
    std::thread::sleep(Duration::from_millis(300));
    let text = String::from_utf8_lossy(&output.lock().unwrap()).into_owned();
    println!("{text}");
    assert_eq!(
        exit,
        Some(Stopped::PowerOff),
        "guest did not finish cleanly:\n{text}"
    );
    assert!(
        text.contains(", failed 0"),
        "guest reported failures:\n{text}"
    );
}
