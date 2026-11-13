//! Sockets in user mode: programs written here as aarch64 machine code (a few instructions a
//! system call, see `Prog`) talk over socketpairs, the loopback and unix names, and to a host
//! that plays the network: connections the program makes to an address out there, and
//! connections the host makes to a port the program listens on.

mod prog;

use std::cell::RefCell;
use std::time::Duration;

use armless::dev::Event;
use armless_user::{Process, Status};
use prog::*;

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

/// struct msghdr of one iovec (at `iov`) and a control buffer (at `ctl`, `ctllen` bytes)
fn msghdr(iov: u32, ctl: u32, ctllen: u64) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    h[16..24].copy_from_slice(&(DATA + iov as u64).to_le_bytes());
    h[24..32].copy_from_slice(&1u64.to_le_bytes());
    if ctllen > 0 {
        h[32..40].copy_from_slice(&(DATA + ctl as u64).to_le_bytes());
        h[40..48].copy_from_slice(&ctllen.to_le_bytes());
    }
    h
}

fn iovec(buf: u32, len: u64) -> Vec<u8> {
    let mut v = (DATA + buf as u64).to_le_bytes().to_vec();
    v.extend_from_slice(&len.to_le_bytes());
    v
}

#[test]
fn a_descriptor_passes_over_a_unix_stream_with_scm_rights() {
    // Erlang hands erl_child_setup the pipes of the programs it starts this way
    let mut p = Prog::default();
    p.put(0x200, b"hello").put(0x210, b"x");
    // The message: one byte, and an SCM_RIGHTS of descriptor 5 (CMSG_LEN 20, CMSG_SPACE 24)
    let mut cmsg = 20u64.to_le_bytes().to_vec();
    cmsg.extend_from_slice(&1u32.to_le_bytes());
    cmsg.extend_from_slice(&1u32.to_le_bytes());
    cmsg.extend_from_slice(&5u32.to_le_bytes());
    p.put(0x600, &cmsg);
    p.put(0x400, &msghdr(0x480, 0x600, 24)).put(0x480, &iovec(0x210, 1));
    p.put(0x500, &msghdr(0x580, 0x700, 64)).put(0x580, &iovec(0x800, 16));
    p.sys(199, &[I(1), I(1), I(0), D(0x100)]).expect(0); // socketpair: 3, 4
    p.sys(59, &[D(0x108), I(0)]).expect(0); // pipe2: 5, 6
    p.sys(64, &[I(6), D(0x200), I(5)]).expect(5);
    p.sys(211, &[I(3), D(0x400), I(0)]).expect(1);
    // Only the copy on its way holds the pipe's read end now; the write end goes
    p.sys(57, &[I(5)]).expect(0);
    p.sys(57, &[I(6)]).expect(0);
    p.sys(212, &[I(4), D(0x500), I(0)]).expect(1);
    // One SCM_RIGHTS message of one descriptor: the lowest free, 5
    p.arg(0, X(0x500 + 40)); // msg_controllen: CMSG_SPACE(4)
    p.expect(24);
    p.arg(0, X(0x700)); // cmsg_len
    p.expect(20);
    p.arg(0, W(0x700 + 16));
    p.expect(5);
    // The pipe it is: what was written, then its end
    p.sys(63, &[W(0x700 + 16), D(0x900), I(16)]).expect(5);
    p.out(0x900, 5);
    p.sys(63, &[W(0x700 + 16), D(0x900), I(16)]).expect(0);
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (out, status) = run(&mut proc, &mut vm);
    ok(&status);
    assert_eq!(out, b"hello");
}

#[test]
fn an_unbound_unix_client_is_accepted_as_its_family_alone() {
    // Postgres matches a connection to pg_hba.conf's "local" lines by the address's family
    let mut p = Prog::default();
    p.put(0x200, &sa_un(b"/tmp/s"));
    p.put(0x300, &128u32.to_le_bytes());
    p.sys(198, &[I(1), I(1), I(0)]).expect(3); // socket(AF_UNIX, SOCK_STREAM)
    p.sys(200, &[I(3), D(0x200), I(9)]).expect(0);
    p.sys(201, &[I(3), I(4)]).expect(0);
    p.sys(198, &[I(1), I(1), I(0)]).expect(4);
    p.sys(203, &[I(4), D(0x200), I(9)]).expect(0);
    p.sys(202, &[I(3), D(0x280), D(0x300)]).expect(5); // accept
    p.arg(0, W(0x300));
    p.expect(2); // just sun_family
    p.arg(0, W(0x280));
    p.expect(1); // AF_UNIX
    let mut vm = machine();
    let mut proc = start(&mut vm, p);
    let (_, status) = run(&mut proc, &mut vm);
    ok(&status);
}

#[test]
fn a_netlink_route_socket_lists_the_interfaces_and_their_addresses() {
    // ip addr, getifaddrs, Node's os.networkInterfaces(): an RTM_GETADDR dump
    let mut vm = machine();
    let mut p = Prog::default();
    // struct nlmsghdr (len 24, RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, seq 1) + ifaddrmsg (AF_INET)
    let mut req = 24u32.to_le_bytes().to_vec();
    req.extend_from_slice(&22u16.to_le_bytes());
    req.extend_from_slice(&0x301u16.to_le_bytes());
    req.extend_from_slice(&1u32.to_le_bytes());
    req.extend_from_slice(&0u32.to_le_bytes());
    req.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 0]);
    p.put(0x200, &req);
    // struct ifreq for "lo"
    p.put(0x300, b"lo\0");
    p.sys(198, &[I(16), I(3), I(0)]).expect(3); // socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE)
    p.sys(206, &[I(3), D(0x200), I(24), I(0), I(0), I(0)]).expect(24);
    p.sys(207, &[I(3), D(0x400), I(2048), I(0), I(0), I(0)]).save(0x100);
    // The first message is an RTM_NEWADDR (its type, after the 4-byte length)
    p.arg(0, W(0x404));
    p.code.push(0x9240_3c00); // and x0, x0, #0xffff
    p.expect(20);
    // SIOCGIFINDEX of lo on any socket: 1
    p.sys(29, &[I(3), I(0x8933), D(0x300)]).expect(0);
    p.arg(0, W(0x310));
    p.expect(1);
    p.sys(29, &[I(3), I(0x8933), D(0x340)]).expect(-19); // ENODEV: no such interface (an empty name)
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

// ------------------------------------------------ descriptors and credentials over unix sockets

/// A cmsghdr of SOL_SOCKET with its data, padded
fn cmsg(ty: u32, data: &[u8]) -> Vec<u8> {
    let mut b = ((16 + data.len()) as u64).to_le_bytes().to_vec();
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&ty.to_le_bytes());
    b.extend_from_slice(data);
    b.resize(b.len().next_multiple_of(8), 0);
    b
}

/// SCM_RIGHTS of descriptors
fn rights(fds: &[u32]) -> Vec<u8> {
    cmsg(
        1,
        &fds.iter()
            .flat_map(|f| f.to_le_bytes())
            .collect::<Vec<u8>>(),
    )
}

const MSG_PEEK: u64 = 2;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;

/// Offsets for the descriptor tests: where a message's header (it takes 0x80), its data and the
/// control data that comes back are
const M1: u32 = 0x500;
const M2: u32 = 0x600;
const M3: u32 = 0x700;
const CTL: u32 = 0x60;

/// A pipe (fds 3 and 4) and a socketpair of `ty` (5 and 6)
fn pipe_and_pair(p: &mut Prog, ty: u64) {
    p.sys(59, &[D(0x100), I(0)]).expect(0);
    p.sys(199, &[I(1), I(ty), I(0), D(0x108)]).expect(0);
}

#[test]
fn a_pipe_passed_over_a_socketpair_is_read_by_a_forked_child_through_it() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x").put(0x210, b"pipe");
    pipe_and_pair(&mut p, 1);
    p.msg(M1, 0x200, 1, &rights(&[3]), 24);
    p.msg(M2, 0x220, 1, &[], 24);
    let child = p.fork();
    // The child receives the read end: it is a descriptor of its own, past the ones it has
    p.sys(212, &[I(6), D(M2), I(0)]).expect(1);
    p.is(M2 + 40, 24);
    p.sys(63, &[W(M2 + CTL + 16), D(0x300), I(16)]).expect(4);
    p.out(0x300, 4);
    p.parent(child);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    // The read end is closed here: it lives on in the child, and what is written reaches it
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x210), I(4)]).expect(4);
    p.sys(260, &[I(0xffff_ffff), D(0x180), I(0), I(0)]);
    p.is(0x180, 0);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"pipe");
}

#[test]
fn received_descriptors_are_close_on_exec_when_asked_and_a_short_buffer_truncates() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x");
    pipe_and_pair(&mut p, 1);
    // Both ends, with MSG_CMSG_CLOEXEC: fds 7 and 8, flagged, the control data 16 + 8 bytes
    p.msg(M1, 0x200, 1, &rights(&[3, 4]), 32);
    p.msg(M2, 0x210, 1, &[], 32);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(M2), I(MSG_CMSG_CLOEXEC)]).expect(1);
    p.is(M2 + 40, 24).is(M2 + 48, 0).is(M2 + CTL + 12, 1);
    p.sys(25, &[W(M2 + CTL + 16), I(1)]).expect(1);
    p.sys(25, &[W(M2 + CTL + 20), I(1)]).expect(1);
    // Without it they are not: the lowest free descriptors, 9 and 10
    p.msg(M3, 0x220, 1, &[], 32);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(M3), I(0)]).expect(1);
    p.sys(25, &[W(M3 + CTL + 16), I(1)]).expect(0);
    p.sys(25, &[W(M3 + CTL + 20), I(1)]).expect(0);
    // Room for one descriptor of two: it is installed (11), the other closed, MSG_CTRUNC
    p.msg(0x800, 0x230, 1, &[], 20);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(0x800), I(0)]).expect(1);
    p.is(0x800 + 40, 20).is(0x800 + 48, 8);
    p.sys(25, &[W(0x800 + CTL + 16), I(1)]).expect(0);
    p.sys(25, &[I(12), I(1)]).expect(-9);
    // No control buffer at all: nothing is installed
    p.msg(0x880, 0x240, 1, &[], 0);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(0x880), I(0)]).expect(1);
    p.is(0x880 + 40, 0).is(0x880 + 48, 8);
    p.sys(25, &[I(12), I(1)]).expect(-9);
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}


#[test]
fn a_description_in_flight_keeps_its_pipe_open_until_it_is_received_or_its_socket_closes() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x");
    pipe_and_pair(&mut p, 1);
    p.msg(M1, 0x200, 1, &rights(&[3]), 24);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    // The only descriptor of the read end is closed, the pipe still has a reader: the message
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]).expect(1);
    // The socket that holds the message closes: the reader goes with it
    p.sys(57, &[I(6)]).expect(0);
    p.sys(64, &[I(4), D(0x200), I(1)]).expect(-32);
    // A read that asks for no control data takes the descriptors and closes them
    p.sys(59, &[D(0x120), I(0)]).expect(0); // 3 and 6
    p.sys(199, &[I(1), I(1), I(0), D(0x128)]).expect(0); // 7 and 8
    p.msg(0x800, 0x200, 1, &rights(&[3]), 24);
    p.sys(211, &[I(7), D(0x800), I(0)]).expect(1);
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(6), D(0x200), I(1)]).expect(1);
    p.sys(63, &[I(8), D(0x300), I(16)]).expect(1);
    p.sys(64, &[I(6), D(0x200), I(1)]).expect(-32);
    // A descriptor that was closed cannot be sent
    p.sys(211, &[I(7), D(0x800), I(0)]).expect(-9);
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn datagrams_carry_descriptors_and_a_peek_gives_copies() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"dg").put(0x210, b"hi");
    pipe_and_pair(&mut p, 2);
    p.msg(M1, 0x200, 2, &rights(&[3]), 24);
    p.msg(M2, 0x300, 1, &[], 24);
    p.msg(M3, 0x320, 16, &[], 24);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(2);
    p.sys(57, &[I(3)]).expect(0);
    // The datagram is cut to 1 byte, which does not hold its descriptors back; a peek leaves
    // it queued, with a descriptor of its own (7)
    p.sys(212, &[I(6), D(M2), I(MSG_PEEK)]).expect(1);
    p.is(M2 + 48, 0x20).is(M2 + 40, 24);
    p.sys(212, &[I(6), D(M3), I(0)]).expect(2);
    p.is(M3 + 48, 0).is(M3 + 40, 24);
    p.sys(212, &[I(6), D(M3), I(MSG_DONTWAIT)]).expect(-11);
    p.sys(64, &[I(4), D(0x210), I(2)]).expect(2);
    p.sys(63, &[W(M3 + CTL + 16), D(0x340), I(16)]).expect(2);
    p.out(0x340, 2);
    // The peek's copy (7) and the received one (8) are both the read end: closing both lets
    // the writer know
    p.sys(57, &[W(M2 + CTL + 16)]).expect(0);
    p.sys(64, &[I(4), D(0x210), I(2)]).expect(2);
    p.sys(57, &[W(M3 + CTL + 16)]).expect(0);
    p.sys(64, &[I(4), D(0x210), I(2)]).expect(-32);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"hi");
}

#[test]
fn a_stream_read_that_takes_the_descriptors_ends_with_their_send() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"ab").put(0x210, b"cd").put(0x220, b"ef");
    pipe_and_pair(&mut p, 1);
    p.msg(M1, 0x200, 2, &rights(&[3]), 24);
    p.msg(M2, 0x210, 2, &[], 0);
    p.msg(0x800, 0x300, 16, &[], 24);
    p.msg(0x880, 0x320, 16, &[], 24);
    p.msg(0x900, 0x340, 16, &[], 24);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(2);
    p.sys(211, &[I(5), D(M2), I(0)]).expect(2);
    // "ab" with the descriptor, and no more of what follows
    p.sys(212, &[I(6), D(0x800), I(0)]).expect(2);
    p.is(0x800 + 40, 24);
    // "cd" has none
    p.sys(212, &[I(6), D(0x880), I(0)]).expect(2);
    p.is(0x880 + 40, 0);
    // Descriptors sent after plain bytes come with a read that reaches into their send: "cd" and
    // "ab" are read together, with the rights of "ab"
    p.sys(211, &[I(5), D(M2), I(0)]).expect(2);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(2);
    p.sys(212, &[I(6), D(0x900), I(0)]).expect(4);
    p.is(0x900 + 40, 24);
    p.out(0x340, 4);
    let mut p = start(&mut vm, p);
    let (out, status) = run(&mut p, &mut vm);
    ok(&status);
    assert_eq!(out, b"cdab");
}

#[test]
fn credentials_come_with_so_passcred() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x").put(0x210, &1u32.to_le_bytes());
    pipe_and_pair(&mut p, 1);
    p.msg(M1, 0x200, 1, &[], 0);
    p.msg(M2, 0x300, 1, &[], 32);
    p.msg(M3, 0x320, 1, &[], 32);
    // The receiving socket has not asked: no control data
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(M2), I(0)]).expect(1);
    p.is(M2 + 40, 0);
    p.sys(208, &[I(6), I(1), I(16), D(0x210), I(4)]).expect(0);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(212, &[I(6), D(M3), I(0)]).expect(1);
    // SCM_CREDENTIALS: the process (the first has pid 1000; umask keeps 9 bits), uid 0, gid 0
    p.is(M3 + 40, 32).is(M3 + CTL + 8, 1).is(M3 + CTL + 12, 2);
    p.is(M3 + CTL + 16, 1000 & 0o777)
        .is(M3 + CTL + 20, 0)
        .is(M3 + CTL + 24, 0);
    // What the sender gives is what arrives: root may give other ids, but a pid has to be a
    // process's (Linux's __scm_send: ESRCH)
    let creds = |pid: u32| {
        let mut c = pid.to_le_bytes().to_vec();
        c.extend_from_slice(&8u32.to_le_bytes());
        c.extend_from_slice(&9u32.to_le_bytes());
        cmsg(2, &c)
    };
    p.msg(0x980, 0x200, 1, &creds(7), 32);
    p.sys(211, &[I(5), D(0x980), I(0)]).expect(-3);
    p.msg(0x800, 0x200, 1, &creds(1000), 32);
    p.msg(0x880, 0x340, 1, &[], 32);
    p.sys(211, &[I(5), D(0x800), I(0)]).expect(1);
    p.sys(212, &[I(6), D(0x880), I(0)]).expect(1);
    p.is(0x880 + CTL + 16, 1000 & 0o777)
        .is(0x880 + CTL + 20, 8)
        .is(0x880 + CTL + 24, 9);
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn what_cannot_be_sent_as_control_data_fails() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x");
    pipe_and_pair(&mut p, 1);
    // A descriptor that is not open
    p.msg(M1, 0x200, 1, &rights(&[40]), 24);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(-9);
    // A type of SOL_SOCKET that is not passed, and a length that does not fit
    p.msg(M2, 0x200, 1, &cmsg(77, &[0; 4]), 24);
    p.sys(211, &[I(5), D(M2), I(0)]).expect(-22);
    let mut long = rights(&[3]);
    long[0] = 40;
    p.msg(M3, 0x200, 1, &long, 24);
    p.sys(211, &[I(5), D(M3), I(0)]).expect(-22);
    // Not over the network
    p.sys(198, &[I(2), I(2), I(0)]).expect(7);
    p.msg(0x800, 0x200, 1, &rights(&[3]), 24);
    p.sys(211, &[I(7), D(0x800), I(0)]).expect(-22);
    let mut p = start(&mut vm, p);
    let (_, status) = run(&mut p, &mut vm);
    ok(&status);
}

#[test]
fn descriptors_in_flight_survive_a_snapshot() {
    let mut vm = machine();
    let mut p = Prog::default();
    p.put(0x200, b"x").put(0x210, b"kept");
    pipe_and_pair(&mut p, 1);
    p.msg(M1, 0x200, 1, &rights(&[3]), 24);
    p.msg(M2, 0x300, 1, &[], 24);
    p.sys(211, &[I(5), D(M1), I(0)]).expect(1);
    p.sys(57, &[I(3)]).expect(0);
    p.sys(64, &[I(4), D(0x210), I(4)]).expect(4);
    p.sys(63, &[I(0), D(0x380), I(1)]).expect(1); // waits for a key
    p.sys(212, &[I(6), D(M2), I(0)]).expect(1);
    p.sys(63, &[W(M2 + CTL + 16), D(0x340), I(16)]).expect(4);
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
