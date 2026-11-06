//! The native host against a machine with no guest running: the test plays the guest's driver
//! (frames into the network stack, frames out of it), the host answers on its own thread with
//! real sockets on loopback.

use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use super::*;
use crate::dev::netstack::tests::{flags::*, Guest, Seg, GUEST_MAC};
use crate::dev::web::ws;
use crate::vm::{Board, Config};

fn board(net: bool) -> Arc<Board> {
    let mut config = Config::default();
    config.devices.fs_tag = Some("root".into());
    config.devices.net = net;
    config.devices.mac = GUEST_MAC;
    Arc::new(Board::new(
        Arc::new(crate::smp::Machine::new(1 << 20, 1)),
        config,
        || 0,
    ))
}

fn options(network: bool) -> HostOptions {
    HostOptions {
        network,
        egress: Egress {
            allow_private: true,
            ..Egress::default()
        },
        ..HostOptions::default()
    }
}

fn observer() -> (Observer, Arc<Mutex<Vec<Event>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    (
        Box::new(move |e: &Event| s.lock().unwrap().push(e.clone())),
        seen,
    )
}

/// Wait (up to 5 s) for `f` to give something.
fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Segments to the guest, collected until one matches.
fn seg(g: &mut Guest, what: &str, f: impl Fn(&Seg) -> bool) -> Seg {
    until(what, || g.segs().into_iter().find(|s| f(s)))
}

#[test]
fn blobs_come_from_the_directory_or_the_source_and_names_outside_it_fail() {
    let dir = std::env::temp_dir().join(format!("armless-blobs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("good"), b"abc").unwrap();
    std::fs::write(dir.join("../armless-outside"), b"secret").unwrap();
    let json = br#"{"version":3,"fsroot":[
        ["a",3,0,33188,0,0,"good"],
        ["b",6,0,33188,0,0,"../armless-outside"],
        ["c",3,0,33188,0,0,"missing"]]}"#;
    let b = board(false);
    b.load_filesystem(json).unwrap();
    let (obs, seen) = observer();
    let host = NativeHost::spawn(
        b.clone(),
        HostOptions {
            blobs: Some(dir.clone()),
            ..options(false)
        },
        obs,
    );
    for name in ["good", "../armless-outside", "missing"] {
        b.io.out.push(Event::Blob(name.into()));
    }
    until("the blobs", || {
        (seen.lock().unwrap().len() == 3).then_some(())
    });
    until("the good one", || b.read_file("/a").ok())
        .eq(b"abc")
        .then_some(())
        .unwrap();
    for path in ["/b", "/c"] {
        let e = until("a failure", || match b.read_file(path) {
            Err(crate::vm::BootError::Fail(e)) => Some(e),
            _ => None,
        });
        assert!(e.contains("error 5"), "{path}: {e}");
    }
    drop(host);
    std::fs::remove_dir_all(&dir).unwrap();
    let _ = std::fs::remove_file(std::env::temp_dir().join("armless-outside"));

    // A source answers for any name it knows
    let b = board(false);
    b.load_filesystem(json).unwrap();
    let source = BlobSource(Arc::new(|name: &str| {
        (name == "../armless-outside").then(|| b"inside".to_vec())
    }));
    let (obs, _) = observer();
    let _host = NativeHost::spawn(
        b.clone(),
        HostOptions {
            blob_source: Some(source),
            ..options(false)
        },
        obs,
    );
    b.io.out.push(Event::Blob("../armless-outside".into()));
    assert_eq!(
        until("the source's blob", || b.read_file("/b").ok()),
        b"inside"
    );
}

/// A server on loopback taking one connection: what it read (until `want` bytes), then it
/// writes `reply` and closes.
fn server(want: usize, reply: &'static [u8]) -> (u16, mpsc::Receiver<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut got = vec![0; want];
        s.read_exact(&mut got).unwrap();
        tx.send(got).unwrap();
        s.write_all(reply).unwrap();
    });
    (port, rx)
}

#[test]
fn the_guests_connections_reach_host_servers_and_come_back() {
    let b = board(true);
    let (obs, seen) = observer();
    let _host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut g = Guest::on_board(b.clone());
    // DNS answers localhost as loopback itself: the host is asked for the address
    let ip = g.resolve("localhost", 1).unwrap();
    assert_eq!(ip, [127, 0, 0, 1]);
    let (port, got) = server(4, b"pong");
    g.syn(ip, 40000, port);
    let sa = seg(&mut g, "the SYN-ACK once the host connected", |s| {
        s.has(SYN | ACK)
    });
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 40000, port, theirs);
    g.data(ip, 40000, port, theirs, b"ping");
    assert_eq!(got.recv_timeout(Duration::from_secs(5)).unwrap(), b"ping");
    let reply = seg(&mut g, "the server's reply", |s| !s.payload.is_empty());
    assert_eq!(reply.payload, b"pong");
    seg(&mut g, "the FIN when the server closed", |s| s.has(FIN));
    assert!(seen.lock().unwrap().iter().any(
        |e| matches!(e, Event::TcpOpen { host, port: p, .. } if host == "127.0.0.1" && *p == port)
    ), "{:?}", seen.lock().unwrap());

    // Nobody listening: refused
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    g.syn(ip, 40001, closed);
    seg(&mut g, "a reset", |s| s.has(RST) && s.dport == 40001);
}

#[test]
fn with_the_network_off_connections_are_refused() {
    let b = board(true);
    let (obs, _) = observer();
    let _host = NativeHost::spawn(b.clone(), options(false), obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("example.com", 1).unwrap();
    g.syn(ip, 40000, 443);
    let r = seg(&mut g, "a reset", |s| s.has(RST));
    assert_eq!(r.dport, 40000);
}

#[test]
fn host_sockets_are_forwarded_into_the_guest() {
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut g = Guest::on_board(b.clone());
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut outside = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (inside, _) = l.accept().unwrap();
    host.forward(inside, 8080);
    let syn = seg(&mut g, "the SYN into the guest", |s| {
        s.has(SYN) && s.dport == 8080
    });
    // The guest accepts
    g.accept(&syn, 0x1000);
    outside.write_all(b"hello guest").unwrap();
    let d = seg(&mut g, "the outside's bytes", |s| !s.payload.is_empty());
    assert_eq!(d.payload, b"hello guest");
    // And answers
    g.reply(
        &syn,
        d.seq.wrapping_add(d.payload.len() as u32),
        b"hello outside",
    );
    outside
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 13];
    outside.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello outside");

    // A machine without a network hangs up on forwarded sockets
    let b = board(false);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut outside = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (inside, _) = l.accept().unwrap();
    host.forward(inside, 8080);
    outside
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(outside.read(&mut buf).unwrap(), 0);
}

/// A WebSocket server for one tunnel: it sends `greeting` as a binary message, then gives back
/// the payloads of the client's frames.
fn ws_server(greeting: &'static [u8]) -> (u16, mpsc::Receiver<(u8, Vec<u8>)>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        s.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n").unwrap();
        s.write_all(&ws::encode(ws::BINARY, greeting, None))
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while let Ok(n) = s.read(&mut chunk) {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some((frame, used)) = ws::parse(&buf) {
                buf.drain(..used);
                let close = frame.opcode == ws::CLOSE;
                let _ = tx.send((frame.opcode, frame.payload));
                if close {
                    let _ = s.write_all(&ws::encode(ws::CLOSE, &ws::close_payload(1000), None));
                    return;
                }
            }
        }
    });
    (port, rx)
}

#[test]
fn tunnelled_destinations_are_carried_over_a_websocket() {
    let (port, frames) = ws_server(b"server says hi");
    let b = board(true);
    let (obs, _) = observer();
    let opts = HostOptions {
        tunnels: vec![Tunnel::parse(&format!("db.internal:5432=ws://127.0.0.1:{port}/t")).unwrap()],
        ..options(true)
    };
    let _host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db.internal", 1).unwrap();
    g.syn(ip, 40000, 5432);
    let sa = seg(&mut g, "the SYN-ACK once the WebSocket is open", |s| {
        s.has(SYN | ACK)
    });
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 40000, 5432, theirs);
    let d = seg(&mut g, "the server's message as bytes", |s| {
        !s.payload.is_empty()
    });
    assert_eq!(d.payload, b"server says hi");
    let theirs = theirs.wrapping_add(d.payload.len() as u32);
    g.data(ip, 40000, 5432, theirs, b"SELECT 1");
    let next =
        |rx: &mpsc::Receiver<(u8, Vec<u8>)>| rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        next(&frames),
        (ws::BINARY, b"SELECT 1".to_vec()),
        "a binary message, masked"
    );
    // The guest finishes: the WebSocket is closed, and the server's close ends the connection
    g.fin(ip, 40000, 5432, theirs);
    assert_eq!(next(&frames).0, ws::CLOSE);
    seg(&mut g, "the FIN after the server's close", |s| s.has(FIN));

    // A tunnel nobody answers is refused like a connection
    let dead = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let b = board(true);
    let (obs, _) = observer();
    let opts = HostOptions {
        tunnels: vec![Tunnel::parse(&format!("5432=ws://127.0.0.1:{dead}/")).unwrap()],
        ..options(true)
    };
    let _host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("anything", 1).unwrap();
    g.syn(ip, 40002, 5432);
    seg(&mut g, "a reset", |s| s.has(RST));
}

#[test]
fn tcp_tunnels_carry_a_connection_to_their_address() {
    // A server on the host's loopback, which the guest knows as db.internal:5432
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let (got, got_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        s.write_all(b"server says hi").unwrap();
        let mut buf = [0u8; 8];
        s.read_exact(&mut buf).unwrap();
        got.send(buf.to_vec()).unwrap();
    });
    let b = board(true);
    let (obs, _) = observer();
    let opts = HostOptions {
        tunnels: vec![Tunnel::parse(&format!("db.internal:5432=tcp://127.0.0.1:{port}")).unwrap()],
        ..options(true)
    };
    let _host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db.internal", 1).unwrap();
    g.syn(ip, 40010, 5432);
    let sa = seg(&mut g, "the SYN-ACK once the tunnel's address answers", |s| {
        s.has(SYN | ACK)
    });
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 40010, 5432, theirs);
    let d = seg(&mut g, "the server's bytes", |s| !s.payload.is_empty());
    assert_eq!(d.payload, b"server says hi");
    let theirs = theirs.wrapping_add(d.payload.len() as u32);
    g.data(ip, 40010, 5432, theirs, b"SELECT 1");
    assert_eq!(got_rx.recv_timeout(Duration::from_secs(5)).unwrap(), b"SELECT 1");
}

#[test]
fn links_set_while_it_runs_reach_their_address_even_a_private_one() {
    // A server on the host's loopback, which the guest knows as db:5432 once the link is set
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        s.write_all(b"linked").unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let b = board(true);
    let (obs, _) = observer();
    // Private addresses off limits: a link is the host's own and goes anyway
    let mut opts = options(true);
    opts.egress.allow_private = false;
    let host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db", 1).unwrap();
    // Before the link: nothing there (refused, as a private address)
    g.syn(ip, 40020, 5432);
    seg(&mut g, "a reset before the link", |s| s.has(RST));
    host.set_links(vec![Tunnel::parse(&format!("db:5432=tcp://127.0.0.1:{port}")).unwrap()]);
    g.syn(ip, 40021, 5432);
    let sa = seg(&mut g, "the SYN-ACK once the link's address answers", |s| s.has(SYN | ACK));
    g.ack(ip, 40021, 5432, sa.seq.wrapping_add(1));
    let d = seg(&mut g, "the server's bytes", |s| !s.payload.is_empty());
    assert_eq!(d.payload, b"linked");
}

#[cfg(unix)]
#[test]
fn a_unix_link_reaches_its_socket_and_nothing_else_leaves_with_egress_denied() {
    use std::os::unix::net::UnixListener;
    let dir = std::env::temp_dir().join(format!("armless-link-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("l.sock");
    let _ = std::fs::remove_file(&sock);
    let server = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        s.write_all(b"over unix").unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    // Something on the loopback that only a guest allowed out could reach
    let open = TcpListener::bind("127.0.0.1:0").unwrap();
    let open_port = open.local_addr().unwrap().port();
    let b = board(true);
    let (obs, _) = observer();
    let mut opts = options(true);
    opts.egress.deny_all = true;
    let host = NativeHost::spawn(b.clone(), opts, obs);
    host.set_links(vec![Tunnel { host: Some("db".into()), port: 5432, url: format!("unix://{}", sock.display()) }]);
    let mut g = Guest::on_board(b.clone());
    let out = g.resolve("localhost.example", 1).unwrap();
    let ip = g.resolve("db", 1).unwrap();
    // Out: refused, even to an allowed private address
    g.syn(out, 40030, open_port);
    seg(&mut g, "a reset: nothing leaves", |s| s.has(RST));
    // The link: through
    g.syn(ip, 40031, 5432);
    let sa = seg(&mut g, "the SYN-ACK once the socket answers", |s| s.has(SYN | ACK));
    g.ack(ip, 40031, 5432, sa.seq.wrapping_add(1));
    let d = seg(&mut g, "the server's bytes", |s| !s.payload.is_empty());
    assert_eq!(d.payload, b"over unix");
    std::fs::remove_dir_all(&dir).ok();
}
