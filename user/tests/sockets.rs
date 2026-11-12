//! Sockets in user mode: programs written here as aarch64 machine code (a few instructions a
//! system call, see `Prog`) talk over socketpairs, the loopback and unix names, and to a host
//! that plays the network: connections the program makes to an address out there, and
//! connections the host makes to a port the program listens on.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use armless::dev::Event;
use armless::vm::{Board, Config, Vm};
use armless_user::{Process, Status};

const BASE: u64 = 0x40_0000;
/// ELF header and one program header: the code starts right after them.
const CODE: u64 = 64 + 56;
/// The data starts a page in; slots for descriptors and results are below 0x100
const DATA: u64 = BASE + 0x1000;

/// What a system call is given in a register
#[derive(Clone, Copy)]
enum A {
    /// A number
    I(u64),
    /// The 32-bit value at this offset of the data
    W(u32),
    /// The address of this offset of the data
    D(u32),
}
use A::{D, I, W};

/// A program: code and data, and the checks it makes of what its system calls return.
#[derive(Default)]
struct Prog {
    code: Vec<u32>,
    data: Vec<u8>,
    checks: u32,
}

impl Prog {
    fn li(&mut self, rd: u32, v: u64) {
        assert!(v < 1 << 32);
        self.code.push(0xd280_0000 | (v as u32 & 0xffff) << 5 | rd);
        if v >> 16 != 0 {
            self.code.push(0xf2a0_0000 | (v as u32 >> 16) << 5 | rd);
        }
    }

    fn arg(&mut self, rd: u32, a: A) {
        match a {
            I(v) => self.li(rd, v),
            D(off) => self.li(rd, DATA + off as u64),
            W(off) => {
                self.li(9, DATA);
                // ldr wd, [x9, #off]
                self.code.push(0xb940_0000 | (off / 4) << 10 | 9 << 5 | rd);
            },
        }
    }

    /// The system call `nr`; its result is in x0
    fn sys(&mut self, nr: u64, args: &[A]) -> &mut Self {
        for (i, &a) in args.iter().enumerate() {
            self.arg(i as u32, a);
        }
        self.li(8, nr);
        self.code.push(0xd400_0001);
        self
    }

    /// Keeps x0 at this offset of the data
    fn save(&mut self, off: u32) -> &mut Self {
        self.li(9, DATA);
        self.code.push(0xf900_0000 | (off / 8) << 10 | 9 << 5);
        self
    }

    /// Exits with 100 + the check's number unless x0 is `v`
    fn expect(&mut self, v: i64) -> &mut Self {
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
    fn out(&mut self, off: u32, len: u32) -> &mut Self {
        self.sys(64, &[I(1), D(off), I(len as u64)])
    }

    fn put(&mut self, off: u32, bytes: &[u8]) -> &mut Self {
        let end = off as usize + bytes.len();
        if self.data.len() < end {
            self.data.resize(end, 0);
        }
        self.data[off as usize..end].copy_from_slice(bytes);
        self
    }

    /// clone(SIGCHLD), a fork: what follows is the child's, up to `parent`. Gives what `parent`
    /// needs.
    fn fork(&mut self) -> usize {
        self.sys(220, &[I(17), I(0), I(0), I(0), I(0)]);
        // A cbnz x0 over the child's part, once it is known
        self.code.push(0);
        self.code.len() - 1
    }

    /// The child exits (its checks passed: 0) and the parent's part begins
    fn parent(&mut self, at: usize) -> &mut Self {
        self.sys(94, &[I(0)]);
        let skip = (self.code.len() - at) as u32;
        self.code[at] = 0xb500_0000 | skip << 5;
        self
    }

    /// The executable: exit_group(0) after what it does
    fn elf(mut self) -> Vec<u8> {
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
fn sa_in(ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut b = vec![2, 0];
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(&ip);
    b.extend_from_slice(&[0; 8]);
    b
}

/// struct sockaddr_un: a path (with its NUL), or an abstract name (starting with NUL)
fn sa_un(name: &[u8]) -> Vec<u8> {
    let mut b = vec![1, 0];
    b.extend_from_slice(name);
    if name.first() != Some(&0) {
        b.push(0);
    }
    b
}

fn machine() -> Vm<armless::platform::NativePlatform> {
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

fn start(vm: &mut Vm<armless::platform::NativePlatform>, p: Prog) -> Process {
    let board = Some(vm.board.clone());
    Process::exec_bytes(&mut vm.cpus[0], board, &p.elf(), &["p".into()], &[]).unwrap()
}

/// Runs until it exits or faults, with the host's part played by `host`: it is given each event
/// the machine raised (`Some`), and is called once more every round (`None`), with whether the
/// program waits for something.
fn drive(
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

fn run(p: &mut Process, vm: &mut Vm<armless::platform::NativePlatform>) -> (Vec<u8>, Status) {
    drive(p, vm, &mut |_, _, _| {})
}

fn ok(status: &Status) {
    assert!(
        matches!(status, Status::Exited(0)),
        "a check failed (100 + its number) or it faulted: {status:?}"
    );
}

#[test]
fn a_socketpair_carries_bytes_both_ways_and_messages() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"ping").put(0x210, b"pong");
    // sendmsg and recvmsg: a msghdr of one iovec each
    p.put(0x300, b"msg");
    let hdr = |iov: u32| {
        let mut h = vec![0u8; 56];
        h[16..24].copy_from_slice(&(DATA + iov as u64).to_le_bytes());
        h[24..32].copy_from_slice(&1u64.to_le_bytes());
        h
    };
    let iov = |buf: u32, len: u64| {
        let mut v = (DATA + buf as u64).to_le_bytes().to_vec();
        v.extend_from_slice(&len.to_le_bytes());
        v
    };
    p.put(0x500, &hdr(0x560)).put(0x560, &iov(0x300, 3));
    p.put(0x580, &hdr(0x5e0)).put(0x5e0, &iov(0x600, 16));
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0); // fds 3 and 4
    p.sys(64, &[W(0x100), D(0x200), I(4)]).expect(4);
    p.sys(63, &[W(0x104), D(0x300 + 0x80), I(16)]).expect(4);
    p.out(0x300 + 0x80, 4);
    p.sys(64, &[W(0x104), D(0x210), I(4)]).expect(4);
    p.sys(63, &[W(0x100), D(0x300 + 0x80), I(16)]).expect(4);
    p.out(0x300 + 0x80, 4);
    p.sys(211, &[W(0x100), D(0x500), I(0)]).expect(3);
    p.sys(212, &[W(0x104), D(0x580), I(0)]).expect(3);
    p.out(0x600, 3);
    // A closed end: the other reads what was sent, then the end of the stream
    p.sys(64, &[W(0x100), D(0x200), I(2)]).expect(2);
    p.sys(57, &[W(0x100)]).expect(0);
    p.sys(63, &[W(0x104), D(0x300 + 0x80), I(16)]).expect(2);
    p.sys(63, &[W(0x104), D(0x300 + 0x80), I(16)]).expect(0);
    p.sys(64, &[W(0x104), D(0x200), I(2)]).expect(-32); // EPIPE
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"pingpongmsg");
}

#[test]
fn a_stream_connects_over_the_loopback() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([127, 0, 0, 1], 8080));
    p.put(0x220, &sa_in([127, 0, 0, 1], 8081));
    p.put(0x240, b"hello").put(0x250, b"world");
    p.put(0x420, &1u32.to_le_bytes());
    p.put(0x410, &4u32.to_le_bytes());
    p.put(0x430, &16u32.to_le_bytes());
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(208, &[W(0x100), I(1), I(2), D(0x420), I(4)])
        .expect(0); // SO_REUSEADDR
    p.sys(209, &[W(0x100), I(1), I(3), D(0x400), D(0x410)])
        .expect(0); // SO_TYPE
    p.out(0x400, 4);
    p.sys(200, &[W(0x100), D(0x200), I(16)]).expect(0);
    p.sys(201, &[W(0x100), I(5)]).expect(0);
    p.sys(198, &[I(2), I(1), I(0)]).expect(4).save(0x108);
    // Nobody listens on 8081
    p.sys(203, &[W(0x108), D(0x220), I(16)]).expect(-111);
    p.sys(57, &[W(0x108)]).expect(0);
    p.sys(198, &[I(2), I(1), I(0)]).expect(4).save(0x108);
    p.sys(203, &[W(0x108), D(0x200), I(16)]).expect(0);
    p.sys(202, &[W(0x100), I(0), I(0)]).expect(5).save(0x110);
    p.sys(64, &[W(0x108), D(0x240), I(5)]).expect(5);
    p.sys(63, &[W(0x110), D(0x300), I(16)]).expect(5);
    p.out(0x300, 5);
    p.sys(64, &[W(0x110), D(0x250), I(5)]).expect(5);
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(5);
    p.out(0x300, 5);
    p.sys(204, &[W(0x110), D(0x440), D(0x430)]).expect(0); // the accepted end's own name
    p.out(0x440, 8);
    // A half close: the other end reads to its end while it can still write
    p.sys(210, &[W(0x108), I(1)]).expect(0);
    p.sys(63, &[W(0x110), D(0x300), I(16)]).expect(0);
    p.sys(64, &[W(0x110), D(0x250), I(5)]).expect(5);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    let mut want = b"\x01\0\0\0helloworld".to_vec();
    want.extend_from_slice(&sa_in([127, 0, 0, 1], 8080)[..8]);
    assert_eq!(out, want);
}

#[test]
fn unix_sockets_have_path_and_abstract_names() {
    let mut vm = machine();
    let mut p = Prog::default();
    let path = sa_un(b"/tmp/s");
    let abs = sa_un(b"\0dg");
    p.put(0x200, &path)
        .put(0x230, &abs)
        .put(0x260, &sa_un(b"/tmp/none"));
    p.put(0x300, b"abc");
    p.sys(198, &[I(1), I(1), I(0)]).expect(3).save(0x100);
    p.sys(200, &[W(0x100), D(0x200), I(path.len() as u64)])
        .expect(0);
    p.sys(201, &[W(0x100), I(1)]).expect(0);
    p.sys(198, &[I(1), I(1), I(0)]).expect(4).save(0x108);
    p.sys(203, &[W(0x108), D(0x260), I(11)]).expect(-111);
    p.sys(203, &[W(0x108), D(0x200), I(path.len() as u64)])
        .expect(0);
    p.sys(202, &[W(0x100), I(0), I(0)]).expect(5).save(0x110);
    p.sys(64, &[W(0x108), D(0x300), I(3)]).expect(3);
    p.sys(63, &[W(0x110), D(0x340), I(16)]).expect(3);
    p.out(0x340, 3);
    // A name in use
    p.sys(198, &[I(1), I(1), I(0)]).expect(6).save(0x118);
    p.sys(200, &[W(0x118), D(0x200), I(path.len() as u64)])
        .expect(-98);
    // Datagrams to an abstract name
    p.sys(198, &[I(1), I(2), I(0)]).expect(7).save(0x120);
    p.sys(200, &[W(0x120), D(0x230), I(abs.len() as u64)])
        .expect(0);
    p.sys(198, &[I(1), I(2), I(0)]).expect(8).save(0x128);
    p.sys(
        206,
        &[
            W(0x128),
            D(0x300),
            I(3),
            I(0),
            D(0x230),
            I(abs.len() as u64),
        ],
    )
    .expect(3);
    p.sys(207, &[W(0x120), D(0x340), I(16), I(0), I(0), I(0)])
        .expect(3);
    p.out(0x340, 3);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"abcabc");
}

#[test]
fn datagrams_over_udp_on_the_loopback() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([127, 0, 0, 1], 9000))
        .put(0x300, b"dgram");
    p.put(0x430, &16u32.to_le_bytes());
    p.sys(198, &[I(2), I(2), I(0)]).expect(3).save(0x100);
    p.sys(200, &[W(0x100), D(0x200), I(16)]).expect(0);
    p.sys(198, &[I(2), I(2), I(0)]).expect(4).save(0x108);
    p.sys(206, &[W(0x108), D(0x300), I(5), I(0), D(0x200), I(16)])
        .expect(5);
    // Cut to the room it has, the rest of the datagram lost
    p.sys(207, &[W(0x100), D(0x340), I(3), I(0), D(0x440), D(0x430)])
        .expect(3);
    p.out(0x340, 3);
    p.out(0x440, 4); // from AF_INET, an ephemeral port
    p.sys(207, &[W(0x100), D(0x340), I(3), I(0x40), I(0), I(0)])
        .expect(-11);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(&out[..3], b"dgr");
    assert_eq!(&out[3..5], &[2, 0], "AF_INET");
    assert!(
        u16::from_be_bytes([out[5], out[6]]) >= 49152,
        "an ephemeral port"
    );
}

#[test]
fn nonblocking_sockets_fail_with_eagain_and_report_readiness() {
    let mut vm = machine();
    let mut p = Prog::default();
    // struct pollfd: fd 3, POLLIN
    let mut pfd = 3u32.to_le_bytes().to_vec();
    pfd.extend_from_slice(&[1, 0, 0, 0]);
    p.put(0x200, &pfd).put(0x240, b"z");
    // struct epoll_event: EPOLLIN, and a datum
    let mut ev = 1u32.to_le_bytes().to_vec();
    ev.extend_from_slice(&[0; 4]);
    ev.extend_from_slice(&0x77u64.to_le_bytes());
    p.put(0x230, &ev);
    p.sys(199, &[I(1), I(1 | 0x800), I(0), D(0x100)]).expect(0); // fds 3 and 4
    p.sys(63, &[W(0x100), D(0x300), I(8)]).expect(-11);
    p.sys(73, &[D(0x200), I(1), D(0x220), I(0), I(8)]).expect(0); // ppoll, not ready
    p.sys(64, &[W(0x104), D(0x240), I(1)]).expect(1);
    p.sys(73, &[D(0x200), I(1), D(0x220), I(0), I(8)]).expect(1);
    p.out(0x206, 2); // revents
    p.sys(20, &[I(0)]).expect(5).save(0x108); // epoll_create1
    p.sys(21, &[W(0x108), I(1), W(0x100), D(0x230)]).expect(0);
    p.sys(22, &[W(0x108), D(0x400), I(4), I(0), I(0), I(8)])
        .expect(1);
    p.out(0x400, 16);
    p.sys(63, &[W(0x100), D(0x300), I(8)]).expect(1);
    p.sys(63, &[W(0x100), D(0x300), I(8)]).expect(-11);
    p.sys(22, &[W(0x108), D(0x400), I(4), I(0), I(0), I(8)])
        .expect(0);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    let mut want = vec![1, 0];
    want.extend_from_slice(&1u32.to_le_bytes());
    want.extend_from_slice(&[0; 4]);
    want.extend_from_slice(&0x77u64.to_le_bytes());
    assert_eq!(out, want);
}

#[test]
fn a_socket_takes_async_io_and_an_owner_though_no_sigio_comes() {
    // nginx makes the channel between its processes asynchronous and owned before it forks a
    // worker, and a failure of either ends the master
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x300, &1u32.to_le_bytes());
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0); // fds 3 and 4
    p.sys(29, &[W(0x100), I(0x5452), D(0x300)]).expect(0); // ioctl(FIOASYNC) on
    p.sys(25, &[W(0x100), I(8), I(1)]).expect(0); // F_SETOWN
    p.sys(25, &[W(0x100), I(3), I(0)]).save(0x110); // F_GETFL
    p.out(0x110, 2);
    p.sys(29, &[W(0x100), I(0x5452), D(0x304)]).expect(0); // off
    p.sys(25, &[W(0x100), I(3), I(0)]).save(0x110);
    p.out(0x110, 2);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, [2, 0x20, 2, 0], "O_RDWR, with O_ASYNC while it is on");
}

#[test]
fn gettimeofday_gives_the_wall_clock_in_microseconds_and_a_zero_timezone() {
    // V8's Date.now() is gettimeofday, and when it fails the date is whatever was in the stack
    let mut vm = machine();
    let mut p = Prog::default();
    // The output is a terminal: without raw output (c_oflag 0) it turns a 0x0a in the time into
    // 0x0d 0x0a, and the test fails one run in ten
    p.put(0x120, &[0xff; 8]).put(0x140, &[0; 36]);
    p.sys(29, &[I(1), I(0x5402), D(0x140)]).expect(0);
    p.sys(169, &[D(0x100), D(0x120)]).expect(0);
    p.out(0x100, 16);
    p.out(0x120, 8);
    p.sys(169, &[I(0), I(0)]).expect(0);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    let sec = u64::from_le_bytes(out[..8].try_into().unwrap());
    let usec = u64::from_le_bytes(out[8..16].try_into().unwrap());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!(sec.abs_diff(now) <= 5, "{sec} is not near {now}");
    assert!(usec < 1_000_000, "{usec}");
    assert_eq!(out[16..], [0; 8], "the timezone is zero");
}

#[test]
fn an_edge_triggered_watch_reports_each_write_to_an_eventfd_that_nobody_reads() {
    let mut vm = machine();
    let mut p = Prog::default();
    // struct epoll_event: EPOLLIN | EPOLLET, and a datum
    let mut ev = (1u32 | 1 << 31).to_le_bytes().to_vec();
    ev.extend_from_slice(&[0; 4]);
    ev.extend_from_slice(&0x77u64.to_le_bytes());
    p.put(0x230, &ev).put(0x240, &1u64.to_le_bytes());
    p.sys(19, &[I(0), I(0x800)]).expect(3); // eventfd2, nonblocking
    p.sys(20, &[I(0)]).expect(4).save(0x108); // epoll_create1
    p.sys(21, &[W(0x108), I(1), I(3), D(0x230)]).expect(0);
    let wait = |p: &mut Prog, n: i64| {
        p.sys(22, &[W(0x108), D(0x400), I(4), I(0), I(0), I(8)])
            .expect(n);
    };
    wait(&mut p, 0);
    p.sys(64, &[I(3), D(0x240), I(8)]).expect(8);
    wait(&mut p, 1);
    wait(&mut p, 0); // nothing new: an edge reports once
    p.sys(64, &[I(3), D(0x240), I(8)]).expect(8);
    wait(&mut p, 1); // the second write, though the first was never read
    wait(&mut p, 0);
    // Read to empty and written again between two waits: still one edge
    p.sys(63, &[I(3), D(0x300), I(8)]).expect(8);
    p.sys(64, &[I(3), D(0x240), I(8)]).expect(8);
    wait(&mut p, 1);
    wait(&mut p, 0);
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn a_connection_to_an_address_out_there_goes_to_the_host() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([93, 184, 216, 34], 80))
        .put(0x240, b"GET");
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(203, &[W(0x100), D(0x200), I(16)]).expect(0); // blocks until the host accepts
    p.sys(64, &[W(0x100), D(0x240), I(3)]).expect(3);
    p.sys(63, &[W(0x100), D(0x300), I(16)]).expect(4); // blocks until the host answers
    p.out(0x300, 4);
    p.sys(63, &[W(0x100), D(0x300), I(16)]).expect(0); // the host's close
    let mut p = start(&mut vm, p);
    let seen = RefCell::new(Vec::new());
    let (out, status) = drive(&mut p, &mut vm, &mut |b, e, _| match e {
        Some(Event::TcpOpen { id, host, port }) => {
            seen.borrow_mut().push(format!("open {host}:{port}"));
            b.tcp_accept(id);
        },
        Some(Event::TcpData { id, data }) => {
            seen.borrow_mut()
                .push(format!("data {}", String::from_utf8_lossy(&data)));
            b.tcp_send(id, b"pong");
            b.tcp_close(id);
        },
        _ => {},
    });
    ok(&status);
    assert_eq!(out, b"pong");
    assert_eq!(*seen.borrow(), ["open 93.184.216.34:80", "data GET"]);
}

#[test]
fn a_refused_connection_to_the_host_fails_the_connect() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([203, 0, 113, 9], 25));
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(203, &[W(0x100), D(0x200), I(16)]).expect(-111);
    let mut p = start(&mut vm, p);
    let (_, status) = drive(&mut p, &mut vm, &mut |b, e, _| {
        if let Some(Event::TcpOpen { id, .. }) = e {
            b.tcp_refuse(id);
        }
    });
    ok(&status);
}

#[test]
fn the_host_connects_to_a_port_the_program_listens_on() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([0, 0, 0, 0], 7000)).put(0x240, b"yo");
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(200, &[W(0x100), D(0x200), I(16)]).expect(0);
    p.sys(201, &[W(0x100), I(5)]).expect(0);
    p.sys(202, &[W(0x100), I(0), I(0)]).expect(4).save(0x108); // blocks until the host connects
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(2);
    p.out(0x300, 2);
    p.sys(64, &[W(0x108), D(0x240), I(2)]).expect(2);
    p.sys(63, &[W(0x108), D(0x300), I(16)]).expect(0); // the host's close
    let mut p = start(&mut vm, p);
    let (mut conn, mut got) = (None, Vec::new());
    let (out, status) = drive(&mut p, &mut vm, &mut |b, e, idle| match e {
        None if idle && conn.is_none() => {
            // The program waits in accept: it listens
            conn = Some(b.tcp_connect(7000).expect("the machine has a network"));
        },
        Some(Event::TcpConnected { id }) => b.tcp_send(id, b"hi"),
        Some(Event::TcpData { id, data }) => {
            got.extend(data);
            b.tcp_close(id);
        },
        _ => {},
    });
    ok(&status);
    assert_eq!(out, b"hi");
    assert_eq!(got, b"yo");
}

#[test]
fn a_port_is_listened_on_between_the_listen_and_the_exit() {
    // What a published port waits for: the host opens its side only once this says yes
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, &sa_in([0, 0, 0, 0], 7001));
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(200, &[W(0x100), D(0x200), I(16)]).expect(0);
    p.sys(201, &[W(0x100), I(5)]).expect(0);
    p.sys(202, &[W(0x100), I(0), I(0)]).expect(4).save(0x108); // blocks until the host connects
    let mut p = start(&mut vm, p);
    assert!(!vm.board.local_listening(7001), "nothing has run yet");
    let (mut conn, mut seen) = (None, false);
    let (_, status) = drive(&mut p, &mut vm, &mut |b, e, idle| match e {
        None if idle && conn.is_none() => {
            seen = b.local_listening(7001);
            conn = Some(b.tcp_connect(7001).unwrap());
        },
        _ => {},
    });
    ok(&status);
    assert!(seen, "the program waits in accept: it listens");
    assert!(!vm.board.local_listening(7001), "and stops when it exits");
}

#[test]
fn a_name_is_answered_by_the_networks_resolver() {
    let mut vm = machine();
    let mut p = Prog::default();
    // example.com, type A, class IN
    let mut q = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    q.extend_from_slice(b"\x07example\x03com\0\0\x01\0\x01");
    p.put(0x200, &sa_in([192, 168, 86, 1], 53)).put(0x240, &q);
    p.sys(198, &[I(2), I(2), I(0)]).expect(3).save(0x100);
    p.sys(
        206,
        &[W(0x100), D(0x240), I(q.len() as u64), I(0), D(0x200), I(16)],
    )
    .expect(q.len() as i64);
    p.sys(207, &[W(0x100), D(0x400), I(512), I(0), I(0), I(0)])
        .save(0x108);
    p.sys(64, &[I(1), D(0x400), W(0x108)]);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(&out[..2], [0x12, 0x34]);
    assert_eq!(
        out[out.len() - 4],
        10,
        "a name resolves to the network's own address"
    );
}

#[test]
fn sockets_survive_a_snapshot() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"kept");
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0);
    p.sys(64, &[W(0x100), D(0x200), I(4)]).expect(4);
    p.sys(63, &[I(0), D(0x300), I(1)]).expect(1); // waits for a key
    p.sys(63, &[W(0x104), D(0x340), I(16)]).expect(4);
    p.out(0x340, 4);
    let mut p = start(&mut vm, p);
    let status = loop {
        let status = p.run(&mut vm.cpus[0], Duration::from_millis(20));
        if !matches!(status, Status::Running) {
            break status;
        }
    };
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], Some(vm.board.clone()), &process).unwrap();
    p.input(b"\n");
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert!(out.ends_with(b"kept"), "{out:?}");
}

/// The program of the fork tests: a listener and a connected pair, then a fork. The child
/// accepts on the listener and answers over the pair's end; the parent connects, sends "ping" and
/// reads what comes back, then the end of the stream when the child has ended (the child's
/// descriptors close with it, and the parent closed its own).
fn forked_sockets(wait_for_a_key: bool) -> Prog {
    let mut p = Prog::default();
    p.put(0x200, &sa_in([127, 0, 0, 1], 8080))
        .put(0x240, b"ping");
    p.sys(198, &[I(2), I(1), I(0)]).expect(3).save(0x100);
    p.sys(200, &[W(0x100), D(0x200), I(16)]).expect(0);
    p.sys(201, &[W(0x100), I(5)]).expect(0);
    p.sys(199, &[I(1), I(1), I(0), D(0x108)]).expect(0); // fds 4 and 5
    let child = p.fork();
    // The child accepts the connection on the listener it inherited and sends what it reads
    // over the pair's other end, which it inherited too
    p.sys(202, &[I(3), I(0), I(0)]).expect(6);
    p.sys(63, &[I(6), D(0x300), I(16)]).expect(4);
    p.sys(64, &[I(5), D(0x300), I(4)]).expect(4);
    p.parent(child);
    p.sys(57, &[I(5)]).expect(0);
    if wait_for_a_key {
        p.sys(63, &[I(0), D(0x380), I(1)]).expect(1);
    }
    p.sys(198, &[I(2), I(1), I(0)]).expect(5);
    p.sys(203, &[I(5), D(0x200), I(16)]).expect(0);
    p.sys(64, &[I(5), D(0x240), I(4)]).expect(4);
    p.sys(63, &[I(4), D(0x340), I(16)]).expect(4);
    p.out(0x340, 4);
    p.sys(63, &[I(4), D(0x340), I(16)]).expect(0);
    // The child ended with a status of 0
    p.sys(260, &[I(0xffff_ffff), D(0x180), I(0), I(0)]);
    p.out(0x180, 4);
    p
}

#[test]
fn a_forked_child_has_the_sockets_of_its_parent_and_the_parent_sees_what_it_writes() {
    let mut vm = machine();
    let mut p = start(&mut vm, forked_sockets(false));
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"ping\0\0\0\0");
}

#[test]
fn processes_with_sockets_survive_a_snapshot() {
    // The child waits to accept and the parent for a key; the machine is saved there and goes on
    // in another one
    let mut vm = machine();
    let mut p = start(&mut vm, forked_sockets(true));
    let status = loop {
        let status = p.run(&mut vm.cpus[0], Duration::from_millis(20));
        if !matches!(status, Status::Running) {
            break status;
        }
    };
    assert!(matches!(status, Status::Idle(None)), "{status:?}");
    let process = p.save(&vm.cpus[0]);
    let state = vm.save_state();
    drop((p, vm));

    let mut vm = machine();
    vm.load_state(&state).unwrap();
    let mut p = Process::restore(&mut vm.cpus[0], Some(vm.board.clone()), &process).unwrap();
    p.input(b"\n");
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert!(out.ends_with(b"ping\0\0\0\0"), "{out:?}");
}

#[test]
fn what_is_not_supported_says_so() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.sys(198, &[I(99), I(1), I(0)]).expect(-97); // an unknown family
    p.sys(198, &[I(2), I(5), I(0)]).expect(-94); // SOCK_SEQPACKET
    p.sys(199, &[I(2), I(1), I(0), D(0x100)]).expect(-95); // socketpair of AF_INET
    p.sys(210, &[I(1), I(0)]).expect(-88); // shutdown of a descriptor that is not a socket
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}
