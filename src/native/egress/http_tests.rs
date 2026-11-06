//! Egress against servers and proxies on loopback: what goes on the wire, and how answers are read.

use super::*;
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// A server on loopback that takes one connection, reads the request head (and a body of its
/// content-length), answers `reply` and closes. Returns its port, and the request it got.
fn server(reply: &'static [u8]) -> (u16, mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let req = read_request(&mut s);
        s.write_all(reply).unwrap();
        let _ = tx.send(req);
    });
    (port, rx)
}

fn read_request(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        if s.read(&mut b).unwrap() == 0 {
            break;
        }
        buf.push(b[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let len = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().to_string())
        })
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).unwrap();
    head + &String::from_utf8_lossy(&body)
}

fn local() -> Egress {
    Egress {
        allow_private: true,
        ..Default::default()
    }
}

fn body(r: Response) -> String {
    let mut s = String::new();
    let mut b = r.body;
    b.read_to_string(&mut s).unwrap();
    s
}

#[test]
fn proxies_parse_from_their_urls() {
    let p = Proxy::parse("http://user:secret@proxy.corp:3128").unwrap();
    assert_eq!((p.host.as_str(), p.port), ("proxy.corp", 3128));
    assert_eq!(p.auth.as_deref(), Some("Basic dXNlcjpzZWNyZXQ="));
    let p = Proxy::parse("proxy.corp").unwrap();
    assert_eq!(
        (p.host.as_str(), p.port, p.auth.is_none()),
        ("proxy.corp", 80, true)
    );
    let p = Proxy::parse("http://[fd00::1]:8080/").unwrap();
    assert_eq!((p.host.as_str(), p.port), ("fd00::1", 8080));
    assert!(Proxy::parse("http://host:notaport").is_none());
}

#[test]
fn urls_parse_into_targets() {
    let t = Target::parse("https://example.com/a/b?c=1").unwrap();
    assert_eq!(
        (t.secure, t.host.as_str(), t.port, t.path.as_str()),
        (true, "example.com", 443, "/a/b?c=1")
    );
    assert_eq!(t.authority(), "example.com");
    let t = Target::parse("http://example.com:8080").unwrap();
    assert_eq!(
        (t.port, t.path.as_str(), t.authority()),
        (8080, "/", "example.com:8080".into())
    );
    let t = Target::parse("ws://example.com?x").unwrap();
    assert_eq!(t.path, "/?x");
    let t = Target::parse("wss://[::1]:9443/s").unwrap();
    assert_eq!(
        (t.secure, t.host.as_str(), t.authority()),
        (true, "::1", "[::1]:9443".into())
    );
    assert!(Target::parse("ftp://example.com").is_none());
    assert!(Target::parse("example.com").is_none());
}

#[test]
fn no_proxy_names_go_direct() {
    let proxy = Proxy::parse("http://proxy:1").unwrap();
    let e = Egress {
        https_proxy: Some(proxy.clone()),
        http_proxy: Some(Proxy { port: 2, ..proxy }),
        no_proxy: vec!["internal.corp".into(), ".svc".into(), "*.local".into()],
        allow_private: false,
        deny_all: false,
    };
    for host in ["internal.corp", "a.internal.corp", "db.svc", "X.Local"] {
        assert!(e.proxy_for(host, true).is_none(), "{host}");
    }
    for host in ["example.com", "notinternal.corp", "svc.example"] {
        assert_eq!(
            e.proxy_for(host, true).map(|p| p.port),
            Some(1),
            "{host}: tunnelled"
        );
        assert_eq!(
            e.proxy_for(host, false).map(|p| p.port),
            Some(2),
            "{host}: forwarded"
        );
    }
    let all = Egress {
        no_proxy: vec!["*".into()],
        ..e
    };
    assert!(all.proxy_for("anything", true).is_none());
}

#[test]
fn private_addresses_are_refused_unless_allowed() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let e = Egress::default();
    let err = e.connect("127.0.0.1", port).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("private"), "{err}");
    assert!(local().connect("127.0.0.1", port).is_ok());
}

#[test]
fn a_request_carries_the_guests_headers_but_not_its_hop_by_hop_ones() {
    let (port, got) =
        server(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nX-Reply: yes\r\n\r\nokEXTRA");
    let headers = [
        ("Accept", "text/plain"),
        ("Connection", "keep-alive"),
        ("Keep-Alive", "timeout=5"),
        ("Transfer-Encoding", "chunked"),
        ("Expect", "100-continue"),
        ("Host", "spoofed.example"),
        ("TE", "trailers"),
        ("X-Custom", "1"),
    ]
    .map(|(k, v)| (k.to_string(), v.to_string()));
    let r = local()
        .request(
            "POST",
            &format!("http://127.0.0.1:{port}/up?x=1"),
            &headers,
            b"data",
        )
        .unwrap();
    assert_eq!((r.status, r.reason.as_str()), (201, "Created"));
    assert!(r.headers.iter().any(|(k, v)| k == "X-Reply" && v == "yes"));
    assert_eq!(body(r), "ok", "the body is its content-length, no more");
    let req = got.recv().unwrap();
    let lines: Vec<&str> = req.split("\r\n").collect();
    assert_eq!(lines[0], "POST /up?x=1 HTTP/1.1");
    assert!(
        req.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
        "{req}"
    );
    assert!(req.contains("Accept: text/plain\r\n") && req.contains("X-Custom: 1\r\n"));
    assert!(req.contains("content-length: 4\r\n") && req.ends_with("\r\n\r\ndata"));
    assert!(req.contains("connection: close\r\n"));
    for gone in [
        "keep-alive",
        "Transfer-Encoding",
        "Expect",
        "spoofed",
        "TE:",
    ] {
        assert!(!req.contains(gone), "{gone} forwarded:\n{req}");
    }
}

#[test]
fn response_bodies_are_read_however_they_are_framed() {
    // Chunked, with an extension and a trailer
    let (port, _) = server(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3;ext=1\r\nabc\r\n2\r\nde\r\n0\r\nX-T: 1\r\n\r\n");
    let r = local()
        .request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .unwrap();
    assert_eq!(body(r), "abcde");
    // Interim answers first: the real one is what counts
    let (port, _) = server(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </s>\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nz");
    let r = local()
        .request("PUT", &format!("http://127.0.0.1:{port}/"), &[], b"x")
        .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(body(r), "z");
    // No length: until the server closes
    let (port, _) = server(b"HTTP/1.0 200 OK\r\n\r\nuntil the end");
    let r = local()
        .request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .unwrap();
    assert_eq!(body(r), "until the end");
    // HEAD and 204/304 have no body, whatever their headers say
    let (port, _) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 50\r\n\r\nleftover");
    let r = local()
        .request("HEAD", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .unwrap();
    assert_eq!(body(r), "");
    let (port, _) = server(b"HTTP/1.1 204 No Content\r\nContent-Length: 50\r\n\r\nleftover");
    let r = local()
        .request("DELETE", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .unwrap();
    assert_eq!((r.status, body(r)), (204, String::new()));
    // A server that hangs up without answering, or answers garbage
    let (port, _) = server(b"");
    assert!(local()
        .request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .is_err());
    let (port, _) = server(b"nonsense\r\n\r\n");
    assert!(local()
        .request("GET", &format!("http://127.0.0.1:{port}/"), &[], &[])
        .is_err());
    // Not a URL it can request
    assert_eq!(
        local()
            .request("GET", "gopher://x/", &[], &[])
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn plain_http_through_a_proxy_uses_the_absolute_target_and_its_credentials() {
    let (port, got) = server(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nvia");
    let e = Egress {
        http_proxy: Some(Proxy {
            host: "127.0.0.1".into(),
            port,
            auth: Some("Basic dTpw".into()),
        }),
        ..Egress::default()
    };
    // The target is a name only the proxy resolves: the host never looks it up
    let r = e
        .request("GET", "http://only-the-proxy-knows.example/p?q", &[], &[])
        .unwrap();
    assert_eq!(body(r), "via");
    let req = got.recv().unwrap();
    assert!(
        req.starts_with("GET http://only-the-proxy-knows.example/p?q HTTP/1.1\r\n"),
        "{req}"
    );
    assert!(req.contains("Proxy-Authorization: Basic dTpw\r\n"), "{req}");
}

/// A proxy on loopback that answers one CONNECT with `status`, then (on 200) echoes the tunnel.
fn connect_proxy(status: &'static str) -> (u16, mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let head = read_request(&mut s);
        let _ = tx.send(head);
        if status.is_empty() {
            return; // hang up
        }
        s.write_all(format!("HTTP/1.1 {status}\r\nX-Proxy: 1\r\n\r\n").as_bytes())
            .unwrap();
        let mut b = [0u8; 64];
        while let Ok(n) = s.read(&mut b) {
            if n == 0 || s.write_all(&b[..n]).is_err() {
                break;
            }
        }
    });
    (port, rx)
}

#[test]
fn tcp_through_a_proxy_is_a_connect_tunnel() {
    let (port, got) = connect_proxy("200 Connection established");
    let e = Egress {
        https_proxy: Some(Proxy {
            host: "127.0.0.1".into(),
            port,
            auth: Some("Basic YTpi".into()),
        }),
        ..Egress::default()
    };
    // A private destination is the proxy's business: the host does not check it
    let mut s = e.connect("10.1.2.3", 5432).unwrap();
    let head = got.recv().unwrap();
    assert!(
        head.starts_with("CONNECT 10.1.2.3:5432 HTTP/1.1\r\nHost: 10.1.2.3:5432\r\n"),
        "{head}"
    );
    assert!(head.contains("Proxy-Authorization: Basic YTpi\r\n"));
    s.write_all(b"through").unwrap();
    let mut b = [0u8; 7];
    s.read_exact(&mut b).unwrap();
    assert_eq!(
        &b, b"through",
        "bytes after the proxy's answer are the tunnel's"
    );
    // IPv6 destinations are bracketed
    let (port, got) = connect_proxy("200 OK");
    let e = Egress {
        https_proxy: Some(Proxy {
            host: "127.0.0.1".into(),
            port,
            auth: None,
        }),
        ..Egress::default()
    };
    e.connect("2001:db8::1", 443).unwrap();
    assert!(got
        .recv()
        .unwrap()
        .starts_with("CONNECT [2001:db8::1]:443 HTTP/1.1\r\n"));
}

#[test]
fn a_proxy_that_refuses_or_hangs_up_is_an_error_naming_it() {
    let (port, _) = connect_proxy("407 Proxy Authentication Required");
    let e = Egress {
        https_proxy: Some(Proxy {
            host: "127.0.0.1".into(),
            port,
            auth: None,
        }),
        ..Egress::default()
    };
    let err = e.connect("example.com", 443).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("407"), "{err}");
    let (port, _) = connect_proxy("");
    let e = Egress {
        https_proxy: Some(Proxy {
            host: "127.0.0.1".into(),
            port,
            auth: None,
        }),
        ..Egress::default()
    };
    assert_eq!(
        e.connect("example.com", 443).unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}

#[test]
fn websockets_open_with_the_servers_subprotocol_or_fail_with_its_answer() {
    let (port, got) = server(
        b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Protocol: chat\r\n\r\n\x81\x02hi",
    );
    let (protocol, mut r) = local()
        .websocket(
            &format!("ws://127.0.0.1:{port}/sock?t=1"),
            &["chat".into(), "x".into()],
            "a2V5",
        )
        .unwrap();
    assert_eq!(protocol, "chat");
    let mut frame = [0u8; 4];
    r.read_exact(&mut frame).unwrap();
    assert_eq!(&frame, b"\x81\x02hi", "what follows the 101 is the stream");
    let req = got.recv().unwrap();
    assert!(req.starts_with("GET /sock?t=1 HTTP/1.1\r\n"), "{req}");
    for h in [
        "Upgrade: websocket",
        "Sec-WebSocket-Key: a2V5",
        "Sec-WebSocket-Version: 13",
        "Sec-WebSocket-Protocol: chat, x",
    ] {
        assert!(req.contains(h), "{h}:\n{req}");
    }
    let (port, _) = server(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
    let Err(err) = local().websocket(&format!("ws://127.0.0.1:{port}/"), &[], "a2V5") else {
        panic!("opened")
    };
    assert!(err.to_string().contains("403 Forbidden"), "{err}");
}
