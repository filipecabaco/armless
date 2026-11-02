//! Builds the bare-metal guest (clang, and lld and objcopy: installed, or rustc's own) and runs it.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use armless::dev::Event;
use armless::native::{HostOptions, NativeHost};
use armless::runner::{self, Runner, Stopped};
use armless::vm::Config;

/// Whether `cmd --version` runs.
fn runs(cmd: &OsString) -> bool {
    Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The first of `names` that runs: as installed, else from the toolchain rustc ships (`rust-lld`
/// as `gcc-ld/ld.lld`, `rust-objcopy`, and `llvm-objcopy` with the llvm-tools component).
fn tool(names: &[&str]) -> Option<OsString> {
    static BIN: OnceLock<Option<PathBuf>> = OnceLock::new();
    let bin = BIN.get_or_init(|| {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        let out = |arg| Command::new(&rustc).arg(arg).output().ok().filter(|o| o.status.success());
        let sysroot = String::from_utf8(out("--print=sysroot")?.stdout).ok()?;
        let version = String::from_utf8(out("-vV")?.stdout).ok()?;
        let host = version.lines().find_map(|l| l.strip_prefix("host: "))?;
        Some(PathBuf::from(sysroot.trim()).join("lib/rustlib").join(host).join("bin"))
    });
    let installed = names.iter().map(OsString::from);
    let shipped = bin.iter().flat_map(|b| names.iter().map(move |n| b.join(n).into_os_string()));
    installed.chain(shipped).find(runs)
}

/// (LD, OBJCOPY) for `guest/build.sh`, or None if they are nowhere.
fn linker_and_objcopy() -> Option<(OsString, OsString)> {
    Some((
        tool(&["ld.lld", "gcc-ld/ld.lld"])?,
        tool(&["llvm-objcopy", "rust-objcopy"])?,
    ))
}

/// Without the tools there is no guest to run. On CI that is a failure, not a pass: it would hide
/// the whole CPU suite.
fn skip(what: &str) {
    assert!(
        std::env::var_os("CI").is_none(),
        "{what}: the tools are missing (clang, and ld.lld and llvm-objcopy or rustc's own)"
    );
    eprintln!("skipping: {what} (needs clang, and ld.lld and llvm-objcopy or rustc's own)");
}

/// Builds `guest/` once for all the tests here: the CPU suite (tests.bin) and the benchmark
/// (bench.bin).
fn guest_image() -> Option<Vec<u8>> {
    built().and_then(|out| std::fs::read(out.join("tests.bin")).ok())
}

fn bench_image() -> Option<Vec<u8>> {
    built().and_then(|out| std::fs::read(out.join("bench.bin")).ok())
}

fn built() -> Option<PathBuf> {
    static OUT: OnceLock<Option<PathBuf>> = OnceLock::new();
    OUT.get_or_init(build_guest).clone()
}

fn build_guest() -> Option<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::temp_dir().join(format!("armless-guest-{}", std::process::id()));
    let (ld, objcopy) = linker_and_objcopy()?;
    let status = Command::new("sh")
        .arg(root.join("guest/build.sh"))
        .env("OUT", &out)
        .env("LD", ld)
        .env("OBJCOPY", objcopy)
        .status()
        .ok()?;
    if !status.success() {
        return None;
    }
    Some(out)
}

/// The bare-metal CPU suite, interpreted or with every block compiled on its first run.
fn bare_metal(native_jit: bool) {
    let Some(image) = guest_image() else {
        return skip("could not build the guest");
    };
    let (mut vm, clock) = runner::new_vm(Config::default()).unwrap();
    #[cfg(feature = "native-jit")]
    if native_jit {
        let cpu = &mut vm.cpus[0];
        if !cpu.enable_native_jit() {
            return eprintln!("skipping: no native JIT backend for this host");
        }
        cpu.jit.threshold = 1;
    }
    #[cfg(not(feature = "native-jit"))]
    if native_jit {
        return;
    }
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
    #[cfg(feature = "native-jit")]
    if native_jit {
        // Compiled code ran, rather than the interpreter alone while compiles were pending
        let (compiled, _) = runner.with_cpu(0, |c| c.native_jit_stats());
        assert!(compiled > 0, "nothing was compiled");
        let failed = runner.with_cpu(0, |c| c.native_jit_failures());
        assert_eq!(failed, 0, "blocks that did not compile");
    }
}

#[test]
fn bare_metal_suite() {
    bare_metal(false);
}

#[test]
fn bare_metal_suite_native_jit() {
    bare_metal(true);
}

/// Assembles `guest/smp_atomics.S` once, or None if the tools are missing.
fn smp_atomics_image() -> Option<Vec<u8>> {
    static IMAGE: OnceLock<Option<Vec<u8>>> = OnceLock::new();
    IMAGE.get_or_init(build_smp_atomics).clone()
}

fn build_smp_atomics() -> Option<Vec<u8>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::temp_dir().join(format!("armless-smp-atomics-{}", std::process::id()));
    std::fs::create_dir_all(&out).ok()?;
    let (ld, objcopy) = linker_and_objcopy()?;
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
    run(Command::new(ld)
        .arg("-T")
        .arg(root.join("guest/link.ld"))
        .arg("-o")
        .arg(out.join("smp.elf"))
        .arg(out.join("smp.o")))?;
    run(Command::new(objcopy)
        .args(["-O", "binary"])
        .arg(out.join("smp.elf"))
        .arg(out.join("smp.bin")))?;
    std::fs::read(out.join("smp.bin")).ok()
}

/// Four CPUs adding to shared counters with the LSE atomics and with exclusives: none lost.
fn smp_atomics(native_jit: bool) {
    let Some(image) = smp_atomics_image() else {
        return skip("could not assemble the SMP guest");
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

/// The benchmark guest (guest/bench.c, what bench/run.exs times) under the native JIT, checked by
/// what the JIT did rather than by time: almost everything runs compiled, almost nothing goes back
/// to the interpreter, and every compile works. A change that quietly sends code back to the
/// interpreter fails here, not later as a slowdown.
#[cfg(feature = "native-jit")]
#[test]
fn benchmark_guest_stays_compiled() {
    let Some(image) = bench_image() else {
        return skip("could not build the benchmark guest");
    };
    let (mut vm, clock) = runner::new_vm(Config::default()).unwrap();
    if !vm.cpus[0].enable_native_jit() {
        return eprintln!("skipping: no native JIT backend for this host");
    }
    armless::boot_raw(&mut vm.cpus[0], &image, armless::board::KERNEL_OFFSET).unwrap();
    let _host = NativeHost::spawn(vm.board.clone(), HostOptions::default(), Box::new(|_| {}));
    let mut runner = Runner::new(vm, clock, true);
    runner.start();
    assert_eq!(runner.wait(Some(Duration::from_secs(300))), Some(Stopped::PowerOff));
    let (insns, compiled, helped, failed) = runner.with_cpu(0, |c| {
        (c.insn_count, c.jit.block_insns, c.jit.helper_interps, c.native_jit_failures())
    });
    assert_eq!(failed, 0, "blocks that did not compile");
    assert!(compiled * 100 >= insns * 98, "{compiled} of {insns} instructions ran compiled");
    // The guest's one TTBR write per round of its context-switch phase is all that remains
    assert!(helped * 1000 <= compiled, "{helped} of {compiled} compiled instructions interpreted");
}

