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

/// Assembles `guest/smp_atomics.S` (clang + lld) once, or None if the tools are missing.
fn smp_atomics_image() -> Option<Vec<u8>> {
    static IMAGE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    IMAGE.get_or_init(build_smp_atomics).clone()
}

fn build_smp_atomics() -> Option<Vec<u8>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::temp_dir().join(format!("armless-smp-atomics-{}", std::process::id()));
    std::fs::create_dir_all(&out).ok()?;
    let run = |cmd: &mut Command| match cmd.status() {
        Ok(s) if s.success() => Some(()),
        Ok(s) => panic!("{cmd:?}: {s}"),
        Err(_) => None, // not installed
    };
    run(Command::new("clang")
        .args(["--target=aarch64-none-elf", "-march=armv8.1-a", "-c"])
        .arg(root.join("guest/smp_atomics.S"))
        .arg("-o")
        .arg(out.join("smp.o")))?;
    run(Command::new("ld.lld")
        .arg("-T")
        .arg(root.join("guest/link.ld"))
        .arg("-o")
        .arg(out.join("smp.elf"))
        .arg(out.join("smp.o")))?;
    run(Command::new("llvm-objcopy")
        .args(["-O", "binary"])
        .arg(out.join("smp.elf"))
        .arg(out.join("smp.bin")))?;
    std::fs::read(out.join("smp.bin")).ok()
}

/// Four CPUs adding to shared counters with the LSE atomics and with exclusives: none lost.
fn smp_atomics(native_jit: bool) {
    let Some(image) = smp_atomics_image() else {
        eprintln!("skipping: could not assemble the guest (needs clang, ld.lld, llvm-objcopy)");
        return;
    };
    let (mut vm, clock) = runner::new_vm(Config {
        cpus: 4,
        ..Default::default()
    })
    .unwrap();
    #[cfg(feature = "native-jit")]
    if native_jit {
        for cpu in vm.cpus.iter_mut() {
            cpu.enable_native_jit();
        }
    }
    #[cfg(not(feature = "native-jit"))]
    let _ = native_jit;
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
    assert_eq!(
        exit,
        Some(Stopped::PowerOff),
        "guest did not finish:\n{text}"
    );
    assert!(text.contains("smp atomics ok"), "{text}");
}

#[test]
fn smp_atomics_interpreted() {
    smp_atomics(false);
}

#[test]
fn smp_atomics_native_jit() {
    smp_atomics(true);
}
