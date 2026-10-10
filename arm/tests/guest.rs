//! Builds the bare-metal guest (if clang + lld are available) and runs it.

use std::path::PathBuf;
use std::process::Command;

use armless::board::Host;
use armless::cpu::{Cpu, Exit, StopReason};
use armless::devices::NativeBus;

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
    let mut cpu = Cpu::new(512 << 20, NativeBus::new());
    armless::boot_raw(&mut cpu, &image, armless::board::KERNEL_OFFSET).unwrap();
    let mut output = Vec::new();
    let exit = loop {
        let exit = cpu.run(100_000);
        output.extend(cpu.host.take_output());
        match exit {
            Exit::Budget if cpu.insn_count < 50_000_000 => {},
            Exit::Idle { wake_ns: Some(w) } => {
                let now = cpu.host.now_ns();
                cpu.host.skew_ns += w.saturating_sub(now);
            },
            other => break other,
        }
    };
    let text = String::from_utf8_lossy(&output);
    println!("{text}");
    assert_eq!(
        exit,
        Exit::Stop(StopReason::PowerOff),
        "guest did not finish cleanly:\n{text}"
    );
    assert!(
        text.contains(", failed 0"),
        "guest reported failures:\n{text}"
    );
}
