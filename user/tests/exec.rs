//! Programs run in user mode, written here as aarch64 machine code in a minimal static ELF, so
//! the tests need no cross toolchain.

use std::time::{Duration, Instant};

use armless::runner::NativeCpu;
use armless::vm::{Config, Vm};
use armless_user::{Process, Status};

const BASE: u64 = 0x40_0000;
/// ELF header and one program header: the code starts right after them.
const CODE: u64 = 64 + 56;

fn movz(rd: u32, imm: u16) -> u32 {
    0xd280_0000 | (imm as u32) << 5 | rd
}
const SVC: u32 = 0xd400_0001;

/// adr xd, pc + off
fn adr(rd: u32, off: i64) -> u32 {
    let imm = off as u32 & 0x1f_ffff;
    0x1000_0000 | (imm & 3) << 29 | (imm >> 2) << 5 | rd
}

/// b pc + off
fn b(off: i64) -> u32 {
    0x1400_0000 | ((off / 4) as u32 & 0x3ff_ffff)
}

/// cbz xt, pc + off
fn cbz(rt: u32, off: i64) -> u32 {
    0xb400_0000 | ((off / 4) as u32 & 0x7_ffff) << 5 | rt
}

/// A static aarch64 executable of `code` (and `data` after it), loaded at BASE.
fn elf(code: &[u32], data: &[u8]) -> Vec<u8> {
    let mut body: Vec<u8> = code.iter().flat_map(|w| w.to_le_bytes()).collect();
    body.extend_from_slice(data);
    let size = CODE + body.len() as u64;
    let mut e = Vec::new();
    e.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    e.extend_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    e.extend_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e.extend_from_slice(&1u32.to_le_bytes());
    e.extend_from_slice(&(BASE + CODE).to_le_bytes()); // entry
    e.extend_from_slice(&64u64.to_le_bytes()); // phoff
    e.extend_from_slice(&0u64.to_le_bytes()); // shoff
    e.extend_from_slice(&0u32.to_le_bytes());
    for v in [64u16, 56, 1, 64, 0, 0] {
        e.extend_from_slice(&v.to_le_bytes()); // ehsize, phentsize, phnum, shentsize, shnum, shstrndx
    }
    e.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    e.extend_from_slice(&5u32.to_le_bytes()); // R X
    for v in [0, BASE, BASE, size, size, 0x1000] {
        e.extend_from_slice(&v.to_le_bytes()); // offset, vaddr, paddr, filesz, memsz, align
    }
    assert_eq!(e.len() as u64, CODE);
    e.extend_from_slice(&body);
    e
}

/// write(1, "hello\n", 6), then exit_group(3).
fn hello() -> Vec<u8> {
    let code = [
        movz(0, 1),
        adr(1, 7 * 4),
        movz(2, 6),
        movz(8, 64),
        SVC,
        movz(0, 3),
        movz(8, 94),
        SVC,
    ];
    elf(&code, b"hello\n")
}

/// Echoes stdin to stdout until its end, then exit_group(7).
fn echo() -> Vec<u8> {
    let code = [
        0xd101_03ff,  // 0x00 sub sp, sp, #64
        movz(0, 0),   // 0x04 read(0, sp, 64)
        0x9100_03e1,  // 0x08 mov x1, sp
        movz(2, 64),  // 0x0c
        movz(8, 63),  // 0x10
        SVC,          // 0x14
        cbz(0, 0x1c), // 0x18 end of input: to 0x34
        0xaa00_03e2,  // 0x1c mov x2, x0: write(1, sp, n)
        movz(0, 1),   // 0x20
        0x9100_03e1,  // 0x24 mov x1, sp
        movz(8, 64),  // 0x28
        SVC,          // 0x2c
        b(-0x2c),     // 0x30 back to 0x04
        movz(0, 7),   // 0x34 exit_group(7)
        movz(8, 94),
        SVC,
    ];
    elf(&code, &[])
}

fn machine() -> Vm<armless::platform::NativePlatform> {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            ..Default::default()
        },
    };
    armless::runner::new_vm(config).unwrap().0
}

/// Run until it exits, faults or waits with nothing to wake it; what it printed, and how.
fn run(p: &mut Process, cpu: &mut NativeCpu) -> (Vec<u8>, Status) {
    let mut out = Vec::new();
    let begun = Instant::now();
    loop {
        let status = p.run(cpu, Duration::from_millis(50));
        out.extend(p.take_output());
        match status {
            Status::Running => assert!(begun.elapsed() < Duration::from_secs(10), "still running"),
            s => return (out, s),
        }
    }
}

#[test]
fn a_program_runs_with_no_kernel() {
    let mut vm = machine();
    let mut p =
        Process::exec_bytes(&mut vm.cpus[0], None, &hello(), &["hello".into()], &[]).unwrap();
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(
        out, b"hello\r\n",
        "on a terminal, ONLCR: a newline starts a line"
    );
    assert!(matches!(status, Status::Exited(3)), "{status:?}");
    assert_eq!(p.exit_code(), Some(3));
    assert_eq!(p.stats.syscalls.get(&64), Some(&1));
}

#[test]
fn a_program_waits_for_input_and_answers_it() {
    let mut vm = machine();
    let mut p = Process::exec_bytes(&mut vm.cpus[0], None, &echo(), &["echo".into()], &[]).unwrap();
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(out.is_empty());
    assert!(
        matches!(status, Status::Idle(None)),
        "waits on stdin, with no deadline: {status:?}"
    );
    p.input(b"one\n");
    assert_eq!(
        run(&mut p, &mut vm.cpus[0]).0,
        b"one\r\none\r\n",
        "the line echoed as typed, then the program's copy"
    );
    p.close_input();
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Exited(7)), "{status:?}");
}

#[test]
fn a_saved_process_goes_on_in_another_machine() {
    // Saved while it waits on stdin: the machine's snapshot and the process's state
    let mut vm = machine();
    let mut p = Process::exec_bytes(&mut vm.cpus[0], None, &echo(), &["echo".into()], &[]).unwrap();
    p.input(b"before\n");
    assert_eq!(run(&mut p, &mut vm.cpus[0]).0, b"before\r\nbefore\r\n");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process).unwrap();
    p.input(b"after\n");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"after\r\nafter\r\n");
    assert!(matches!(status, Status::Idle(None)), "{status:?}");

    // A process state is refused on a machine of another size, and when it is not one
    let mut other = armless::runner::new_vm(Config {
        memory_size: 256 << 20,
        ..Config::default()
    })
    .unwrap()
    .0;
    assert!(Process::restore(&mut other.cpus[0], None, &process).is_err());
    assert!(Process::restore(&mut vm.cpus[0], None, b"nonsense").is_err());
}

#[test]
fn a_dynamic_or_foreign_executable_is_refused() {
    let mut vm = machine();
    let mut e = hello();
    e[18] = 62; // EM_X86_64
    assert!(Process::exec_bytes(&mut vm.cpus[0], None, &e, &["x".into()], &[]).is_err());
    assert!(
        Process::exec_bytes(&mut vm.cpus[0], None, b"#!/bin/sh\n", &["x".into()], &[]).is_err()
    );
}

/// open("/etc/greeting"), read it, write it to stdout, exit_group(0).
fn cat_greeting() -> Vec<u8> {
    let code = [
        0xd101_03ff,  // 0x00 sub sp, sp, #64
        0x9280_0c60,  // 0x04 movn x0, #99: AT_FDCWD
        adr(1, 0x48), // 0x08 the path, at 0x50
        movz(2, 0),   // 0x0c O_RDONLY
        movz(8, 56),  // 0x10 openat
        SVC,          // 0x14
        0xaa00_03f3,  // 0x18 mov x19, x0
        0xaa13_03e0,  // 0x1c mov x0, x19: read(fd, sp, 64)
        0x9100_03e1,  // 0x20 mov x1, sp
        movz(2, 64),  // 0x24
        movz(8, 63),  // 0x28
        SVC,          // 0x2c
        0xaa00_03e2,  // 0x30 mov x2, x0: write(1, sp, n)
        movz(0, 1),   // 0x34
        0x9100_03e1,  // 0x38 mov x1, sp
        movz(8, 64),  // 0x3c
        SVC,          // 0x40
        movz(0, 0),   // 0x44 exit_group(0)
        movz(8, 94),  // 0x48
        SVC,          // 0x4c
    ];
    elf(&code, b"/etc/greeting\0")
}

#[test]
fn a_file_whose_contents_havent_arrived_waits_for_them() {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let mut vm = armless::runner::new_vm(config).unwrap().0;
    // /etc/greeting: 6 bytes in blob "greeting-blob", which the host has not given yet
    vm.board
        .load_filesystem(br#"{"version":3,"fsroot":[["etc",0,0,16877,0,0,[["greeting",6,0,33188,0,0,"greeting-blob"]]]]}"#)
        .unwrap();
    let board = vm.board.clone();
    let mut p = Process::exec_bytes(
        &mut vm.cpus[0],
        Some(board.clone()),
        &cat_greeting(),
        &["cat".into()],
        &[],
    )
    .unwrap();
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(out.is_empty());
    assert!(
        matches!(status, Status::Idle(None)),
        "the read waits for its blob: {status:?}"
    );
    assert!(
        board
            .events()
            .iter()
            .any(|e| matches!(e, armless::Event::Blob(n) if n == "greeting-blob")),
        "and the machine asked its host for it"
    );
    board
        .provide_raw_blob("greeting-blob", b"hello\n".to_vec())
        .unwrap();
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"hello\r\n");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}
