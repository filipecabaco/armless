//! Programs run in user mode, written here as aarch64 machine code in a minimal static ELF, so
//! the tests need no cross toolchain.

use std::time::{Duration, Instant};

use armless::runner::NativeCpu;
use armless::vm::{Config, Vm};
use armless_user::{Process, Status};

const BASE: u64 = 0x40_0000;
/// ELF header and one program header: the code starts right after them.
const CODE: u64 = 64 + 56;

const fn movz(rd: u32, imm: u16) -> u32 {
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
    assert!(!p.waiting_on_blobs(), "waiting for input, not for a file");
    assert!(p.waiting_for_input());
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
        p.waiting_on_blobs(),
        "busy reading a file, not waiting for input"
    );
    assert!(!p.waiting_for_input());
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

// ---------------------------------------------------------------- signals

/// A little assembler: instructions with labels, and data after the code.
#[derive(Default)]
struct Asm {
    code: Vec<u32>,
    labels: std::collections::HashMap<&'static str, usize>,
    /// (instruction index, label, how to patch it)
    fixups: Vec<(usize, &'static str, Fix)>,
}

#[derive(Clone, Copy)]
enum Fix {
    /// b.cond / cbz / cbnz: imm19 at bit 5
    Imm19,
    /// b, bl: imm26
    Imm26,
    /// adr: immlo, immhi
    Adr,
}

const COND_EQ: u32 = 0;
const COND_NE: u32 = 1;

impl Asm {
    fn i(&mut self, w: u32) -> &mut Self {
        self.code.push(w);
        self
    }
    fn label(&mut self, l: &'static str) -> &mut Self {
        self.labels.insert(l, self.code.len());
        self
    }
    fn fix(&mut self, w: u32, l: &'static str, f: Fix) -> &mut Self {
        self.fixups.push((self.code.len(), l, f));
        self.i(w)
    }
    fn movz(&mut self, rd: u32, imm: u16) -> &mut Self {
        self.i(movz(rd, imm))
    }
    /// x`rd` = a 32-bit value
    fn mov32(&mut self, rd: u32, v: u32) -> &mut Self {
        self.i(movz(rd, v as u16));
        self.i(0xf2a0_0000 | (v >> 16) << 5 | rd) // movk xd, #hi, lsl 16
    }
    fn svc(&mut self, nr: u16) -> &mut Self {
        self.movz(8, nr).i(SVC)
    }
    fn adr(&mut self, rd: u32, l: &'static str) -> &mut Self {
        self.fix(0x1000_0000 | rd, l, Fix::Adr)
    }
    fn b(&mut self, l: &'static str) -> &mut Self {
        self.fix(0x1400_0000, l, Fix::Imm26)
    }
    fn bcond(&mut self, cond: u32, l: &'static str) -> &mut Self {
        self.fix(0x5400_0000 | cond, l, Fix::Imm19)
    }
    fn cbnz_fail(&mut self, rt: u32) -> &mut Self {
        self.fix(0xb500_0000 | rt, "fail", Fix::Imm19)
    }
    fn cmp(&mut self, rn: u32, rm: u32) -> &mut Self {
        self.i(0xeb00_001f | rm << 16 | rn << 5)
    }
    fn cmp_imm(&mut self, rn: u32, imm: u32) -> &mut Self {
        self.i(0xf100_001f | imm << 10 | rn << 5)
    }
    /// cmn xn, #imm: x`rn` == -imm
    fn cmn_imm(&mut self, rn: u32, imm: u32) -> &mut Self {
        self.i(0xb100_001f | imm << 10 | rn << 5)
    }
    /// str xt, [sp, #off]
    fn str_sp(&mut self, rt: u32, off: u32) -> &mut Self {
        self.i(0xf900_0000 | (off / 8) << 10 | 31 << 5 | rt)
    }
    /// write(1, label, len)
    fn print(&mut self, l: &'static str, len: u16) -> &mut Self {
        self.movz(0, 1).adr(1, l).movz(2, len).svc(64)
    }
    fn exit(&mut self, code: u16) -> &mut Self {
        self.movz(0, code).svc(94)
    }
    /// rt_sigaction(sig, {handler, flags, restorer, 0}, NULL, 8), the act built on the stack
    fn sigaction(&mut self, sig: u16, handler: &'static str, flags: u32) -> &mut Self {
        self.i(0xd100_83ff) // sub sp, sp, #32
            .adr(9, handler)
            .str_sp(9, 0)
            .mov32(9, flags)
            .str_sp(9, 8)
            .adr(9, "restorer")
            .str_sp(9, 16)
            .str_sp(31, 24)
            .i(0x9100_03e1) // mov x1, sp
            .movz(0, sig)
            .movz(2, 0)
            .movz(3, 8)
            .svc(134)
            .i(0x9100_83ff) // add sp, sp, #32
    }
    /// rt_sigprocmask(how, {1 << (sig - 1)}, NULL, 8)
    fn sigprocmask(&mut self, how: u16, sig: u32) -> &mut Self {
        self.i(0xd100_43ff) // sub sp, sp, #16
            .mov32(9, 1 << (sig - 1))
            .str_sp(9, 0)
            .i(0x9100_03e1) // mov x1, sp
            .movz(0, how)
            .movz(2, 0)
            .movz(3, 8)
            .svc(135)
            .i(0x9100_43ff) // add sp, sp, #16
    }
    /// The handler every signal test uses (prints "H", leaves its marks on registers and flags)
    /// and the restorer (rt_sigreturn), with the strings the programs print
    fn common(&mut self) -> &mut Self {
        self.label("handler")
            .print("H", 1)
            .movz(19, 0xdead)
            .i(0x9e67_0260) // fmov d0, x19
            .cmp_imm(19, 1) // flags: not equal
            .i(0xd65f_03c0) // ret
            .label("restorer")
            .svc(139)
    }

    /// The executable: the code, then the strings "A", "B", "E", "H", "R", "W" (labels of
    /// their own text).
    fn elf(&mut self) -> Vec<u8> {
        let strings = ["A", "B", "E", "H", "R", "W"];
        let at = self.code.len();
        for (i, s) in strings.iter().enumerate() {
            self.labels.insert(s, at + i);
        }
        let data: Vec<u8> = strings
            .iter()
            .flat_map(|s| [s.as_bytes()[0], 0, 0, 0])
            .collect();
        let mut code = self.code.clone();
        for &(i, l, f) in &self.fixups {
            let off = (self.labels[l] as i64 - i as i64) * 4;
            code[i] |= match f {
                Fix::Imm19 => ((off / 4) as u32 & 0x7_ffff) << 5,
                Fix::Imm26 => (off / 4) as u32 & 0x3ff_ffff,
                Fix::Adr => {
                    let imm = off as u32 & 0x1f_ffff;
                    (imm & 3) << 29 | (imm >> 2) << 5
                },
            };
        }
        elf(&code, &data)
    }
}

const SIGINT: u16 = 2;
const SIGUSR1: u16 = 10;
const SIGWINCH: u16 = 28;
const SA_SIGINFO: u32 = 4;
const SA_RESTORER: u32 = 0x0400_0000;
const SA_RESTART: u32 = 0x1000_0000;

fn start(elf: &[u8]) -> (Vm<armless::platform::NativePlatform>, Process) {
    let mut vm = machine();
    let p = Process::exec_bytes(&mut vm.cpus[0], None, elf, &["sig".into()], &[]).unwrap();
    (vm, p)
}

#[test]
fn a_handler_runs_and_sigreturn_gives_the_thread_back_whole() {
    let mut a = Asm::default();
    a.sigaction(SIGUSR1, "check", SA_SIGINFO | SA_RESTORER)
        .movz(19, 0x123)
        .i(0x9e67_0260) // fmov d0, x19
        .cmp(19, 19) // flags: equal
        .movz(0, 1000)
        .movz(1, SIGUSR1)
        .svc(129) // kill(getpid(), SIGUSR1): handled before kill returns
        .bcond(COND_NE, "fail") // the flags as they were
        .cbnz_fail(0) // kill's 0
        .cmp_imm(19, 0x123)
        .bcond(COND_NE, "fail")
        .i(0x9e66_0014) // fmov x20, d0
        .cmp(20, 19)
        .bcond(COND_NE, "fail")
        .print("B", 1)
        .exit(0)
        .label("fail")
        .exit(1)
        // The handler: x0 the signal, x1 its siginfo (si_signo), x2 the ucontext
        .label("check")
        .cmp_imm(0, SIGUSR1 as u32)
        .bcond(COND_NE, "fail")
        .i(0xb940_0029) // ldr w9, [x1]
        .cmp_imm(9, SIGUSR1 as u32)
        .bcond(COND_NE, "fail")
        .b("handler")
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"HB", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    assert_eq!(p.stats.signals_delivered, 1);
}

/// Blocks SIGUSR1, raises it, prints A, unblocks it (the handler prints H), prints B.
fn blocked_then_unblocked(wait_for_input: bool) -> Vec<u8> {
    let mut a = Asm::default();
    a.sigaction(SIGUSR1, "handler", SA_RESTORER)
        .sigprocmask(0, SIGUSR1 as u32) // SIG_BLOCK
        .movz(0, 1000)
        .movz(1, SIGUSR1)
        .svc(129)
        .print("A", 1);
    if wait_for_input {
        a.i(0xd101_03ff) // sub sp, sp, #64: read(0, sp, 64)
            .movz(0, 0)
            .i(0x9100_03e1)
            .movz(2, 64)
            .svc(63)
            .i(0x9101_03ff); // add sp, sp, #64
    }
    a.sigprocmask(1, SIGUSR1 as u32) // SIG_UNBLOCK
        .print("B", 1)
        .exit(0)
        .common();
    a.elf()
}

#[test]
fn a_blocked_signal_waits_until_it_is_unblocked() {
    let (mut vm, mut p) = start(&blocked_then_unblocked(false));
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"AHB");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_pending_signal_and_the_handlers_survive_a_snapshot() {
    let (mut vm, mut p) = start(&blocked_then_unblocked(true));
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process).unwrap();
    p.input(b"x\n");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(
        out, b"x\r\nHB",
        "SIGUSR1 still pending, its handler still set"
    );
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

/// A handler for `sig` with `flags`, then read(0): "E" and exit 4 on EINTR, "R" and exit 0 when
/// it reads.
fn reader(sig: u16, flags: u32) -> Vec<u8> {
    let mut a = Asm::default();
    a.sigaction(sig, "handler", flags)
        .i(0xd101_03ff) // sub sp, sp, #64
        .label("read")
        .movz(0, 0)
        .i(0x9100_03e1) // mov x1, sp
        .movz(2, 64)
        .svc(63)
        .cmn_imm(0, 4)
        .bcond(COND_EQ, "eintr")
        .print("R", 1)
        .exit(0)
        .label("eintr")
        .print("E", 1)
        .exit(4)
        .common();
    a.elf()
}

#[test]
fn ctrl_c_interrupts_a_read_with_eintr() {
    let (mut vm, mut p) = start(&reader(SIGINT, SA_RESTORER));
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    p.input(b"abc\x03");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(
        out, b"^CHE",
        "the echo flushed, ^C, the handler, then read's EINTR"
    );
    assert!(matches!(status, Status::Exited(4)), "{status:?}");
}

#[test]
fn sa_restart_starts_the_read_again_after_the_handler() {
    let (mut vm, mut p) = start(&reader(SIGINT, SA_RESTORER | SA_RESTART));
    run(&mut p, &mut vm.cpus[0]);
    p.kill(2); // the host's own interrupt
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"H");
    assert!(
        matches!(status, Status::Idle(None)),
        "reading again: {status:?}"
    );
    p.input(b"x\n");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"x\r\nR");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn sigint_with_no_handler_ends_the_program() {
    let (mut vm, mut p) = start(&echo());
    run(&mut p, &mut vm.cpus[0]);
    p.input(b"\x03");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"^C");
    assert!(matches!(status, Status::Exited(130)), "{status:?}");
}

#[test]
fn a_resize_is_sigwinch() {
    // No handler: ignored
    let (mut vm, mut p) = start(&echo());
    run(&mut p, &mut vm.cpus[0]);
    p.resize(100, 40);
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Idle(None)), "{status:?}");

    // A handler: once per change of size. With no SA_RESTORER (as Go has it on arm64) the
    // handler returns to a trampoline of the process's, as to the vDSO's on Linux
    let (mut vm, mut p) = start(&reader(SIGWINCH, SA_RESTART));
    run(&mut p, &mut vm.cpus[0]);
    p.resize(100, 40);
    assert_eq!(run(&mut p, &mut vm.cpus[0]).0, b"H");
    p.resize(100, 40);
    assert_eq!(
        run(&mut p, &mut vm.cpus[0]).0,
        b"",
        "the same size: no signal"
    );
    p.resize(120, 40);
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"H");
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
}

#[test]
fn a_sleep_a_handler_interrupts_returns_eintr_and_the_time_left() {
    let mut a = Asm::default();
    a.sigaction(SIGUSR1, "handler", SA_RESTORER | SA_RESTART)
        .i(0xd100_83ff) // sub sp, sp, #32: {10 s, 0} at sp, the time left at sp + 16
        .movz(9, 10)
        .str_sp(9, 0)
        .str_sp(31, 8)
        .i(0x9100_03e0) // mov x0, sp
        .i(0x9100_43e1) // add x1, sp, #16
        .svc(101) // nanosleep: EINTR whatever SA_RESTART says
        .cmn_imm(0, 4)
        .bcond(COND_NE, "fail")
        .i(0xf940_0be9) // ldr x9, [sp, #16]
        .cmp_imm(9, 9)
        .bcond(COND_NE, "fail")
        .print("E", 1)
        .exit(0)
        .label("fail")
        .exit(1)
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Idle(Some(_))), "{status:?}");
    assert!(!p.waiting_for_input(), "asleep, not waiting for input");
    p.kill(SIGUSR1 as u32);
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"HE", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

// ------------------------------------------------------------ files mapped lazily

/// mov xd, xm
fn mov(rd: u32, rm: u32) -> u32 {
    0xaa00_03e0 | rm << 16 | rd
}

/// add xd, xn, #imm (shifted by 12 when `hi`)
fn add_imm(rd: u32, rn: u32, imm: u32, hi: bool) -> u32 {
    0x9100_0000 | (hi as u32) << 22 | imm << 10 | rn << 5 | rd
}

/// ldrb wt, [xn]
fn ldrb(rt: u32, rn: u32) -> u32 {
    0x3940_0000 | rn << 5 | rt
}

/// strb wt, [xn]
fn strb(rt: u32, rn: u32) -> u32 {
    0x3900_0000 | rn << 5 | rt
}

/// add xd, xn, xm
fn add(rd: u32, rn: u32, rm: u32) -> u32 {
    0x8b00_0000 | rm << 16 | rn << 5 | rd
}

/// open("/data") into x21 and mmap two pages of it privately into x19 (x20 the second page),
/// then `rest`, then the path. Built twice: adr needs the code's length.
fn mapping_program(rest: &[u32]) -> Vec<u8> {
    let build = |n: usize| {
        let mut c = vec![
            0x9280_0c60, // movn x0, #99: AT_FDCWD
            0,           // adr x1, path
            movz(2, 0),  // O_RDONLY
            movz(8, 56), // openat
            SVC,
            mov(21, 0),
            mov(4, 0), // mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0)
            movz(0, 0),
            movz(1, 0x2000),
            movz(2, 3),
            movz(3, 2),
            movz(5, 0),
            movz(8, 222),
            SVC,
            mov(19, 0),
            add_imm(20, 19, 1, true),
        ];
        c[1] = adr(1, (4 * n.saturating_sub(1)) as i64);
        c.extend_from_slice(rest);
        c
    };
    let n = build(0).len();
    elf(&build(n), b"/data\0")
}

const EXIT: [u32; 2] = [movz(8, 94), SVC];

/// A machine whose filesystem has /prog (the program, mapped lazily too) and /data: two pages, in
/// a blob each ("hello..." and "B..."), which the host gives when asked.
fn lazy_machine(prog: &[u8]) -> Vm<armless::platform::NativePlatform> {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            fs_tag: Some("host9p".into()),
            ..Default::default()
        },
    };
    let vm = armless::runner::new_vm(config).unwrap().0;
    let json = format!(
        r#"{{"version":3,"fsroot":[["prog",{},0,33261,0,0,"prog-blob"],["data",8192,0,33188,0,0,[["page-a",4096],["page-b",4096]]]]}}"#,
        prog.len()
    );
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    vm.board
        .provide_raw_blob("prog-blob", prog.to_vec())
        .unwrap();
    vm
}

fn page(first: &[u8]) -> Vec<u8> {
    let mut p = first.to_vec();
    p.resize(4096, b'.');
    p
}

/// Runs to the end, giving the blobs the machine asks for: what it printed, how it ended, and the
/// blobs it asked for.
fn run_serving(
    p: &mut Process,
    vm: &mut Vm<armless::platform::NativePlatform>,
) -> (Vec<u8>, Status, Vec<String>) {
    let mut out = Vec::new();
    let mut asked = Vec::new();
    loop {
        let (o, status) = run(p, &mut vm.cpus[0]);
        out.extend(o);
        let wanted: Vec<String> = vm
            .board
            .events()
            .into_iter()
            .filter_map(|e| match e {
                armless::Event::Blob(n) => Some(n),
                _ => None,
            })
            .collect();
        if !p.waiting_on_blobs() {
            return (out, status, asked);
        }
        assert!(!wanted.is_empty(), "waits on blobs it didn't ask for");
        for n in wanted {
            let data = match n.as_str() {
                "page-a" => page(b"hello"),
                "page-b" => page(b"B"),
                _ => panic!("asked for {n}"),
            };
            vm.board.provide_raw_blob(&n, data).unwrap();
            asked.push(n);
        }
    }
}

fn exec_lazy(vm: &mut Vm<armless::platform::NativePlatform>) -> Process {
    let board = vm.board.clone();
    Process::exec(&mut vm.cpus[0], board, &["/prog".into()], &[], "/").unwrap()
}

#[test]
fn a_mapped_file_page_is_read_when_first_touched() {
    // exit_group(the first byte of the second page)
    let prog = mapping_program(&[ldrb(0, 20), EXIT[0], EXIT[1]]);
    let mut vm = lazy_machine(&prog);
    let mut p = exec_lazy(&mut vm);
    let (_, status, asked) = run_serving(&mut p, &mut vm);
    assert!(matches!(status, Status::Exited(66)), "{status:?}");
    assert_eq!(asked, ["page-b"], "only the page it touched");
    let (left, filled) = p.lazy_pages();
    assert_eq!(left, 1, "the first page of the file is still lazy");
    assert!(filled >= 2, "its own code, and the data page: {filled}");
}

#[test]
fn a_system_call_reads_a_mapped_page_it_was_given() {
    // write(1, the mapping, 5), exit_group(0): the host reads the page, not the program
    let prog = mapping_program(&[
        movz(0, 1),
        mov(1, 19),
        movz(2, 5),
        movz(8, 64),
        SVC,
        movz(0, 0),
        EXIT[0],
        EXIT[1],
    ]);
    let mut vm = lazy_machine(&prog);
    let mut p = exec_lazy(&mut vm);
    let (out, status, asked) = run_serving(&mut p, &mut vm);
    assert_eq!(out, b"hello");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    assert_eq!(asked, ["page-a"]);
}

#[test]
fn a_private_mapping_takes_writes_and_the_file_keeps_its_contents() {
    // 'J' over the mapping's first byte, write(1, mapping, 5), then pread64(fd, sp, 5, 0) and
    // write(1, sp, 5)
    let prog = mapping_program(&[
        movz(0, b'J' as u16),
        strb(0, 19),
        movz(0, 1),
        mov(1, 19),
        movz(2, 5),
        movz(8, 64),
        SVC,
        0xd101_03ff, // sub sp, sp, #64
        mov(0, 21),
        0x9100_03e1, // mov x1, sp
        movz(2, 5),
        movz(3, 0),
        movz(8, 67),
        SVC,
        movz(0, 1),
        0x9100_03e1, // mov x1, sp
        movz(2, 5),
        movz(8, 64),
        SVC,
        movz(0, 0),
        EXIT[0],
        EXIT[1],
    ]);
    let mut vm = lazy_machine(&prog);
    let mut p = exec_lazy(&mut vm);
    let (out, status, _) = run_serving(&mut p, &mut vm);
    assert_eq!(out, b"Jellohello");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn unmapping_or_mapping_over_lazy_pages_leaves_zeros_and_fetches_nothing() {
    let prog = mapping_program(&[
        // munmap(x19, 8192), then map the file again: x22
        mov(0, 19),
        movz(1, 0x2000),
        movz(8, 215),
        SVC,
        mov(4, 21),
        movz(0, 0),
        movz(1, 0x2000),
        movz(2, 3),
        movz(3, 2),
        movz(5, 0),
        movz(8, 222),
        SVC,
        mov(22, 0),
        // mmap(x22, 8192, RW, MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, -1, 0) over it
        movz(1, 0x2000),
        movz(2, 3),
        movz(3, 0x32),
        0x9280_0004, // movn x4, #0
        movz(5, 0),
        movz(8, 222),
        SVC,
        // exit_group(*x19 + *x20 + *x22 + *(x22 + 4096))
        ldrb(0, 19),
        ldrb(1, 20),
        add(0, 0, 1),
        ldrb(1, 22),
        add(0, 0, 1),
        add_imm(23, 22, 1, true),
        ldrb(1, 23),
        add(0, 0, 1),
        EXIT[0],
        EXIT[1],
    ]);
    let mut vm = lazy_machine(&prog);
    let mut p = exec_lazy(&mut vm);
    let (_, status, asked) = run_serving(&mut p, &mut vm);
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    assert!(asked.is_empty(), "{asked:?}");
    assert_eq!(p.lazy_pages().0, 0, "nothing lazy is left");
}

#[test]
fn a_saved_process_keeps_its_untouched_pages_lazy() {
    // Touch the first page, wait for a byte of input, then exit_group(*x19 + *x20)
    let prog = mapping_program(&[
        ldrb(24, 19),
        0xd101_03ff, // sub sp, sp, #64
        movz(0, 0),
        0x9100_03e1, // mov x1, sp
        movz(2, 1),
        movz(8, 63),
        SVC,
        ldrb(1, 20),
        add(0, 24, 1),
        EXIT[0],
        EXIT[1],
    ]);
    let mut vm = lazy_machine(&prog);
    let mut p = exec_lazy(&mut vm);
    let (_, status, asked) = run_serving(&mut p, &mut vm);
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    assert_eq!(asked, ["page-a"]);
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = lazy_machine(&prog);
    vm.load_state(&state).unwrap();
    let board = vm.board.clone();
    let mut p = Process::restore(&mut vm.cpus[0], Some(board), &process).unwrap();
    assert_eq!(p.lazy_pages().0, 1, "the second page is still lazy");
    p.input(b"x\n");
    let (_, status, asked) = run_serving(&mut p, &mut vm);
    assert!(matches!(status, Status::Exited(170)), "h + B: {status:?}");
    assert_eq!(asked, ["page-b"], "the first page came with the snapshot");
}

// ------------------------------------------------------------ SIGILL

const SIGILL: u16 = 4;
/// sm3partw1 v4.4s, v0.4s, v3.4s: OpenSSL's probe for SM3, which armless doesn't have
const SM3_PROBE: u32 = 0xce63_c004;

/// A program probes for an extension as OpenSSL does: it runs one of its instructions under a
/// SIGILL handler, which gets the signal with ILL_ILLOPC and the instruction's address, and goes
/// on without it (OpenSSL's siglongjmp: here, it exits 5).
#[test]
fn an_instruction_the_cpu_lacks_is_sigill_for_its_handler() {
    let mut a = Asm::default();
    a.sigaction(SIGILL, "ill", SA_SIGINFO | SA_RESTORER)
        .label("probe")
        .i(SM3_PROBE)
        .print("B", 1) // the probe passed: it can't here
        .exit(0)
        .label("ill")
        .cmp_imm(0, SIGILL as u32)
        .bcond(COND_NE, "fail")
        .i(0xb940_0029) // ldr w9, [x1]: si_signo
        .cmp_imm(9, SIGILL as u32)
        .bcond(COND_NE, "fail")
        .i(0xb940_0829) // ldr w9, [x1, #8]: si_code
        .cmp_imm(9, 1) // ILL_ILLOPC
        .bcond(COND_NE, "fail")
        .i(0xf940_082b) // ldr x11, [x1, #16]: si_addr
        .adr(10, "probe")
        .cmp(11, 10)
        .bcond(COND_NE, "fail")
        .print("E", 1)
        .exit(5)
        .label("fail")
        .exit(1)
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"E", "{status:?}");
    assert!(matches!(status, Status::Exited(5)), "{status:?}");
    assert_eq!(p.stats.signals_delivered, 1);
}

/// With no handler, an instruction the CPU lacks ends the program as SIGILL does on Linux.
#[test]
fn an_instruction_the_cpu_lacks_ends_a_program_without_a_handler() {
    let mut a = Asm::default();
    a.i(SM3_PROBE).print("B", 1).exit(0).common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"");
    assert!(
        matches!(status, Status::Exited(132)),
        "128 + SIGILL: {status:?}"
    );
}

// ------------------------------------------------------------ socket pairs

/// socketpair(AF_UNIX, SOCK_STREAM, 0, sp): the ends in w19 and w20, the pair at sp (64 bytes
/// of stack: the ends, then room for what is read).
fn socketpair(a: &mut Asm) -> &mut Asm {
    a.i(0xd101_03ff) // sub sp, sp, #64
        .movz(0, 1) // AF_UNIX
        .movz(1, 1) // SOCK_STREAM
        .movz(2, 0)
        .i(0x9100_03e3) // mov x3, sp
        .svc(199)
        .cbnz_fail(0)
        .i(0xb940_03f3) // ldr w19, [sp]
        .i(0xb940_07f4) // ldr w20, [sp, #4]
}

/// read(fd, sp + 16, 64) into x0
fn read_into_stack(a: &mut Asm, fd: u32) -> &mut Asm {
    a.i(mov(0, fd)).i(0x9100_43e1).movz(2, 64).svc(63) // add x1, sp, #16
}

/// write(1, sp + 16, 1): the byte read
fn print_read(a: &mut Asm) -> &mut Asm {
    a.movz(0, 1).i(0x9100_43e1).movz(2, 1).svc(64)
}

/// What one end writes the other reads, with write and read, sendto and recvfrom; a read that
/// would wait is EAGAIN with MSG_DONTWAIT; after shutdown(SHUT_WR) the other end reads its end;
/// getsockopt says it's a stream.
#[test]
fn a_socketpair_carries_bytes_between_its_ends() {
    let mut a = Asm::default();
    socketpair(&mut a);
    // write(end 0, "B") and read it at end 1
    a.i(mov(0, 19)).adr(1, "B").movz(2, 1).svc(64);
    read_into_stack(&mut a, 20)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    print_read(&mut a);
    // sendto(end 1, "R", 1, 0, NULL, 0) and recvfrom it at end 0
    a.i(mov(0, 20))
        .adr(1, "R")
        .movz(2, 1)
        .movz(3, 0)
        .movz(4, 0)
        .movz(5, 0)
        .svc(206)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail")
        .i(mov(0, 19))
        .i(0x9100_43e1)
        .movz(2, 64)
        .movz(3, 0)
        .movz(4, 0)
        .movz(5, 0)
        .svc(207)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    print_read(&mut a);
    // Nothing more: recvfrom(end 0, MSG_DONTWAIT) is EAGAIN
    a.i(mov(0, 19))
        .i(0x9100_43e1)
        .movz(2, 64)
        .movz(3, 0x40)
        .movz(4, 0)
        .svc(207)
        .cmn_imm(0, 11)
        .bcond(COND_NE, "fail")
        // shutdown(end 0, SHUT_WR): end 1 reads its end of input
        .i(mov(0, 19))
        .movz(1, 1)
        .svc(210)
        .cbnz_fail(0);
    read_into_stack(&mut a, 20).cbnz_fail(0);
    // getsockopt(end 0, SOL_SOCKET, SO_TYPE, sp + 16, sp + 24): SOCK_STREAM
    a.movz(9, 4)
        .i(0xb900_1be9) // str w9, [sp, #24]
        .i(mov(0, 19))
        .movz(1, 1)
        .movz(2, 3)
        .i(0x9100_43e3) // add x3, sp, #16
        .i(0x9100_63e4) // add x4, sp, #24
        .svc(209)
        .cbnz_fail(0)
        .i(0xb940_13e9) // ldr w9, [sp, #16]
        .cmp_imm(9, 1)
        .bcond(COND_NE, "fail")
        .print("E", 1)
        .exit(0)
        .label("fail")
        .exit(1)
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"BRE", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

/// What a socketpair holds is kept with the process: written before a snapshot, it is read after
/// the process goes on in another machine.
#[test]
fn a_socketpairs_bytes_survive_a_snapshot() {
    let mut a = Asm::default();
    socketpair(&mut a);
    a.i(mov(0, 19)).adr(1, "W").movz(2, 1).svc(64);
    // read(0, sp + 32, 32): waits for input, the snapshot's moment
    a.movz(0, 0).i(0x9100_83e1).movz(2, 32).svc(63);
    read_into_stack(&mut a, 20)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    print_read(&mut a).exit(0).label("fail").exit(1).common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"");
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], None, &process).unwrap();
    p.input(b"x\n");
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"x\r\nW");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

/// An end with bytes waiting is readable to poll (as asyncio and tokio wait on theirs), and the
/// other end, with room, writable.
#[test]
fn poll_sees_a_socketpair_end_readable() {
    let mut a = Asm::default();
    socketpair(&mut a);
    a.i(mov(0, 19)).adr(1, "W").movz(2, 1).svc(64);
    // struct pollfd at sp + 16: { end 1, POLLIN }, then { end 0, POLLOUT } at sp + 24
    a.i(0xb900_13f4) // str w20, [sp, #16]
        .movz(9, 1)
        .i(0x7900_2be9) // strh w9, [sp, #20]
        .i(0xb900_1bf3) // str w19, [sp, #24]
        .movz(9, 4)
        .i(0x7900_3be9) // strh w9, [sp, #28]
        .i(0x9100_43e0) // add x0, sp, #16
        .movz(1, 2)
        .movz(2, 0)
        .movz(3, 0)
        .movz(4, 8)
        .svc(73) // ppoll(fds, 2, NULL, NULL, 8): both ready
        .cmp_imm(0, 2)
        .bcond(COND_NE, "fail")
        .i(0x7940_2fe9) // ldrh w9, [sp, #22]: revents
        .cmp_imm(9, 1)
        .bcond(COND_NE, "fail")
        .i(0x7940_3fe9) // ldrh w9, [sp, #30]
        .cmp_imm(9, 4)
        .bcond(COND_NE, "fail")
        .print("E", 1)
        .exit(0)
        .label("fail")
        .exit(1)
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"E", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

// ------------------------------------------------------------ futex requeue

/// A thread waiting at futex A is moved by FUTEX_CMP_REQUEUE to wait at B (as musl's condition
/// variables hand their waiters to the mutex), so a wake of B wakes it: "R" once it's moved, "B"
/// once B's wake took it, "W" from the thread itself.
#[test]
fn futex_requeue_moves_a_waiter_to_another_address() {
    let mut a = Asm::default();
    a.i(0xd101_03ff) // sub sp, sp, #64: A at sp, B at sp + 8
        .i(0xf900_03ff) // str xzr, [sp]
        .i(0xf900_07ff) // str xzr, [sp, #8]
        .i(0x9100_03f5) // mov x21, sp: A
        .i(0x9100_23f6) // add x22, sp, #8: B
        // clone(CLONE_VM | FS | FILES | SIGHAND | THREAD, sp - 4096, 0, 0, 0)
        .movz(0, 0x0f00)
        .i(0xf2a0_0020) // movk x0, #1, lsl #16
        .i(0xd140_07e1) // sub x1, sp, #4096
        .movz(2, 0)
        .movz(3, 0)
        .movz(4, 0)
        .svc(220)
        .cmp_imm(0, 0)
        .bcond(COND_EQ, "child")
        // FUTEX_CMP_REQUEUE_PRIVATE(A, wake 0, requeue 1, B, while *A == 0): once the thread waits
        .label("requeue")
        .i(mov(0, 21))
        .movz(1, 0x84)
        .movz(2, 0)
        .movz(3, 1)
        .i(mov(4, 22))
        .movz(5, 0)
        .svc(98)
        .cmp_imm(0, 1)
        .bcond(COND_EQ, "moved")
        .svc(124) // sched_yield: let the thread run to its wait
        .b("requeue")
        .label("moved")
        .print("R", 1)
        // FUTEX_WAKE_PRIVATE(B, 1): the moved thread
        .i(mov(0, 22))
        .movz(1, 0x81)
        .movz(2, 1)
        .svc(98)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail")
        .print("B", 1)
        .movz(19, 20)
        .label("yield")
        .svc(124)
        .i(0xd100_0673) // sub x19, x19, #1
        .cmp_imm(19, 0)
        .bcond(COND_NE, "yield")
        .exit(0)
        // The thread: FUTEX_WAIT_PRIVATE(A, 0), then "W" and its own exit
        .label("child")
        .i(mov(0, 21))
        .movz(1, 0x80)
        .movz(2, 0)
        .movz(3, 0)
        .svc(98)
        .cbnz_fail(0)
        .print("W", 1)
        .movz(0, 0)
        .svc(93)
        .label("fail")
        .exit(1)
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"RBW", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}
