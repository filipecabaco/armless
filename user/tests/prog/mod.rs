//! What the user-mode tests have in common: `Prog`, a program written as aarch64 machine code (a
//! few instructions a system call), and a machine to run it in.
#![allow(dead_code)]


use std::time::{Duration, Instant};

use armless::dev::Event;
use armless::vm::{Board, Config, Vm};
use armless_user::{Process, Status};

pub const BASE: u64 = 0x40_0000;
/// ELF header and one program header: the code starts right after them.
pub const CODE: u64 = 64 + 56;
/// The data starts a page in; slots for descriptors and results are below 0x100
pub const DATA: u64 = BASE + 0x1000;

/// What a system call is given in a register
#[derive(Clone, Copy)]
pub enum A {
    /// A number
    I(u64),
    /// The 32-bit value at this offset of the data
    W(u32),
    /// The address of this offset of the data
    D(u32),
    /// The 64-bit value at this offset of the data (an address a call gave)
    X(u32),
}
pub use A::{D, I, W, X};

/// A program: code and data, and the checks it makes of what its system calls return.
#[derive(Default)]
pub struct Prog {
    pub code: Vec<u32>,
    pub data: Vec<u8>,
    pub checks: u32,
}

impl Prog {
    pub fn li(&mut self, rd: u32, v: u64) {
        assert!(v < 1 << 32);
        self.code.push(0xd280_0000 | (v as u32 & 0xffff) << 5 | rd);
        if v >> 16 != 0 {
            self.code.push(0xf2a0_0000 | (v as u32 >> 16) << 5 | rd);
        }
    }

    pub fn arg(&mut self, rd: u32, a: A) {
        match a {
            I(v) => self.li(rd, v),
            D(off) => self.li(rd, DATA + off as u64),
            W(off) => {
                self.li(9, DATA);
                // ldr wd, [x9, #off]
                self.code.push(0xb940_0000 | (off / 4) << 10 | 9 << 5 | rd);
            },
            X(off) => {
                self.li(9, DATA);
                // ldr xd, [x9, #off]
                self.code.push(0xf940_0000 | (off / 8) << 10 | 9 << 5 | rd);
            },
        }
    }

    /// The system call `nr`; its result is in x0
    pub fn sys(&mut self, nr: u64, args: &[A]) -> &mut Self {
        for (i, &a) in args.iter().enumerate() {
            self.arg(i as u32, a);
        }
        self.li(8, nr);
        self.code.push(0xd400_0001);
        self
    }

    /// Keeps x0 at this offset of the data
    pub fn save(&mut self, off: u32) -> &mut Self {
        self.li(9, DATA);
        self.code.push(0xf900_0000 | (off / 8) << 10 | 9 << 5);
        self
    }

    /// Exits with 100 + the check's number unless x0 is `v`
    pub fn expect(&mut self, v: i64) -> &mut Self {
        self.checks += 1;
        let imm = v.unsigned_abs() as u32;
        assert!(imm < 4096);
        // cmp x0, #v, or cmn for a negative one
        self.code
            .push(if v < 0 { 0xb100_001f } else { 0xf100_001f } | imm << 10);
        self.code.push(0x5400_0080); // b.eq +16
        self.li(0, 100 + self.checks as u64);
        self.li(8, 94);
        self.code.push(0xd400_0001);
        self
    }

    /// write(1, data[off..off + len])
    pub fn out(&mut self, off: u32, len: u32) -> &mut Self {
        self.sys(64, &[I(1), D(off), I(len as u64)])
    }

    pub fn put(&mut self, off: u32, bytes: &[u8]) -> &mut Self {
        let end = off as usize + bytes.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[off as usize..end].copy_from_slice(bytes);
        self
    }

    /// clone(SIGCHLD), a fork: what follows is the child's, up to `parent`. Gives what `parent`
    /// needs.
    pub fn fork(&mut self) -> usize {
        self.sys(220, &[I(17), I(0), I(0), I(0), I(0)]);
        // A cbnz x0 over the child's part, once it is known
        self.code.push(0);
        self.code.len() - 1
    }

    /// The child exits (its checks passed: 0) and the parent's part begins
    pub fn parent(&mut self, at: usize) -> &mut Self {
        self.sys(94, &[I(0)]);
        let skip = (self.code.len() - at) as u32;
        self.code[at] = 0xb500_0000 | skip << 5;
        self
    }


    /// A struct msghdr at `base` of one iovec over `buf` (`len` long) and the control data `ctl`
    /// in a buffer of `cap` bytes (none: no pointer)
    pub fn msg(&mut self, base: u32, buf: u32, len: u64, ctl: &[u8], cap: u64) -> &mut Self {
        let mut h = vec![0u8; 56];
        h[16..24].copy_from_slice(&(DATA + base as u64 + 0x40).to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        if cap != 0 {
            h[32..40].copy_from_slice(&(DATA + base as u64 + 0x60).to_le_bytes());
        }
        h[40..48].copy_from_slice(&cap.to_le_bytes());
        let mut v = (DATA + buf as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&len.to_le_bytes());
        self.put(base, &h).put(base + 0x40, &v);
        let mut c = ctl.to_vec();
        c.resize(c.len().max(cap as usize), 0);
        self.put(base + 0x60, &c)
    }

    /// The 32-bit value at this offset is `v` (it goes through umask, which gives its old value)
    pub fn is(&mut self, off: u32, v: i64) -> &mut Self {
        self.sys(166, &[W(off)]);
        self.sys(166, &[I(0)]).expect(v)
    }
    /// The executable: exit_group(0) after what it does
    pub fn elf(mut self) -> Vec<u8> {
        self.sys(94, &[I(0)]);
        let mut body: Vec<u8> = self.code.iter().flat_map(|w| w.to_le_bytes()).collect();
        assert!(body.len() as u64 + CODE <= 0x1000, "code past its page");
        body.resize(0x1000 - CODE as usize, 0);
        body.extend_from_slice(&self.data);
        let filesz = CODE + body.len() as u64;
        let memsz = filesz + 0x4000;
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
            e.extend_from_slice(&v.to_le_bytes());
        }
        e.extend_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        e.extend_from_slice(&7u32.to_le_bytes()); // R W X
        for v in [0, BASE, BASE, filesz, memsz, 0x1000] {
            e.extend_from_slice(&v.to_le_bytes());
        }
        e.extend_from_slice(&body);
        e
    }
}

/// struct sockaddr_in
pub fn sa_in(ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut b = vec![2, 0];
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(&ip);
    b.extend_from_slice(&[0; 8]);
    b
}

/// struct sockaddr_un: a path (with its NUL), or an abstract name (starting with NUL)
pub fn sa_un(name: &[u8]) -> Vec<u8> {
    let mut b = vec![1, 0];
    b.extend_from_slice(name);
    if name.first() != Some(&0) {
        b.push(0);
    }
    b
}

pub fn machine() -> Vm<armless::platform::NativePlatform> {
    let config = Config {
        memory_size: 128 << 20,
        cpus: 1,
        devices: armless::dev::DeviceConfig {
            console: false,
            net: true,
            ..Default::default()
        },
    };
    armless::runner::new_vm(config).unwrap().0
}

pub fn start(vm: &mut Vm<armless::platform::NativePlatform>, p: Prog) -> Process {
    let board = Some(vm.board.clone());
    Process::exec_bytes(&mut vm.cpus[0], board, &p.elf(), &["p".into()], &[]).unwrap()
}

/// Runs until it exits or faults, with the host's part played by `host`: it is given each event
/// the machine raised (`Some`), and is called once more every round (`None`), with whether the
/// program waits for something.
pub fn drive(
    p: &mut Process,
    vm: &mut Vm<armless::platform::NativePlatform>,
    host: &mut dyn FnMut(&Board, Option<Event>, bool),
) -> (Vec<u8>, Status) {
    let board = vm.board.clone();
    let begun = Instant::now();
    let mut out = Vec::new();
    loop {
        let status = p.run(&mut vm.cpus[0], Duration::from_millis(20));
        out.extend(p.take_output());
        let idle = matches!(status, Status::Idle(_));
        for e in board.events() {
            host(&board, Some(e), idle);
        }
        host(&board, None, idle);
        match status {
            Status::Exited(_) | Status::Fault(_) => return (out, status),
            Status::Idle(_) => std::thread::sleep(Duration::from_millis(1)),
            Status::Running => {},
        }
        assert!(begun.elapsed() < Duration::from_secs(10), "still waiting");
    }
}

pub fn run(p: &mut Process, vm: &mut Vm<armless::platform::NativePlatform>) -> (Vec<u8>, Status) {
    drive(p, vm, &mut |_, _, _| {})
}

pub fn ok(status: &Status) {
    assert!(
        matches!(status, Status::Exited(0)),
        "a check failed (100 + its number) or it faulted: {status:?}"
    );
}

