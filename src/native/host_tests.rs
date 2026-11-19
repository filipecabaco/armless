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
    // The host answers in order, so once "good" is in, the two before it were failed
    for name in ["../armless-outside", "missing", "good"] {
        b.io.out.push(Event::Blob(name.into()));
    }
    until("the blobs", || {
        (seen.lock().unwrap().len() == 3).then_some(())
    });
    until("the good one", || b.read_file("/a").ok())
        .eq(b"abc")
        .then_some(())
        .unwrap();
    // A failed blob is not kept as failed: the next read asks for it again (and never gets the file outside)
    for (path, name) in [("/b", "../armless-outside"), ("/c", "missing")] {
        assert!(!b.has_blob(name), "{path}");
        match b.read_file(path) {
            Err(crate::vm::BootError::Need(n)) => assert_eq!(n, vec![name.to_string()], "{path}"),
            other => panic!("{path}: {:?}", other.map(|_| ())),
        }
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
        s.write_all(switching(&head).as_bytes()).unwrap();
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
    let host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("anything", 1).unwrap();
    g.syn(ip, 40002, 5432);
    seg(&mut g, "a reset", |s| s.has(RST));
    untracked(&host);
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

#[test]
fn a_link_dialer_opens_the_links_connections_and_none_leaves_it_to_the_host() {
    use std::os::unix::net::{UnixListener, UnixStream};
    // A socket the host could reach itself, and a link whose socket isn't there: the dialer
    // answers that one (as a sandboxed host's program asks whoever made the socket), and leaves
    // the other to the host
    let dir = std::env::temp_dir().join(format!("armless-dialer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let real = dir.join("real.sock");
    let _ = std::fs::remove_file(&real);
    let server = UnixListener::bind(&real).unwrap();
    std::thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        s.write_all(b"by the host").unwrap();
        std::thread::sleep(Duration::from_secs(1));
    });
    let asked = Arc::new(Mutex::new(Vec::new()));
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    host.set_links(vec![
        Tunnel { host: Some("db".into()), port: 5432, url: "unix:///nowhere/l.sock".into() },
        Tunnel { host: Some("cache".into()), port: 6379, url: format!("unix://{}", real.display()) },
    ]);
    {
        let asked = asked.clone();
        host.set_link_dialer(Arc::new(move |t: &Tunnel| {
            asked.lock().unwrap().push(t.url.clone());
            if t.url != "unix:///nowhere/l.sock" {
                return None;
            }
            let (mine, mut theirs) = UnixStream::pair().unwrap();
            std::thread::spawn(move || {
                theirs.write_all(b"by the dialer").unwrap();
                std::thread::sleep(Duration::from_secs(1));
            });
            Some(Ok(TcpStream::from(std::os::fd::OwnedFd::from(mine))))
        }));
    }
    let mut g = Guest::on_board(b.clone());
    for (name, sport, port, want) in [("db", 40040, 5432, &b"by the dialer"[..]), ("cache", 40041, 6379, b"by the host")] {
        let ip = g.resolve(name, 1).unwrap();
        g.syn(ip, sport, port);
        let sa = seg(&mut g, "the SYN-ACK", |s| s.has(SYN | ACK) && s.dport == sport);
        g.ack(ip, sport, port, sa.seq.wrapping_add(1));
        let d = seg(&mut g, "the bytes", |s| !s.payload.is_empty() && s.dport == sport);
        assert_eq!(d.payload, want, "{name}");
    }
    assert_eq!(asked.lock().unwrap().len(), 2, "the dialer is asked for every link connection");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_link_its_dialer_fails_is_refused_and_forgotten() {
    // The dialer's own error stands: the host doesn't try the address after it
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    host.set_links(vec![Tunnel { host: Some("db".into()), port: 5432, url: "unix:///nowhere/l.sock".into() }]);
    host.set_link_dialer(Arc::new(|_: &Tunnel| {
        Some(Err(std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "not yours")))
    }));
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db", 1).unwrap();
    g.syn(ip, 40050, 5432);
    seg(&mut g, "a reset", |s| s.has(RST) && s.dport == 40050);
    untracked(&host);
}

#[test]
fn a_link_dialer_is_asked_for_links_only() {
    // A connection no link takes goes out as before, past the dialer
    let outside = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = outside.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut s, _) = outside.accept().unwrap();
        s.write_all(b"outside").unwrap();
        thread::sleep(Duration::from_secs(1));
    });
    let asked = Arc::new(Mutex::new(0));
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    host.set_links(vec![Tunnel { host: Some("db".into()), port: 5432, url: "unix:///nowhere/l.sock".into() }]);
    {
        let asked = asked.clone();
        host.set_link_dialer(Arc::new(move |_: &Tunnel| {
            *asked.lock().unwrap() += 1;
            None
        }));
    }
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("localhost", 1).unwrap();
    g.syn(ip, 40060, port);
    let sa = seg(&mut g, "the SYN-ACK", |s| s.has(SYN | ACK) && s.dport == 40060);
    g.ack(ip, 40060, port, sa.seq.wrapping_add(1));
    let d = seg(&mut g, "the bytes", |s| !s.payload.is_empty() && s.dport == 40060);
    assert_eq!(d.payload, b"outside");
    assert_eq!(*asked.lock().unwrap(), 0, "the dialer is not asked for a connection no link takes");
}

#[test]
fn a_new_link_dialer_takes_the_new_connections() {
    use std::os::unix::net::UnixStream;
    // Each dialer answers with its own name
    let dialer = |name: &'static [u8]| -> crate::native::LinkDialer {
        Arc::new(move |_: &Tunnel| {
            let (mine, mut theirs) = UnixStream::pair().unwrap();
            thread::spawn(move || {
                theirs.write_all(name).unwrap();
                thread::sleep(Duration::from_secs(1));
            });
            Some(Ok(TcpStream::from(std::os::fd::OwnedFd::from(mine))))
        })
    };
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    // Given before the links: it waits for them
    host.set_link_dialer(dialer(b"first"));
    host.set_links(vec![Tunnel { host: Some("db".into()), port: 5432, url: "unix:///nowhere/l.sock".into() }]);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db", 1).unwrap();
    for (sport, want) in [(40070, &b"first"[..]), (40071, b"second")] {
        if sport == 40071 {
            host.set_link_dialer(dialer(b"second"));
        }
        g.syn(ip, sport, 5432);
        let sa = seg(&mut g, "the SYN-ACK", |s| s.has(SYN | ACK) && s.dport == sport);
        g.ack(ip, sport, 5432, sa.seq.wrapping_add(1));
        let d = seg(&mut g, "the bytes", |s| !s.payload.is_empty() && s.dport == sport);
        assert_eq!(d.payload, want);
    }
}

/// How the guest ends a connection it has just written to.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// The outside has said it is done writing (the host's FIN is acknowledged), then the guest
    /// sends its FIN: the host sees the close the moment the FIN arrives.
    Fin,
    /// A reset, as when a process closes a socket with unread data.
    Reset,
}

/// The guest has the connection to `outside` up (`dst`, `sport` and `dport` address its segments,
/// `theirs` is the host's next sequence number). It sends `payload` and ends the connection at
/// once, so the data and the close reach the host together. The outside reads nothing until the
/// host has seen the close: a writer still busy then must still deliver everything.
fn guest_sends_then_closes(
    mut g: Guest,
    seen: &Mutex<Vec<Event>>,
    mut outside: TcpStream,
    (dst, sport, dport, mut theirs): ([u8; 4], u16, u16, u32),
    payload: &[u8],
    ending: Ending,
) {
    outside
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    if let Ending::Fin = ending {
        outside.shutdown(Shutdown::Write).unwrap();
        let fin = seg(&mut g, "the FIN for the outside's EOF", |s| s.has(FIN));
        theirs = fin.seq.wrapping_add(1);
        g.ack(dst, sport, dport, theirs);
    }
    // The outside reads as it comes, as a peer does: with flow control, a host that can't write
    // what the guest sent stops taking more, so a reader that waited for the end never got it
    let reader = thread::spawn(move || {
        let mut got = Vec::new();
        let read = outside.read_to_end(&mut got);
        (got, read)
    });
    // As a TCP stack does: no more in flight than the window the host's side offers (with flow
    // control, what the host hasn't consumed yet closes it)
    let base = g.seq[&sport];
    let (mut sent, mut acked, mut window) = (0usize, 0usize, 65535usize);
    while sent < payload.len() {
        for s in g.segs() {
            let a = s.ack.wrapping_sub(base) as usize;
            if a <= sent && a >= acked {
                acked = a;
                window = s.window as usize;
            }
        }
        let room = (acked + window).saturating_sub(sent);
        if room == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let n = room.min(1400).min(payload.len() - sent);
        g.data(dst, sport, dport, theirs, &payload[sent..sent + n]);
        sent += n;
    }
    match ending {
        Ending::Fin => g.fin(dst, sport, dport, theirs),
        Ending::Reset => g.rst(dst, sport, dport, theirs),
    }
    until("the close", || {
        let seen = seen.lock().unwrap();
        seen.iter()
            .any(|e| matches!(e, Event::TcpClosed { .. }))
            .then_some(())
    });
    let (got, read) = reader.join().unwrap();
    assert!(
        got == payload,
        "{ending:?}: the outside got {} of the guest's {} bytes ({read:?})",
        got.len(),
        payload.len()
    );
}

/// Bytes that tell where they are, so a hole or a repeat shows.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8 ^ (i / 251) as u8).collect()
}

/// Payloads for the tests below: a small one, and one that outlasts the socket buffers, so the
/// host's writer is still busy when it handles the close.
fn payloads() -> [Vec<u8>; 2] {
    [pattern(100), pattern(32 << 20)]
}

#[test]
fn what_the_guest_wrote_before_closing_reaches_a_forwarded_socket() {
    for payload in payloads() {
        for ending in [Ending::Fin, Ending::Reset] {
            let b = board(true);
            let (obs, seen) = observer();
            let host = NativeHost::spawn(b.clone(), options(true), obs);
            let mut g = Guest::on_board(b.clone());
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let outside = TcpStream::connect(l.local_addr().unwrap()).unwrap();
            let (inside, _) = l.accept().unwrap();
            host.forward(inside, 8080);
            let syn = seg(&mut g, "the SYN into the guest", |s| {
                s.has(SYN) && s.dport == 8080
            });
            g.accept(&syn, 0x1000);
            let to = (syn.src, syn.dport, syn.sport, syn.seq.wrapping_add(1));
            guest_sends_then_closes(g, &seen, outside, to, &payload, ending);
            untracked(&host);
        }
    }
}

#[test]
fn what_the_guest_wrote_before_closing_reaches_the_host_server_it_connected_to() {
    for payload in payloads() {
        for ending in [Ending::Fin, Ending::Reset] {
            let b = board(true);
            let (obs, seen) = observer();
            let host = NativeHost::spawn(b.clone(), options(true), obs);
            let mut g = Guest::on_board(b.clone());
            let ip = g.resolve("localhost", 1).unwrap();
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let accepted = thread::spawn(move || l.accept().unwrap().0);
            g.syn(ip, 40000, port);
            let sa = seg(&mut g, "the SYN-ACK once the host connected", |s| {
                s.has(SYN | ACK)
            });
            let theirs = sa.seq.wrapping_add(1);
            g.ack(ip, 40000, port, theirs);
            let outside = accepted.join().unwrap();
            guest_sends_then_closes(
                g,
                &seen,
                outside,
                (ip, 40000, port, theirs),
                &payload,
                ending,
            );
            untracked(&host);
        }
    }
}

/// A server that stops reading closes the guest's window: what the guest can make the host hold
/// is the socket's buffers and a window more, not all it cares to send. Reading again opens it.
#[test]
fn a_server_that_does_not_read_makes_the_guest_wait() {
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("localhost", 1).unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let accepted = thread::spawn(move || l.accept().unwrap().0);
    g.syn(ip, 40500, port);
    let sa = seg(&mut g, "the SYN-ACK", |s| s.has(SYN | ACK));
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 40500, port, theirs);
    let mut outside = accepted.join().unwrap();
    let base = g.seq[&40500];
    let chunk = [0x5a; 1400];
    let (mut sent, mut acked, mut window) = (0usize, 0usize, 65535usize);
    let mut stuck = Instant::now();
    // Send within the window until it stays shut for half a second
    while stuck.elapsed() < Duration::from_millis(500) {
        for s in g.segs() {
            let a = s.ack.wrapping_sub(base) as usize;
            if a <= sent && a >= acked {
                acked = a;
                window = s.window as usize;
            }
        }
        let room = (acked + window).saturating_sub(sent);
        if room == 0 {
            thread::sleep(Duration::from_millis(1));
            continue;
        }
        let n = room.min(chunk.len());
        g.data(ip, 40500, port, theirs, &chunk[..n]);
        sent += n;
        stuck = Instant::now();
        assert!(sent < 256 << 20, "the guest was never held back");
    }
    assert_eq!(window, 0);
    // Reading opens it again
    outside.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = vec![0u8; 1 << 20];
    let mut read = 0;
    let opened = until("the window to open", || {
        if let Ok(n) = outside.read(&mut buf) {
            read += n;
        }
        g.segs().into_iter().find(|s| s.window > 0)
    });
    assert!(opened.window > 0 && read > 0);
    g.rst(ip, 40500, port, theirs);
    untracked(&host);
}

#[test]
fn what_the_guest_wrote_before_closing_reaches_a_tunnels_far_side() {
    let (port, frames) = ws_server(b"server says hi");
    let b = board(true);
    let (obs, _) = observer();
    let opts = HostOptions {
        tunnels: vec![Tunnel::parse(&format!("db.internal:5432=ws://127.0.0.1:{port}/t")).unwrap()],
        ..options(true)
    };
    let host = NativeHost::spawn(b.clone(), opts, obs);
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
    let theirs = theirs.wrapping_add(d.payload.len() as u32);
    // The guest writes and resets at once
    g.data(ip, 40000, 5432, theirs, b"the last words");
    g.rst(ip, 40000, 5432, theirs);
    let next = || frames.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(next(), (ws::BINARY, b"the last words".to_vec()));
    assert_eq!(next().0, ws::CLOSE);
    untracked(&host);
}

/// What the host keeps for connections (the flags, sockets, tunnels), once there are none left.
fn untracked(host: &NativeHost) {
    until("the host to forget its connections", || {
        (host.conns.lock().unwrap().tracked() == 0).then_some(())
    });
}

#[test]
fn what_the_host_keeps_for_closed_connections_goes_with_them() {
    const N: u16 = 64;
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    // Every connection the host makes must end: its socket closed, not left in a table
    let (eof_tx, eof_rx) = mpsc::channel();
    thread::spawn(move || {
        for _ in 0..N {
            let (mut s, _) = l.accept().unwrap();
            let eof_tx = eof_tx.clone();
            thread::spawn(move || {
                let mut sink = Vec::new();
                let _ = s.read_to_end(&mut sink);
                let _ = eof_tx.send(());
            });
        }
    });
    let b = board(true);
    let (obs, seen) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("localhost", 1).unwrap();
    for i in 0..N {
        let sport = 41000 + i;
        g.syn(ip, sport, port);
        if i % 2 == 0 {
            // Reset once the connection is up
            let sa = seg(&mut g, "the SYN-ACK", |s| {
                s.has(SYN | ACK) && s.dport == sport
            });
            let theirs = sa.seq.wrapping_add(1);
            g.ack(ip, sport, port, theirs);
            g.rst(ip, sport, port, theirs);
        } else {
            // Reset before the host has connected, perhaps before its thread has started: the
            // close comes first, and the socket the thread makes after it must not be kept
            g.rst(ip, sport, port, 0);
        }
    }
    for _ in 0..N {
        eof_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a host socket was kept open after the guest closed its connection");
    }
    until("every close", || {
        let closed = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::TcpClosed { .. }))
            .count();
        (closed == N as usize).then_some(())
    });
    untracked(&host);
}

/// A request through the web relay, its response under way and the guest taking none of it.
struct Endless {
    host: NativeHost,
    g: Guest,
    ip: [u8; 4],
    theirs: u32,
    /// Why the server stopped writing
    done: mpsc::Receiver<std::io::Error>,
}

/// A request for a response nobody can finish reading (a terabyte): the server writes until it
/// can't.
fn endless_request() -> Endless {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (done_tx, done_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1099511627776\r\n\r\n")
            .unwrap();
        let chunk = [7u8; 64 * 1024];
        let _ = done_tx.send(loop {
            if let Err(e) = s.write_all(&chunk) {
                break e;
            }
        });
    });
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("localhost", 1).unwrap();
    g.syn(ip, 42000, 80);
    let sa = seg(&mut g, "the SYN-ACK", |s| s.has(SYN | ACK));
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 42000, 80, theirs);
    g.data(
        ip,
        42000,
        80,
        theirs,
        format!("GET /big HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
    );
    // The guest takes nothing: the host's thread waits for it, holding the response open
    seg(&mut g, "the response", |s| !s.payload.is_empty());
    thread::sleep(Duration::from_millis(500));
    assert!(
        done_rx.try_recv().is_err(),
        "the server stopped writing before anything closed"
    );
    Endless {
        host,
        g,
        ip,
        theirs,
        done: done_rx,
    }
}

#[test]
fn a_request_stops_when_the_guest_closes_its_connection_and_leaves_nothing_kept() {
    let mut r = endless_request();
    r.g.rst(r.ip, 42000, 80, r.theirs);
    r.done
        .recv_timeout(Duration::from_secs(5))
        .expect("the host kept reading the response after the guest closed");
    untracked(&r.host);
}

#[test]
fn a_request_stops_when_the_host_goes() {
    let r = endless_request();
    drop(r.host);
    r.done
        .recv_timeout(Duration::from_secs(5))
        .expect("the request kept reading the response after its host was dropped");
}

#[test]
fn a_tunnel_the_far_side_resets_is_reset_for_the_guest_and_forgotten() {
    // A WebSocket server that takes the handshake, then resets the connection once the client
    // sends something (closing with bytes unread sends a reset)
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        s.write_all(switching(&head).as_bytes()).unwrap();
        let _ = s.peek(&mut b);
    });
    let b = board(true);
    let (obs, seen) = observer();
    let opts = HostOptions {
        tunnels: vec![Tunnel::parse(&format!("5432=ws://127.0.0.1:{port}/")).unwrap()],
        ..options(true)
    };
    let host = NativeHost::spawn(b.clone(), opts, obs);
    let mut g = Guest::on_board(b.clone());
    let ip = g.resolve("db.internal", 1).unwrap();
    g.syn(ip, 40000, 5432);
    let sa = seg(&mut g, "the SYN-ACK once the WebSocket is open", |s| {
        s.has(SYN | ACK)
    });
    let theirs = sa.seq.wrapping_add(1);
    g.ack(ip, 40000, 5432, theirs);
    g.data(ip, 40000, 5432, theirs, b"SELECT 1");
    // The host resets the guest's connection: no TcpClosed comes, the host forgets it itself
    seg(&mut g, "the reset", |s| s.has(RST));
    untracked(&host);
    assert!(!seen
        .lock()
        .unwrap()
        .iter()
        .any(|e| matches!(e, Event::TcpClosed { .. })));
}

#[test]
fn connections_a_process_never_accepted_end_when_it_stops_listening() {
    let b = board(true);
    let (obs, _) = observer();
    let host = NativeHost::spawn(b.clone(), options(true), obs);
    b.local_listen(8080);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut outside = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (inside, _) = l.accept().unwrap();
    host.forward(inside, 8080);
    until("the host's socket for the connection", || {
        (host.conns.lock().unwrap().tracked() > 0).then_some(())
    });
    // The process closes its listening socket without accepting: the outside hears the end
    b.local_unlisten(8080);
    outside
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(outside.read(&mut buf).unwrap(), 0);
    untracked(&host);
}

/// A server's 101 for the WebSocket handshake request `head` (its key answered).
fn switching(head: &[u8]) -> String {
    let head = String::from_utf8_lossy(head);
    let key = head
        .lines()
        .find_map(|l| l.strip_prefix("Sec-WebSocket-Key: "))
        .expect("a WebSocket key");
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        crate::dev::web::ws::accept_key(key.trim())
    )
}
