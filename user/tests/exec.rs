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
    /// Strings of its own after the standard ones, NUL-terminated and padded to a word
    texts: Vec<(&'static str, &'static [u8])>,
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
    fn with_texts(texts: Vec<(&'static str, &'static [u8])>) -> Asm {
        Asm {
            texts,
            ..Asm::default()
        }
    }
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
        let mut data: Vec<u8> = strings
            .iter()
            .flat_map(|s| [s.as_bytes()[0], 0, 0, 0])
            .collect();
        for &(l, text) in &self.texts {
            self.labels.insert(l, at + data.len() / 4);
            data.extend_from_slice(text);
            data.resize((data.len() / 4 + 1) * 4, 0);
        }
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

// ------------------------------------------------------------ memory protections

const PROT_READ: u16 = 1;
const PROT_WRITE: u16 = 2;
const PROT_EXEC: u16 = 4;

impl Asm {
    /// mmap(0, 4096, prot, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) into x19
    fn mmap_anon(&mut self, prot: u16) -> &mut Self {
        self.movz(0, 0)
            .movz(1, 4096)
            .movz(2, prot)
            .movz(3, 0x22)
            .i(0x9280_0004) // movn x4, #0
            .movz(5, 0)
            .svc(222)
            .i(mov(19, 0))
    }
    /// mprotect(x19, 4096, prot)
    fn mprotect(&mut self, prot: u16) -> &mut Self {
        self.i(mov(0, 19)).movz(1, 4096).movz(2, prot).svc(226)
    }
    fn munmap(&mut self) -> &mut Self {
        self.i(mov(0, 19)).movz(1, 4096).svc(215)
    }
    /// exit_group(the byte at x19)
    fn exit_byte(&mut self) -> &mut Self {
        self.i(ldrb(0, 19)).svc(94)
    }
    fn run_program(&mut self) -> Status {
        let (mut vm, mut p) = start(&self.elf());
        run(&mut p, &mut vm.cpus[0]).1
    }
}

#[test]
fn memory_protections_hold() {
    let faults = |s: Status| matches!(s, Status::Fault(_));
    // A read-only page reads as zero, and a store to it is a fault
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ)
        .i(ldrb(20, 19))
        .movz(21, 7)
        .i(strb(21, 19))
        .exit(0);
    assert!(faults(a.run_program()));

    // mprotect makes it writable: the store takes
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ)
        .i(ldrb(20, 19))
        .mprotect(PROT_READ | PROT_WRITE)
        .movz(21, 7)
        .i(strb(21, 19))
        .exit_byte();
    assert!(matches!(a.run_program(), Status::Exited(7)));

    // and read-only again after its pages were had: the store faults
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ | PROT_WRITE)
        .movz(21, 7)
        .i(strb(21, 19))
        .mprotect(PROT_READ)
        .i(strb(21, 19))
        .exit(0);
    assert!(faults(a.run_program()));

    // PROT_NONE: not even a read
    let mut a = Asm::default();
    a.mmap_anon(0).i(ldrb(20, 19)).exit(0);
    assert!(faults(a.run_program()));

    // Unmapped: a read faults; the byte it wrote is gone with the page
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ | PROT_WRITE)
        .movz(21, 7)
        .i(strb(21, 19))
        .munmap()
        .exit_byte();
    assert!(faults(a.run_program()));

    // A page that isn't executable can't run: jump to anonymous memory
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ | PROT_WRITE).i(0xd61f_0260).exit(0); // br x19
    assert!(faults(a.run_program()));

    // and one that is executable runs: `mov x0, #9; mov x8, #94; svc` copied there, then br
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ | PROT_WRITE);
    for (i, w) in [movz(0, 9), movz(8, 94), SVC].into_iter().enumerate() {
        a.mov32(21, w).movz(22, i as u16 * 4).i(add(22, 19, 22));
        a.i(0xb900_02d5); // str w21, [x22]
    }
    a.mprotect(PROT_READ | PROT_EXEC).i(0xd61f_0260).exit(0);
    assert!(matches!(a.run_program(), Status::Exited(9)));
}

#[test]
fn a_system_call_cant_write_where_the_program_cant() {
    // read(0, a read-only page, 1) is EFAULT, and exit_group(-errno)
    let mut a = Asm::default();
    a.mmap_anon(PROT_READ)
        .movz(0, 0)
        .i(mov(1, 19))
        .movz(2, 1)
        .svc(63)
        .i(0xcb00_03e0) // neg x0, x0
        .svc(94);
    let (mut vm, mut p) = start(&a.elf());
    p.input(b"x\n");
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Exited(14)), "EFAULT: {status:?}");
}

#[test]
fn the_break_gives_pages_back_and_keeps_what_is_below_it() {
    // brk(0) into x19; brk(x19 + 8192); two stores; brk(x19 + 4096): the first page stays, the
    // second is gone and comes back as zeros when the break grows again
    let mut a = Asm::default();
    a.movz(0, 0).svc(214).i(mov(19, 0));
    a.i(add_imm(0, 19, 2, true)).svc(214); // brk(x19 + 8192)
    a.movz(21, 5).i(strb(21, 19));
    a.i(add_imm(20, 19, 1, true)).movz(21, 6).i(strb(21, 20));
    a.i(mov(0, 20)).svc(214); // shrink to the first page
    a.i(add_imm(0, 19, 2, true)).svc(214); // and grow again
    a.i(ldrb(22, 19)).i(ldrb(23, 20)); // 5, and 0 again
    a.i(add(0, 22, 23)).svc(94);
    let (mut vm, mut p) = start(&a.elf());
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Exited(5)), "{status:?}");
}

#[test]
fn mremap_moves_pages_without_copying_them() {
    // Two pages mapped, a byte in each; mremap to four, MAYMOVE past a page that blocks growth
    // in place; both bytes follow
    let mut a = Asm::default();
    a.movz(0, 0)
        .movz(1, 0x2000)
        .movz(2, 3)
        .movz(3, 0x22)
        .i(0x9280_0004)
        .movz(5, 0)
        .svc(222)
        .i(mov(19, 0));
    // the page right below is taken, so the range can't grow downwards in place: a second mmap
    // lands just below the first (highest free range first), and the first's growth is blocked
    // by nothing above it only if free; map one above to be sure
    a.movz(21, 3).i(strb(21, 19));
    a.i(add_imm(20, 19, 1, true)).movz(21, 4).i(strb(21, 20));
    a.i(mov(0, 19))
        .movz(1, 0x2000)
        .movz(2, 0x4000)
        .movz(3, 1)
        .svc(216)
        .i(mov(22, 0));
    a.i(ldrb(23, 22));
    a.i(add_imm(24, 22, 1, true)).i(ldrb(25, 24));
    a.i(add(0, 23, 25)).svc(94);
    let (mut vm, mut p) = start(&a.elf());
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Exited(7)), "{status:?}");
}

// ------------------------------------------------------------ the address space is not the RAM

/// movz xd, #imm, lsl #32
fn movz_hi(rd: u32, imm: u32) -> u32 {
    0xd2c0_0000 | imm << 5 | rd
}

impl Asm {
    /// x`rd` = mmap(0, `gib` GiB, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0)
    fn reserve(&mut self, rd: u32, gib: u32) -> &mut Self {
        self.movz(0, 0)
            .i(movz_hi(1, gib / 4))
            .movz(2, 0)
            .movz(3, 0x4022)
            .i(0x9280_0004) // movn x4, #0
            .movz(5, 0)
            .svc(222)
            .i(mov(rd, 0))
    }
}

#[test]
fn a_reservation_larger_than_ram_costs_frames_only_where_it_is_touched() {
    // The machine has 128 MiB. The program reserves 8 GiB (a JavaScript engine's heap cage, a
    // WebAssembly memory's guard region): that costs no frame. Three pages 4 GiB into it (past
    // the end of RAM: a virtual address is not a physical one) are made writable and written:
    // they and their tables cost frames, and nothing else does. madvise(DONTNEED) gives two of
    // them back; they read as zeros again, which takes frames again, and munmap gives back
    // everything, tables too.
    let mut a = Asm::default();
    a.room().read_byte(31); // (x31 reads as 0: stdin) wait: nothing mapped yet
    a.reserve(19, 8);
    a.i(movz_hi(22, 1)).i(add(22, 19, 22)); // x22 = x19 + 4 GiB
    a.i(mov(0, 22))
        .movz(1, 3 * 4096)
        .movz(2, 3)
        .svc(226)
        .cbnz_fail(0);
    a.read_byte(31); // wait: reserved, and nothing touched
    for page in 0..3 {
        a.i(add_imm(23, 22, page, true))
            .movz(21, 7 + page as u16)
            .i(strb(21, 23));
    }
    a.read_byte(31); // wait: three pages written
    a.i(mov(0, 22))
        .movz(1, 2 * 4096)
        .movz(2, 4)
        .svc(233)
        .cbnz_fail(0); // MADV_DONTNEED
    a.read_byte(31); // wait: two pages given back
    a.i(ldrb(20, 22));
    a.i(add_imm(23, 22, 1, true))
        .i(ldrb(21, 23))
        .i(add(20, 20, 21));
    a.cbnz_fail(20); // both read as zero
    a.i(add_imm(23, 22, 2, true))
        .i(ldrb(21, 23))
        .cmp_imm(21, 9)
        .bcond(COND_NE, "fail");
    a.i(mov(0, 19)).i(movz_hi(1, 2)).svc(215).cbnz_fail(0); // munmap all 8 GiB
    a.read_byte(31); // wait: nothing left
    a.ends();
    let (mut vm, mut p) = start(&a.elf());
    let mut frames = Vec::new();
    for _ in 0..5 {
        let (_, status) = run(&mut p, &mut vm.cpus[0]);
        assert!(
            matches!(status, Status::Idle(None)),
            "waits for input: {status:?}"
        );
        frames.push(p.frames_in_use());
        p.input(b"\n");
    }
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(out.ends_with(b"A"), "{out:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    let [before, reserved, touched, advised, unmapped] = frames[..] else {
        unreachable!()
    };
    assert_eq!(
        reserved, before,
        "8 GiB reserved, and no frame taken for it"
    );
    // three pages, a table of the second level and one or two of the third (the pages are in
    // one range of 2 MiB or straddle two)
    assert!((5..=6).contains(&(touched - reserved)), "{frames:?}");
    assert_eq!(advised, touched - 2, "{frames:?}");
    assert_eq!(unmapped, before, "{frames:?}");
}

#[test]
fn the_address_space_holds_many_reservations_and_ends_where_it_ends() {
    // Sixteen reservations of 8 GiB (128 GiB, a thousand times the RAM) all succeed, and are not
    // the same range; a terabyte is more than the space and is ENOMEM
    let mut a = Asm::default();
    a.room().movz(19, 0);
    for _ in 0..16 {
        a.i(mov(20, 19)).reserve(19, 8);
        a.cmn_imm(19, 4095).bcond(2, "fail"); // not an errno
        a.cmp(19, 20).bcond(COND_EQ, "fail");
    }
    a.movz(0, 0)
        .i(movz_hi(1, 256))
        .movz(2, 0)
        .movz(3, 0x4022)
        .i(0x9280_0004)
        .movz(5, 0)
        .svc(222);
    a.cmn_imm(0, 12).bcond(COND_NE, "fail"); // ENOMEM
    a.ends();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

// ------------------------------------------------------------ processes

const CLONE_VM: u32 = 0x100;
const CLONE_VFORK: u32 = 0x4000;
const SIGCHLD: u16 = 17;
const SIGKILL: u16 = 9;
const WNOHANG: u16 = 1;

impl Asm {
    /// cbz xt, label
    fn cbz_to(&mut self, rt: u32, l: &'static str) -> &mut Self {
        self.fix(0xb400_0000 | rt, l, Fix::Imm19)
    }
    /// ldr wt, [xn, #off]
    fn ldr_w(&mut self, rt: u32, rn: u32, off: u32) -> &mut Self {
        self.i(0xb940_0000 | (off / 4) << 10 | rn << 5 | rt)
    }
    /// sub sp, sp, #64: room for a wait status at [sp], pipe fds at [sp + 8] and a byte at
    /// [sp + 16]
    fn room(&mut self) -> &mut Self {
        self.i(0xd101_03ff)
    }
    /// x0 = clone(flags, 0, 0, 0, 0): the child's stack is its parent's (a copy, or the same)
    fn clone(&mut self, flags: u32) -> &mut Self {
        self.mov32(0, flags)
            .movz(1, 0)
            .movz(2, 0)
            .movz(3, 0)
            .movz(4, 0)
            .svc(220)
    }
    /// x0 = fork(): the child gets 0
    fn fork(&mut self) -> &mut Self {
        self.clone(SIGCHLD as u32)
    }
    /// wait4(pid, &[sp], options, 0): any child when `pid` is None, else the pid in x`pid`
    fn wait4(&mut self, pid: Option<u32>, options: u16) -> &mut Self {
        match pid {
            Some(r) => self.i(mov(0, r)),
            None => self.i(0x9280_0000), // movn x0, #0: -1
        };
        self.i(0x9100_03e1) // mov x1, sp
            .movz(2, options)
            .movz(3, 0)
            .svc(260)
    }
    /// pipe2(&[sp + 8], 0), the read end into x23 and the write end into x24
    fn pipe(&mut self) -> &mut Self {
        self.i(add_imm(0, 31, 8, false))
            .movz(1, 0)
            .svc(59)
            .cbnz_fail(0);
        self.ldr_w(23, 31, 8).ldr_w(24, 31, 12)
    }
    /// read(x`fd`, [sp + 16], 1) into x0
    fn read_byte(&mut self, fd: u32) -> &mut Self {
        self.i(mov(0, fd))
            .i(add_imm(1, 31, 16, false))
            .movz(2, 1)
            .svc(63)
    }
    /// write(x`fd`, "W", 1)
    fn write_w(&mut self, fd: u32) -> &mut Self {
        self.i(mov(0, fd)).adr(1, "W").movz(2, 1).svc(64)
    }
    /// the status wait4 stored must be `status`
    fn status_is(&mut self, status: u32) -> &mut Self {
        self.ldr_w(1, 31, 0)
            .cmp_imm(1, status)
            .bcond(COND_NE, "fail")
    }
    /// "A" and exit(0): the happy ending; then "E" and exit(1) at the label "fail"
    fn ends(&mut self) -> &mut Self {
        self.print("A", 1)
            .exit(0)
            .label("fail")
            .print("E", 1)
            .exit(1)
    }
}

#[test]
fn a_forked_child_ends_alone_and_its_parent_hears_how() {
    // The child is its own process: its ids are its own, its exit_group ends only it, and
    // wait4 gives the parent the pid and the status (code << 8)
    let mut a = Asm::default();
    a.room().movz(0, 0).svc(172).i(mov(20, 0)); // x20 = my pid
    a.fork().cbz_to(0, "child");
    a.i(mov(19, 0)); // the child's pid
    a.cmp(19, 20).bcond(COND_EQ, "fail");
    a.wait4(None, 0).cmp(0, 19).bcond(COND_NE, "fail");
    a.status_is(5 << 8).ends();
    a.label("child").svc(172).cmp(0, 20).bcond(COND_EQ, "fail"); // not the parent's pid
    a.svc(173).cmp(0, 20).bcond(COND_NE, "fail"); // but its parent is
    a.svc(178)
        .i(mov(19, 0))
        .svc(172)
        .cmp(0, 19)
        .bcond(COND_NE, "fail"); // tid == pid
    a.exit(5);
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn the_machine_ends_when_init_does_with_children_still_alive() {
    let mut a = Asm::default();
    a.fork().cbz_to(0, "child");
    a.exit(3);
    a.label("child").label("spin").b("spin");
    let (mut vm, mut p) = start(&a.elf());
    let (_, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(matches!(status, Status::Exited(3)), "{status:?}");
}

#[test]
fn a_fork_shares_pages_until_either_side_writes() {
    // 3 before the fork; the parent writes 7 and then tells the child through a pipe, which
    // reads its page (still 3) and writes 2; the parent's page is still 7 when it is done
    let mut a = Asm::default();
    a.room().pipe().mmap_anon(PROT_READ | PROT_WRITE);
    a.movz(21, 3).i(strb(21, 19));
    a.fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.movz(21, 7).i(strb(21, 19));
    a.write_w(24).cmp_imm(0, 1).bcond(COND_NE, "fail");
    a.wait4(Some(20), 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(3 << 8);
    a.i(ldrb(22, 19)).cmp_imm(22, 7).bcond(COND_NE, "fail");
    a.ends();
    a.label("child")
        .read_byte(23)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    a.i(ldrb(22, 19)).movz(21, 2).i(strb(21, 19));
    a.i(ldrb(21, 19)).cmp_imm(21, 2).bcond(COND_NE, "fail");
    a.i(mov(0, 22)).svc(94);
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_pipe_joins_a_parent_and_its_child_and_closes_when_both_have_let_go() {
    // The parent reads before the child has written: it waits, the child runs. The second read
    // gets the end of the file only because the child's copy of the write end went with it.
    let mut a = Asm::default();
    a.room().pipe().fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.i(mov(0, 24)).svc(57); // close the write end
    a.read_byte(23).cmp_imm(0, 1).bcond(COND_NE, "fail");
    a.movz(0, 1).i(add_imm(1, 31, 16, false)).movz(2, 1).svc(64); // and echo it
    a.read_byte(23).cbnz_fail(0);
    a.wait4(None, 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(0).ends();
    a.label("child").i(mov(0, 23)).svc(57);
    a.adr(1, "B").i(mov(0, 24)).movz(2, 1).svc(64);
    a.exit(0);
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"BA");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_vfork_child_runs_in_its_parents_memory_and_the_parent_waits_for_it() {
    // CLONE_VM | CLONE_VFORK: the child's write lands in the parent's page, and the parent
    // doesn't run until the child has ended, so it sees the write at once
    let mut a = Asm::default();
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.movz(21, 1).i(strb(21, 19));
    a.clone(CLONE_VM | CLONE_VFORK | SIGCHLD as u32)
        .cbz_to(0, "child");
    a.i(mov(20, 0));
    a.i(ldrb(22, 19)).cmp_imm(22, 5).bcond(COND_NE, "fail");
    a.wait4(Some(20), 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(3 << 8).ends();
    a.label("child").movz(21, 5).i(strb(21, 19)).exit(3);
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn wait4_says_zero_for_a_running_child_and_echild_when_none_are_left() {
    // The child waits on a pipe nobody writes; WNOHANG finds it running; SIGKILL ends it with
    // status 9; a second wait4 finds no children
    let mut a = Asm::default();
    a.room().pipe().fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.wait4(Some(20), WNOHANG).cbnz_fail(0);
    a.i(mov(0, 20)).movz(1, SIGKILL).svc(129).cbnz_fail(0);
    a.wait4(None, 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(9);
    a.wait4(None, 0).cmn_imm(0, 10).bcond(COND_NE, "fail"); // -ECHILD
    a.ends();
    a.label("child").read_byte(23).b("fail");
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_child_that_ends_signals_its_parent_with_sigchld() {
    let mut a = Asm::default();
    a.room()
        .sigaction(SIGCHLD, "handler", SA_RESTORER | SA_RESTART);
    a.fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.wait4(None, 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(0).ends();
    a.label("child").exit(0);
    a.common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"HA");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn processes_survive_a_snapshot_with_their_shared_pages() {
    // The child waits for stdin; the machine is saved there and goes on in another one. The
    // child's page is still shared with its parent's: its write stays its own.
    let mut a = Asm::default();
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.movz(21, 3).i(strb(21, 19));
    a.fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.wait4(None, 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(3 << 8);
    a.i(ldrb(22, 19)).cmp_imm(22, 3).bcond(COND_NE, "fail");
    a.ends();
    a.label("child")
        .movz(0, 0)
        .i(add_imm(1, 31, 16, false))
        .movz(2, 1)
        .svc(63);
    a.i(ldrb(22, 19)).movz(21, 4).i(strb(21, 19));
    a.i(mov(0, 22)).svc(94);
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
    assert_eq!(out, b"x\r\nA");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

// ------------------------------------------------------------ exec

/// ldr xt, [xn, #off]
fn ldr(rt: u32, rn: u32, off: u32) -> u32 {
    0xf940_0000 | (off / 8) << 10 | rn << 5 | rt
}

/// A machine whose filesystem has these files (name, mode, contents: a symlink's target). The contents of those not in
/// `now` come as blobs when asked for: `run_with_files` gives them.
fn machine_with(
    files: &[(&str, u32, &[u8])],
    now: &[&str],
) -> Vm<armless::platform::NativePlatform> {
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
    let entries: Vec<String> = files
        .iter()
        .map(|(n, mode, data)| {
            // A symlink's contents are its target
            let target = if mode & 0o170000 == 0o120000 {
                String::from_utf8_lossy(data).into_owned()
            } else {
                format!("{n}-blob")
            };
            format!(r#"["{n}",{},0,{mode},0,0,"{target}"]"#, data.len())
        })
        .collect();
    let json = format!(r#"{{"version":3,"fsroot":[{}]}}"#, entries.join(","));
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    for (n, _, data) in files {
        if now.contains(n) {
            vm.board
                .provide_raw_blob(&format!("{n}-blob"), data.to_vec())
                .unwrap();
        }
    }
    vm
}

/// Runs to the end, giving the files' blobs when the machine asks for them: what it printed, how
/// it ended, and the blobs it asked for.
fn run_with_files(
    p: &mut Process,
    vm: &mut Vm<armless::platform::NativePlatform>,
    files: &[(&str, u32, &[u8])],
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
            let (_, _, data) = files
                .iter()
                .find(|(f, _, _)| n == format!("{f}-blob"))
                .unwrap_or_else(|| panic!("asked for {n}"));
            vm.board.provide_raw_blob(&n, data.to_vec()).unwrap();
            asked.push(n);
        }
    }
}

const EXE: u32 = 0o100755;
const PLAIN: u32 = 0o100644;

/// The first process of a machine with these files, from /prog.
fn exec_prog(vm: &mut Vm<armless::platform::NativePlatform>) -> Process {
    let board = vm.board.clone();
    Process::exec(&mut vm.cpus[0], board, &["/prog".into()], &[], "/").unwrap()
}

impl Asm {
    /// execve(label, NULL, NULL) into x0
    fn execve(&mut self, path: &'static str) -> &mut Self {
        self.adr(0, path).movz(1, 0).movz(2, 0).svc(221)
    }
    /// execve(path, {path, arg, NULL}, envp) with the arrays built on the stack: envp is
    /// {env, NULL} when `env` is given, else empty
    fn execve_with(
        &mut self,
        path: &'static str,
        arg: &'static str,
        env: Option<&'static str>,
    ) -> &mut Self {
        self.i(0xd100_c3ff) // sub sp, sp, #48
            .adr(9, path)
            .str_sp(9, 0)
            .adr(9, arg)
            .str_sp(9, 8)
            .str_sp(31, 16);
        if let Some(env) = env {
            self.adr(9, env).str_sp(9, 24);
        } else {
            self.str_sp(31, 24);
        }
        self.str_sp(31, 32)
            .adr(0, path)
            .i(0x9100_03e1) // mov x1, sp
            .i(add_imm(2, 31, 24, false))
            .svc(221)
    }
}

/// What /prog execs in the exec tests: argc must be 2, argv[1] start with "a", envp[0] with "K";
/// descriptor 5 is open and 4 closed (the pipe's write end, dup'ed without close-on-exec and not);
/// it prints "B" and sends itself SIGUSR1.
fn exec_target() -> Vec<u8> {
    let mut a = Asm::default();
    a.i(ldr(19, 31, 0)).cmp_imm(19, 2).bcond(COND_NE, "fail");
    a.i(ldr(1, 31, 16))
        .i(ldrb(2, 1))
        .cmp_imm(2, b'a' as u32)
        .bcond(COND_NE, "fail");
    a.i(ldr(1, 31, 32))
        .i(ldrb(2, 1))
        .cmp_imm(2, b'K' as u32)
        .bcond(COND_NE, "fail");
    a.movz(21, 5)
        .write_w(21)
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    a.movz(21, 4)
        .write_w(21)
        .cmn_imm(0, 9)
        .bcond(COND_NE, "fail"); // EBADF
    a.print("B", 1);
    a.svc(172).movz(1, SIGUSR1).svc(129); // kill(getpid(), SIGUSR1)
    a.exit(0);
    a.label("fail").print("E", 1).exit(1);
    a.elf()
}

#[test]
fn exec_runs_another_program_in_the_same_process() {
    // The process forks and its child execs /child, whose blob comes only when asked for (the
    // call waits and runs again). The child's pid is still the one wait4 was given; the pipe end
    // that was close-on-exec is closed in it and the dup'ed one is not; its SIGUSR1 handler from
    // before is the default again, so the signal ends it (status 10) with no "H".
    let mut a = Asm::with_texts(vec![
        ("target", b"/child"),
        ("arg", b"arg"),
        ("env", b"K=V"),
    ]);
    a.room().sigaction(SIGUSR1, "handler", SA_RESTORER);
    a.i(add_imm(0, 31, 8, false))
        .mov32(1, 0x80000)
        .svc(59)
        .cbnz_fail(0); // pipe2(O_CLOEXEC)
    a.ldr_w(23, 31, 8).ldr_w(24, 31, 12);
    a.i(mov(0, 24)).svc(23).cmp_imm(0, 5).bcond(COND_NE, "fail"); // dup of the write end: fd 5
    a.fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.wait4(Some(20), 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(SIGUSR1 as u32);
    a.read_byte(23).cmp_imm(0, 1).bcond(COND_NE, "fail"); // what the new program wrote to fd 5
    a.ends();
    a.label("child")
        .execve_with("target", "arg", Some("env"))
        .b("fail");
    a.common();
    let prog = a.elf();
    let target = exec_target();
    let files: [(&str, u32, &[u8]); 2] = [("prog", EXE, &prog), ("child", EXE, &target)];
    let mut vm = machine_with(&files, &["prog"]);
    let mut p = exec_prog(&mut vm);
    let (out, status, asked) = run_with_files(&mut p, &mut vm, &files);
    assert_eq!(out, b"BA");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    assert_eq!(asked, ["child-blob"]);
}

#[test]
fn a_script_runs_its_interpreter_with_the_scripts_path() {
    // /script is "#!/args -v": the interpreter gets {"/args", "-v", "/script", "arg"}: it prints
    // the first byte of argv[1] and exits with argc
    let mut args = Asm::default();
    args.i(ldr(19, 31, 0))
        .i(ldr(1, 31, 16))
        .movz(0, 1)
        .movz(2, 1)
        .svc(64);
    args.i(mov(0, 19)).svc(94);
    let args = args.elf();
    let mut a = Asm::with_texts(vec![("script", b"/script"), ("arg", b"arg")]);
    a.room().fork().cbz_to(0, "child");
    a.i(mov(20, 0));
    a.wait4(Some(20), 0).cmp(0, 20).bcond(COND_NE, "fail");
    a.status_is(4 << 8).ends();
    a.label("child")
        .execve_with("script", "arg", None)
        .b("fail");
    let prog = a.elf();
    let files: [(&str, u32, &[u8]); 3] = [
        ("prog", EXE, &prog),
        ("args", EXE, &args),
        ("script", EXE, b"#!/args -v\nignored\n"),
    ];
    let mut vm = machine_with(&files, &["prog", "args", "script"]);
    let mut p = exec_prog(&mut vm);
    let (out, status, _) = run_with_files(&mut p, &mut vm, &files);
    assert_eq!(out, b"-A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_vfork_parent_goes_on_when_its_child_execs() {
    // The child writes in the shared page and execs a program that waits for input forever; the
    // parent runs again then, and the page it has is intact and has the child's write
    let mut waits = Asm::default();
    waits
        .i(0xd100_43ff)
        .i(0x9100_03e1)
        .movz(0, 0)
        .movz(2, 1)
        .svc(63)
        .exit(0); // read(0, sp, 1)
    let waits = waits.elf();
    let mut a = Asm::with_texts(vec![("wait", b"/wait")]);
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.movz(21, 1).i(strb(21, 19));
    a.clone(CLONE_VM | CLONE_VFORK | SIGCHLD as u32)
        .cbz_to(0, "child");
    a.i(mov(20, 0));
    a.i(ldrb(22, 19)).cmp_imm(22, 9).bcond(COND_NE, "fail");
    a.wait4(Some(20), WNOHANG).cbnz_fail(0); // the child lives on, as the new program
    a.ends();
    a.label("child")
        .movz(21, 9)
        .i(strb(21, 19))
        .execve("wait")
        .b("fail");
    let prog = a.elf();
    let files: [(&str, u32, &[u8]); 2] = [("prog", EXE, &prog), ("wait", EXE, &waits)];
    let mut vm = machine_with(&files, &["prog", "wait"]);
    let mut p = exec_prog(&mut vm);
    let (out, status, _) = run_with_files(&mut p, &mut vm, &files);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_failed_exec_returns_the_error_and_leaves_the_program_as_it_was() {
    let mut a = Asm::with_texts(vec![
        ("missing", b"/missing"),
        ("plain", b"/plain"),
        ("text", b"/text"),
        ("root", b"/"),
    ]);
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.movz(21, 7).i(strb(21, 19));
    a.execve("missing").cmn_imm(0, 2).bcond(COND_NE, "fail"); // ENOENT
    a.execve("plain").cmn_imm(0, 13).bcond(COND_NE, "fail"); // EACCES: not executable
    a.execve("root").cmn_imm(0, 13).bcond(COND_NE, "fail"); // EACCES: a directory
    a.execve("text").cmn_imm(0, 8).bcond(COND_NE, "fail"); // ENOEXEC: not an ELF
    a.i(ldrb(22, 19)).cmp_imm(22, 7).bcond(COND_NE, "fail");
    a.ends();
    let prog = a.elf();
    let target = exec_target();
    let files: [(&str, u32, &[u8]); 3] = [
        ("prog", EXE, &prog),
        ("plain", PLAIN, &target),
        ("text", EXE, b"hello\n"),
    ];
    let mut vm = machine_with(&files, &["prog", "plain", "text"]);
    let mut p = exec_prog(&mut vm);
    let (out, status, _) = run_with_files(&mut p, &mut vm, &files);
    assert_eq!(out, b"A");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

const LINK: u32 = 0o120777;

#[test]
fn a_symlink_to_a_device_opens_the_device_as_the_images_log_files_do() {
    // nginx opens /var/log/nginx/error.log, a link to /dev/stderr, to append to it; the device
    // is not in the filesystem, the terminal is what it names
    let mut a = Asm::with_texts(vec![("log", b"log"), ("chain", b"chain")]);
    a.room().room(); // 128 bytes: a struct stat
    for (path, fd, text) in [("log", 3, "H"), ("chain", 4, "R")] {
        a.i(0x9280_0c60) // AT_FDCWD
            .adr(1, path)
            .movz(2, 0x441) // O_WRONLY | O_CREAT | O_APPEND
            .movz(3, 0o644)
            .svc(56)
            .cmp_imm(0, fd)
            .bcond(COND_NE, "fail")
            .movz(0, fd as u16)
            .adr(1, text)
            .movz(2, 1)
            .svc(64)
            .cmp_imm(0, 1)
            .bcond(COND_NE, "fail");
    }
    // fstatat(AT_FDCWD, "chain", &st, 0): a character device, though the link is a file
    a.i(0x9280_0c60)
        .adr(1, "chain")
        .i(0x9100_03e2)
        .movz(3, 0)
        .svc(79);
    a.cbnz_fail(0);
    a.i(0xb940_13e9) // ldr w9, [sp, #16]: st_mode
        .i(0x1214_0d29) // and w9, w9, #0xf000
        .i(0x530c_7d29) // lsr w9, w9, #12
        .cmp_imm(9, 2) // S_IFCHR
        .bcond(COND_NE, "fail");
    a.ends();
    let prog = a.elf();
    let files: [(&str, u32, &[u8]); 3] = [
        ("prog", EXE, &prog),
        ("log", LINK, b"/dev/stderr"),
        ("chain", LINK, b"log"),
    ];
    let mut vm = machine_with(&files, &["prog"]);
    let mut p = exec_prog(&mut vm);
    let (out, status, _) = run_with_files(&mut p, &mut vm, &files);
    assert_eq!(out, b"HRA");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

const CLONE_SIGHAND: u32 = 0x800;
const CLONE_THREAD: u32 = 0x10000;

impl Asm {
    /// A thread that waits on the futex word at x19 + `at` and, when woken, prints "B" and ends
    /// alone: it goes on at the label "waiter", which each call that follows `spawn_waiter` can
    /// reuse with its own address in x20
    fn spawn_waiter(&mut self, at: u32) -> &mut Self {
        self.i(add_imm(20, 19, at, false))
            .clone(CLONE_VM | CLONE_SIGHAND | CLONE_THREAD)
            .cbz_to(0, "waiter")
    }
    /// The thread's body: futex(x20, FUTEX_WAIT, 0, NULL), print "B", exit(0)
    fn waiter(&mut self) -> &mut Self {
        self.label("waiter")
            .i(mov(0, 20))
            .movz(1, 0)
            .movz(2, 0)
            .movz(3, 0)
            .svc(98)
            .print("B", 1)
            .movz(0, 0)
            .svc(93)
    }
    /// futex(x19 + `at`, op, val, val2, x19 + `at2`, val3) into x0
    fn futex(&mut self, at: u32, op: u16, val: u32, val2: u32, at2: u32, val3: u32) -> &mut Self {
        self.i(add_imm(0, 19, at, false))
            .movz(1, op)
            .mov32(2, val)
            .mov32(3, val2)
            .i(add_imm(4, 19, at2, false))
            .mov32(5, val3)
            .svc(98)
    }
}

fn run_threads(a: &mut Asm) {
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"BBA", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
}

#[test]
fn a_futex_requeue_moves_waiters_to_another_word_and_wakes_the_rest_there() {
    // FUTEX_CMP_REQUEUE: of two waiters on the first word, one wakes and one moves to the
    // second; waking the first word finds nobody, waking the second finds the one it got
    let mut a = Asm::default();
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.spawn_waiter(0).spawn_waiter(0);
    a.movz(0, 0).svc(124); // sched_yield: both threads run to their waits
    a.futex(0, 4, 1, 0x7fff_ffff, 8, 0) // wake 1, requeue all the rest
        .cmp_imm(0, 2)
        .bcond(COND_NE, "fail");
    a.futex(0, 4, 1, 0, 8, 1) // the word is not 1: EAGAIN, nothing moves or wakes
        .cmn_imm(0, 11)
        .bcond(COND_NE, "fail");
    a.movz(0, 0).svc(124); // the woken thread prints "B"
    a.futex(0, 1, 10, 0, 0, 0) // nobody waits on the first word any more
        .cbnz_fail(0);
    a.futex(8, 1, 10, 0, 0, 0) // one waits on the second
        .cmp_imm(0, 1)
        .bcond(COND_NE, "fail");
    a.movz(0, 0).svc(124).ends();
    a.waiter();
    run_threads(&mut a);
}

#[test]
fn a_futex_wake_op_changes_a_word_and_wakes_by_its_old_value() {
    // FUTEX_WAKE_OP (word2 += 1, wake word2's waiter if its old value was 0): one waiter on
    // each word, both wake, and the second word holds 1
    let mut a = Asm::default();
    a.room().mmap_anon(PROT_READ | PROT_WRITE);
    a.spawn_waiter(0).spawn_waiter(8);
    a.movz(0, 0).svc(124);
    a.futex(0, 5, 1, 1, 8, 0x1000_1000) // op ADD 1, cmp EQ 0
        .cmp_imm(0, 2)
        .bcond(COND_NE, "fail");
    a.ldr_w(22, 19, 8).cmp_imm(22, 1).bcond(COND_NE, "fail");
    a.movz(0, 0).svc(124).movz(0, 0).svc(124).ends();
    a.waiter();
    run_threads(&mut a);
}

// -------------------------------------------------------------- undefined instructions

const SIGILL: u16 = 4;
/// An SVE instruction, which armless doesn't implement: OpenSSL's probe for SVE.
const SVE_PROBE: u32 = 0x04a0_3000;

#[test]
fn an_undefined_instruction_is_sigill_for_a_program_with_a_handler() {
    let mut a = Asm::default();
    a.sigaction(SIGILL, "check", SA_SIGINFO | SA_RESTORER)
        .adr(10, "ill")
        .label("ill")
        .i(SVE_PROBE)
        .print("B", 1)
        .exit(0)
        .label("fail")
        .exit(1)
        // The handler: SIGILL, ILL_ILLOPC, si_addr the instruction; it skips the instruction
        // (the pc of the ucontext is where sigreturn goes on)
        .label("check")
        .cmp_imm(0, SIGILL as u32)
        .bcond(COND_NE, "fail")
        .i(0xb940_0829) // ldr w9, [x1, #8]: si_code
        .cmp_imm(9, 1)
        .bcond(COND_NE, "fail")
        .i(0xf940_0829) // ldr x9, [x1, #16]: si_addr
        .cmp(9, 10)
        .bcond(COND_NE, "fail")
        .i(0xf940_dc49) // ldr x9, [x2, #440]: the pc
        .i(0x9100_1129) // add x9, x9, #4
        .i(0xf900_dc49) // str x9, [x2, #440]
        .b("handler")
        .common();
    let (mut vm, mut p) = start(&a.elf());
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert_eq!(out, b"HB", "{status:?}");
    assert!(matches!(status, Status::Exited(0)), "{status:?}");
    assert_eq!(p.stats.signals_delivered, 1);
}

#[test]
fn an_undefined_instruction_ends_a_program_with_no_handler_saying_where() {
    let (mut vm, mut p) = start(&elf(&[SVE_PROBE], &[]));
    let (out, status) = run(&mut p, &mut vm.cpus[0]);
    assert!(out.is_empty());
    let Status::Fault(f) = status else {
        panic!("{status:?}")
    };
    assert!(
        f.contains("exception class 0x0") && f.contains(&format!("pc {:#x}", BASE + CODE)),
        "{f}"
    );
}

// ------------------------------------------------------ headers past the first bytes

/// An executable of `ty` whose first bytes are only its ELF header and its code: its program
/// headers are at 0x20000 and the path of its loader, if it asks for one, at 0x30000, both past
/// what exec reads of a file up front. One PT_LOAD maps all of it at `vaddr`.
fn far_elf(ty: u16, vaddr: u64, code: &[u32], data: &[u8], interp: Option<&[u8]>) -> Vec<u8> {
    const PHOFF: u64 = 0x2_0000;
    const INTERP: u64 = 0x3_0000;
    let phnum = 1 + interp.is_some() as u16;
    let size = INTERP + interp.map_or(0, |p| p.len() as u64 + 1);
    let mut e = Vec::new();
    e.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    e.extend_from_slice(&ty.to_le_bytes());
    e.extend_from_slice(&183u16.to_le_bytes()); // EM_AARCH64
    e.extend_from_slice(&1u32.to_le_bytes());
    e.extend_from_slice(&(vaddr + 64).to_le_bytes()); // entry
    e.extend_from_slice(&PHOFF.to_le_bytes());
    e.extend_from_slice(&0u64.to_le_bytes()); // shoff
    e.extend_from_slice(&0u32.to_le_bytes());
    for v in [64u16, 56, phnum, 64, 0, 0] {
        e.extend_from_slice(&v.to_le_bytes());
    }
    e.extend(code.iter().flat_map(|w| w.to_le_bytes()));
    e.extend_from_slice(data);
    e.resize(PHOFF as usize, 0);
    e.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    e.extend_from_slice(&5u32.to_le_bytes()); // R X
    for v in [0, vaddr, vaddr, size, size, 0x1000] {
        e.extend_from_slice(&v.to_le_bytes());
    }
    if let Some(path) = interp {
        e.extend_from_slice(&3u32.to_le_bytes()); // PT_INTERP
        e.extend_from_slice(&4u32.to_le_bytes()); // R
        let len = path.len() as u64 + 1;
        for v in [INTERP, vaddr + INTERP, vaddr + INTERP, len, len, 1] {
            e.extend_from_slice(&v.to_le_bytes());
        }
        e.resize(INTERP as usize, 0);
        e.extend_from_slice(path);
        e.push(0);
    }
    e
}

#[test]
fn the_program_headers_and_the_loaders_path_are_read_from_wherever_they_are_in_the_file() {
    // The loader (ET_DYN) writes "hello" and exits 3; the program (ET_EXEC) would exit 9
    let loader = far_elf(
        3,
        0,
        &[
            movz(0, 1),
            adr(1, 7 * 4),
            movz(2, 6),
            movz(8, 64),
            SVC,
            movz(0, 3),
            movz(8, 94),
            SVC,
        ],
        b"hello\n",
        None,
    );
    let prog = far_elf(
        2,
        BASE,
        &[movz(0, 9), movz(8, 94), SVC],
        &[],
        Some(b"/lib/ld.so"),
    );
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
    let json = format!(
        r#"{{"version":3,"fsroot":[["prog",{},0,33261,0,0,"prog-blob"],["lib",0,0,16877,0,0,[["ld.so",{},0,33261,0,0,"ld-blob"]]]]}}"#,
        prog.len(),
        loader.len()
    );
    vm.board.load_filesystem(json.as_bytes()).unwrap();
    vm.board.provide_raw_blob("prog-blob", prog).unwrap();
    vm.board.provide_raw_blob("ld-blob", loader).unwrap();
    let mut p = exec_lazy(&mut vm);
    let (out, status, _) = run_serving(&mut p, &mut vm);
    assert_eq!(out, b"hello\r\n", "the loader the far PT_INTERP names ran");
    assert!(matches!(status, Status::Exited(3)), "{status:?}");
}

// ------------------------------------------------------------ groups, AIO, preadv2 and pwritev2

impl Asm {
    /// movk x`rd`, #imm, lsl 32
    fn movk32(&mut self, rd: u32, imm: u16) -> &mut Self {
        self.i(0xf2c0_0000 | (imm as u32) << 5 | rd)
    }
    /// ldr xt, [sp, #off]
    fn ldr_sp(&mut self, rt: u32, off: u32) -> &mut Self {
        self.i(0xf940_0000 | (off / 8) << 10 | 31 << 5 | rt)
    }
    /// sub sp, sp, #1024
    fn big_room(&mut self) -> &mut Self {
        self.i(0xd110_03ff)
    }
    /// x`rd` = sp + off
    fn sp_plus(&mut self, rd: u32, off: u32) -> &mut Self {
        self.i(add_imm(rd, 31, off, false))
    }
    /// preadv2 or pwritev2 on x19 with the iovec at [sp + iov]; no position (-1) is the
    /// descriptor's own
    fn rwv2(&mut self, write: bool, iov: u32, cnt: u16, pos: Option<u16>, flags: u16) -> &mut Self {
        self.i(mov(0, 19)).sp_plus(1, iov).movz(2, cnt);
        match pos {
            Some(p) => self.movz(3, p),
            None => self.i(0x9280_0003), // movn x3, #0: -1
        };
        self.movz(4, 0)
            .movz(5, flags)
            .svc(if write { 287 } else { 286 })
    }
    /// write(1, [sp + off], n)
    fn print_sp(&mut self, off: u32, n: u16) -> &mut Self {
        self.movz(0, 1).sp_plus(1, off).movz(2, n).svc(64)
    }
    /// The word at [sp + off] must be `v`
    fn word_is(&mut self, off: u32, v: u32) -> &mut Self {
        self.ldr_sp(10, off).cmp_imm(10, v).bcond(COND_NE, "fail")
    }
    /// x0 must be `v`
    fn ret_is(&mut self, v: u32) -> &mut Self {
        self.cmp_imm(0, v).bcond(COND_NE, "fail")
    }
    /// x0 must be -`e`
    fn ret_err(&mut self, e: u32) -> &mut Self {
        self.cmn_imm(0, e).bcond(COND_NE, "fail")
    }
    /// An iocb at [sp + 64] (buffer: the label, or the stack at 256 when None)
    fn iocb(
        &mut self,
        op: u16,
        fd: u16,
        buf: Option<&'static str>,
        n: u16,
        off: u16,
        resfd: Option<u16>,
    ) -> &mut Self {
        self.movz(9, 0x111).str_sp(9, 64).str_sp(31, 72);
        self.movz(9, op).movk32(9, fd).str_sp(9, 80);
        match buf {
            Some(l) => self.adr(9, l),
            None => self.sp_plus(9, 256),
        };
        self.str_sp(9, 88);
        self.movz(9, n).str_sp(9, 96);
        self.movz(9, off).str_sp(9, 104);
        self.str_sp(31, 112);
        match resfd {
            Some(r) => self.movz(9, 1).movk32(9, r),
            None => self.movz(9, 0),
        };
        self.str_sp(9, 120)
    }
    /// io_submit(x20, 1, [the iocb at sp + 64]) must give 1
    fn submit(&mut self) -> &mut Self {
        self.sp_plus(9, 64).str_sp(9, 8);
        self.i(mov(0, 20)).movz(1, 1).sp_plus(2, 8).svc(2).ret_is(1)
    }
    /// io_getevents(x20, 1, 1, [sp + 128], none) must give 1, and the event: its data, the
    /// iocb's address and `res`
    fn reap(&mut self, res: u32) -> &mut Self {
        self.i(mov(0, 20))
            .movz(1, 1)
            .movz(2, 1)
            .sp_plus(3, 128)
            .movz(4, 0)
            .svc(4)
            .ret_is(1);
        self.word_is(128, 0x111).word_is(144, res)
    }
    fn run_checked(&mut self, expect: &[u8]) {
        let prog = self.elf();
        let files: [(&str, u32, &[u8]); 1] = [("prog", EXE, &prog)];
        let mut vm = machine_with(&files, &["prog"]);
        let mut p = exec_prog(&mut vm);
        p.trace = std::env::var("TRACE").is_ok();
        let mut out = Vec::new();
        let mut status;
        loop {
            let (o, s, _) = run_with_files(&mut p, &mut vm, &files);
            out.extend(o);
            status = s;
            // A timed wait: let its time pass
            if !matches!(status, Status::Idle(Some(_))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(out, expect);
        assert!(matches!(status, Status::Exited(0)), "{status:?}");
    }
    /// x19 = openat(AT_FDCWD, "f", O_RDWR | O_CREAT, 0644)
    fn create_f(&mut self) -> &mut Self {
        self.i(0x9280_0c60)
            .adr(1, "f")
            .movz(2, 0x42)
            .movz(3, 0o644)
            .svc(56)
            .i(mov(19, 0))
            .cmp_imm(19, 3)
            .bcond(COND_NE, "fail")
    }
}

#[test]
fn setgroups_and_getgroups_round_trip_and_a_child_inherits_the_list() {
    let mut a = Asm::default();
    a.big_room();
    // The list {5, 7} at [sp + 64]
    a.movz(9, 5).movk32(9, 7).str_sp(9, 64);
    a.movz(0, 0).movz(1, 0).svc(158).ret_is(0);
    a.movz(0, 2).sp_plus(1, 64).svc(159).ret_is(0);
    a.movz(0, 0).movz(1, 0).svc(158).ret_is(2); // its size
    a.movz(0, 1).sp_plus(1, 80).svc(158).ret_err(22); // too small
    a.movz(0, 2).sp_plus(1, 80).svc(158).ret_is(2);
    // 5 then 7 (a word each, 4 bytes apart)
    a.ldr_w(10, 31, 80).cmp_imm(10, 5).bcond(COND_NE, "fail");
    a.ldr_w(10, 31, 84).cmp_imm(10, 7).bcond(COND_NE, "fail");
    a.mov32(0, 65537).sp_plus(1, 64).svc(159).ret_err(22); // more than NGROUPS_MAX
    a.movz(0, 1).movz(1, 0).svc(159).ret_err(14); // a bad pointer
    a.movz(0, 2).sp_plus(1, 64).svc(158); // unchanged by the failures: still 2
    a.cmp_imm(0, 2).bcond(COND_NE, "fail");
    a.fork().cbz_to(0, "child");
    a.i(mov(19, 0));
    a.wait4(Some(19), 0).status_is(0);
    a.ends();
    a.label("child")
        .movz(0, 0)
        .movz(1, 0)
        .svc(158)
        .cmp_imm(0, 2)
        .bcond(COND_NE, "fail");
    a.movz(0, 0).movz(1, 0).movz(2, 0).svc(159); // size 0 sets the list empty in the child alone
    a.movz(0, 0)
        .movz(1, 0)
        .svc(158)
        .cmp_imm(0, 0)
        .bcond(COND_NE, "fail");
    a.exit(0);
    a.run_checked(b"A");
}

#[test]
fn aio_writes_and_reads_a_file_and_signals_the_eventfd() {
    let mut a = Asm::with_texts(vec![("f", b"f"), ("hello", b"hello")]);
    a.big_room().create_f();
    // io_setup(8, [sp]) into x20
    a.str_sp(31, 0).movz(0, 8).i(0x9100_03e1).svc(0).ret_is(0);
    a.ldr_sp(20, 0);
    // eventfd2(0, 0) is fd 4
    a.movz(0, 0).movz(1, 0).svc(19).ret_is(4);
    // PWRITE "hello" at 0, then the event, with the eventfd
    a.iocb(1, 3, Some("hello"), 5, 0, Some(4)).submit().reap(5);
    // the eventfd counted the one completion
    a.movz(0, 4).sp_plus(1, 320).movz(2, 8).svc(63).ret_is(8);
    a.word_is(320, 1);
    // PREAD it back at 0, without the eventfd; print it
    a.iocb(0, 3, None, 5, 0, None).submit().reap(5);
    a.print_sp(256, 5);
    // PREAD at 3: 2 bytes left
    a.iocb(0, 3, None, 5, 3, None).submit().reap(2);
    // io_cancel finds nothing to cancel
    a.i(mov(0, 20))
        .sp_plus(1, 64)
        .sp_plus(2, 128)
        .svc(3)
        .ret_err(22);
    // nothing is queued: a zero timeout returns 0
    a.str_sp(31, 320).str_sp(31, 328);
    a.i(mov(0, 20))
        .movz(1, 1)
        .movz(2, 1)
        .sp_plus(3, 128)
        .sp_plus(4, 320)
        .svc(4)
        .ret_is(0);
    // a bad opcode, a bad fd
    a.iocb(9, 3, None, 5, 0, None);
    a.sp_plus(9, 64).str_sp(9, 8);
    a.i(mov(0, 20)).movz(1, 1).sp_plus(2, 8).svc(2).ret_err(22);
    a.iocb(0, 40, None, 5, 0, None);
    a.i(mov(0, 20)).movz(1, 1).sp_plus(2, 8).svc(2).ret_err(9);
    // io_destroy, and the context is gone
    a.i(mov(0, 20)).svc(1).ret_is(0);
    a.i(mov(0, 20)).svc(1).ret_err(22);
    a.i(mov(0, 20))
        .movz(1, 0)
        .movz(2, 1)
        .sp_plus(3, 128)
        .movz(4, 0)
        .svc(4)
        .ret_err(22);
    a.ends();
    a.run_checked(b"helloA");
}

#[test]
fn aio_getevents_with_nothing_queued_waits_its_timeout_and_returns_none() {
    let mut a = Asm::default();
    a.big_room();
    a.str_sp(31, 0).movz(0, 8).i(0x9100_03e1).svc(0).ret_is(0);
    a.ldr_sp(20, 0);
    // a 10 ms timeout: 0 events
    a.str_sp(31, 320).mov32(9, 10_000_000).str_sp(9, 328);
    a.i(mov(0, 20))
        .movz(1, 1)
        .movz(2, 1)
        .sp_plus(3, 128)
        .sp_plus(4, 320)
        .svc(4)
        .ret_is(0);
    a.ends();
    a.run_checked(b"A");
}

#[test]
fn pwritev2_and_preadv2_take_a_position_or_the_descriptors_and_their_flags() {
    let mut a = Asm::with_texts(vec![
        ("f", b"f"),
        ("abc", b"abc"),
        ("def", b"def"),
        ("XY", b"XY"),
        ("Z", b"Z"),
    ]);
    a.big_room().create_f();
    for (slot, l, n) in [(0, "abc", 3), (1, "def", 3), (2, "XY", 2), (3, "Z", 1)] {
        a.adr(9, l).str_sp(9, 64 + 16 * slot);
        a.movz(9, n).str_sp(9, 72 + 16 * slot);
    }
    // The stack buffer for reads
    a.sp_plus(9, 256).str_sp(9, 192);
    a.movz(9, 16).str_sp(9, 200);
    // "abc" "def" at the descriptor's position (0): 6 bytes
    a.rwv2(true, 64, 2, None, 0).ret_is(6);
    // "XY" at 1 leaves the position where it was (6)
    a.rwv2(true, 96, 1, Some(1), 0).ret_is(2);
    // a read at the position, the end: nothing
    a.rwv2(false, 192, 1, None, 0).ret_is(0);
    // a flag it cannot take: EOPNOTSUPP
    a.rwv2(true, 96, 1, Some(0), 0x20).ret_err(95);
    // RWF_DSYNC is as a plain write, RWF_APPEND with a position goes to the end anyway
    a.rwv2(true, 96, 1, Some(0), 2).ret_is(2);
    a.rwv2(true, 112, 1, Some(0), 0x10).ret_is(1);
    // RWF_APPEND on a read is not supported
    a.rwv2(false, 192, 1, Some(0), 0x10).ret_err(95);
    // all of it from 0: "abc" "def", "XY" at 1, "XY" at 0 (DSYNC), "Z" at the end
    a.rwv2(false, 192, 1, Some(0), 0).ret_is(7);
    a.print_sp(256, 7);
    a.ends();
    a.run_checked(b"XYYdefZA");
}
