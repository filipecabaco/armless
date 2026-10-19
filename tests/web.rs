//! The web relay against a real TLS client (rustls standing in for the guest's), with the CA
//! made the way snowglobe makes it (tests/fixtures: test-only keys).

use std::io::{Read, Write};
use std::sync::Arc;

use armless::dev::web::cert::TlsKeys;
use armless::dev::web::{Out, Rng, Web};
use armless::dev::Event;

fn keys() -> TlsKeys {
    TlsKeys::from_json(include_bytes!("fixtures/tls.json")).unwrap()
}

fn client(
    host: &str,
    groups: Option<&'static dyn rustls::crypto::SupportedKxGroup>,
) -> rustls::ClientConnection {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(
            include_bytes!("fixtures/ca.der").to_vec(),
        ))
        .unwrap();
    let mut provider = rustls::crypto::ring::default_provider();
    if let Some(g) = groups {
        provider.kx_groups = vec![g];
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    rustls::ClientConnection::new(Arc::new(config), host.to_string().try_into().unwrap()).unwrap()
}

struct Pair {
    client: rustls::ClientConnection,
    web: Web,
    keys: TlsKeys,
    rng: Rng,
    events: Vec<Event>,
    plain: Vec<u8>,
    closed: bool,
}

impl Pair {
    fn new(host: &str, groups: Option<&'static dyn rustls::crypto::SupportedKxGroup>) -> Pair {
        let mut rng = Rng::default();
        rng.seed(b"test");
        Pair {
            client: client(host, groups),
            web: Web::new(1, true, host.into()),
            keys: keys(),
            rng,
            events: Vec::new(),
            plain: Vec::new(),
            closed: false,
        }
    }

    /// Bytes from the relay to the client.
    fn take(&mut self, out: Out) {
        self.events.extend(out.events);
        let mut data = &out.to_guest[..];
        let mut buf = [0u8; 65536];
        while !data.is_empty() {
            self.client.read_tls(&mut data).unwrap();
            self.client.process_new_packets().unwrap();
            loop {
                match self.client.reader().read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => self.plain.extend_from_slice(&buf[..n]),
                    Err(_) => break,
                }
            }
        }
        self.closed |= out.close;
    }

    /// Run the conversation until nothing moves.
    fn pump(&mut self) {
        for _ in 0..20 {
            let mut wire = Vec::new();
            while self.client.wants_write() {
                self.client.write_tls(&mut wire).unwrap();
            }
            if wire.is_empty() {
                return;
            }
            let out = self
                .web
                .from_guest(&wire, Some(&mut self.keys), &mut self.rng);
            self.take(out);
        }
    }
}

#[test]
fn https_request_both_key_shares() {
    for group in [
        rustls::crypto::ring::kx_group::X25519,
        rustls::crypto::ring::kx_group::SECP256R1,
    ] {
        let mut p = Pair::new("example.com", Some(group));
        p.client
            .writer()
            .write_all(b"POST /api?x=1 HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nUser-Agent: t\r\nConnection: close\r\n\r\nhello")
            .unwrap();
        p.pump();
        let Some(Event::HttpRequest {
            method,
            url,
            headers,
            body,
            ..
        }) = p.events.pop()
        else {
            panic!("no request: {:?}", p.events)
        };
        assert_eq!(method, "POST");
        assert_eq!(url, "https://example.com/api?x=1");
        assert_eq!(body, b"hello");
        assert!(headers.contains(&("user-agent".into(), "t".into())));
        assert!(!headers
            .iter()
            .any(|(k, _)| k == "host" || k == "content-length"));

        let out = p.web.respond(
            201,
            "Created",
            &[
                ("Content-Type".into(), "text/plain".into()),
                ("Content-Length".into(), "999".into()),
            ],
        );
        p.take(out);
        let big = vec![b'x'; 100_000]; // several TLS records
        let out = p.web.body(&big);
        p.take(out);
        let out = p.web.end();
        p.take(out);
        let text = String::from_utf8_lossy(&p.plain).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 201 Created\r\n"),
            "{}",
            &text[..80.min(text.len())]
        );
        assert!(text.contains("Content-Type: text/plain\r\n"));
        assert!(
            !text.contains("999"),
            "a body that ends with the connection has no Content-Length"
        );
        assert!(text.contains("connection: close\r\n\r\n"));
        assert_eq!(text.split("\r\n\r\n").nth(1).unwrap().len(), 100_000);
        assert!(p.closed);
    }
}

#[test]
fn websocket_inside_tls() {
    let mut p = Pair::new("ws.example.org", None);
    p.client
        .writer()
        .write_all(
            b"GET /socket HTTP/1.1\r\nHost: ws.example.org\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
              Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: chat, x\r\n\r\n",
        )
        .unwrap();
    p.pump();
    let Some(Event::WsOpen { url, protocols, .. }) = p.events.pop() else {
        panic!("{:?}", p.events)
    };
    assert_eq!(url, "wss://ws.example.org/socket");
    assert_eq!(protocols, ["chat", "x"]);
    let out = p.web.ws_accept("chat");
    p.take(out);
    let text = String::from_utf8_lossy(&p.plain).into_owned();
    // RFC 6455's example key
    assert!(
        text.contains("sec-websocket-accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"),
        "{text}"
    );
    assert!(text.contains("sec-websocket-protocol: chat\r\n"));
    p.plain.clear();

    // A masked text frame from the guest, in two fragments
    let mask = [1u8, 2, 3, 4];
    let frame = |fin: bool, op: u8, data: &[u8]| {
        let mut f = vec![(fin as u8) << 7 | op, 0x80 | data.len() as u8];
        f.extend_from_slice(&mask);
        f.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        f
    };
    p.client
        .writer()
        .write_all(&frame(false, 1, b"hel"))
        .unwrap();
    p.client.writer().write_all(&frame(true, 0, b"lo")).unwrap();
    p.pump();
    assert!(
        matches!(p.events.pop(), Some(Event::WsMessage { text: true, ref data, .. }) if data == b"hello")
    );

    let out = p.web.ws_send(false, &[9, 8, 7]);
    p.take(out);
    assert_eq!(p.plain, [0x82, 3, 9, 8, 7]);
    p.plain.clear();

    p.client
        .writer()
        .write_all(&frame(true, 8, &1000u16.to_be_bytes()))
        .unwrap();
    p.pump();
    assert!(matches!(
        p.events.pop(),
        Some(Event::WsClose { code: 1000, .. })
    ));
    assert!(p.closed);
}

#[test]
fn plain_http_chunked_and_expect() {
    let mut web = Web::new(7, false, "fallback.test".into());
    let mut rng = Rng::default();
    let out = web.from_guest(
        b"PUT /up HTTP/1.1\r\nHost: files.test:8080\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n",
        None,
        &mut rng,
    );
    assert_eq!(out.to_guest, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert!(out.events.is_empty());
    let out = web.from_guest(b"3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n", None, &mut rng);
    match &out.events[..] {
        [Event::HttpRequest { url, body, .. }] => {
            assert_eq!(url, "http://files.test:8080/up");
            assert_eq!(body, b"abcde");
        },
        e => panic!("{e:?}"),
    }
    let out = web.fail("no route");
    let text = String::from_utf8_lossy(&out.to_guest);
    assert!(text.starts_with("HTTP/1.1 502 Bad Gateway"), "{text}");
    assert!(out.close);
}

#[test]
fn no_sni_is_refused() {
    // An IP literal: clients send no server name, and there is no name to certify
    let mut p = Pair::new("example.com", None);
    let mut c = client("example.com", None);
    std::mem::swap(&mut p.client, &mut c);
    let mut wire = Vec::new();
    // A ClientHello without SNI: rustls can't make one, so hand-craft the check through the
    // parser instead: an empty record stream must not panic or produce events
    let out = p.web.from_guest(&wire, Some(&mut p.keys), &mut p.rng);
    assert!(out.events.is_empty());
    wire.extend_from_slice(&[22, 3, 1, 0, 4, 1, 0, 0, 0]);
    let out = p.web.from_guest(&wire, Some(&mut p.keys), &mut p.rng);
    assert!(out.close, "a malformed hello fails the connection");
}

// ------------------------------------------------------------ keep-alive

fn h(k: &str, v: &str) -> (String, String) {
    (k.into(), v.into())
}

/// The requests a step produced: (method, url, body).
fn requests(out: &Out) -> Vec<(String, String, Vec<u8>)> {
    out.events
        .iter()
        .filter_map(|e| match e {
            Event::HttpRequest {
                method, url, body, ..
            } => Some((method.clone(), url.clone(), body.clone())),
            _ => None,
        })
        .collect()
}

/// Answer the request in flight: the bytes the guest gets, and the last step's output.
fn answer(
    web: &mut Web,
    status: u16,
    headers: &[(String, String)],
    body: &[&[u8]],
) -> (String, Out) {
    let mut text = Vec::new();
    let o = web.respond(status, "", headers);
    text.extend_from_slice(&o.to_guest);
    let mut close = o.close;
    for b in body {
        let o = web.body(b);
        text.extend_from_slice(&o.to_guest);
        close |= o.close;
    }
    let mut o = web.end();
    text.extend_from_slice(&o.to_guest);
    o.close |= close;
    (String::from_utf8_lossy(&text).into_owned(), o)
}

fn get(path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: a.test\r\n\r\n").into_bytes()
}

#[test]
fn keep_alive_content_length_and_chunked() {
    let mut web = Web::new(3, false, "a.test".into());
    let mut rng = Rng::default();
    let out = web.from_guest(&get("/one"), None, &mut rng);
    assert_eq!(
        requests(&out),
        [("GET".into(), "http://a.test/one".into(), vec![])]
    );

    // The host's Content-Length frames the body; its Connection is not ours
    let (text, out) = answer(
        &mut web,
        200,
        &[h("Content-Length", "5"), h("Connection", "close")],
        &[b"he", b"llo"],
    );
    assert_eq!(text, "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello");
    assert!(!out.close);

    let out = web.from_guest(&get("/two"), None, &mut rng);
    assert_eq!(
        requests(&out),
        [("GET".into(), "http://a.test/two".into(), vec![])]
    );

    // No length (fetch() hides it): chunked, the host's transfer headers dropped
    let (text, out) = answer(
        &mut web,
        404,
        &[h("Transfer-Encoding", "chunked"), h("X-A", "1")],
        &[b"abc", b"", b"defgh"],
    );
    assert_eq!(
        text,
        "HTTP/1.1 404 OK\r\nX-A: 1\r\ntransfer-encoding: chunked\r\n\r\n3\r\nabc\r\n5\r\ndefgh\r\n0\r\n\r\n"
    );
    assert!(!out.close);

    // Transfer-Encoding overrides a Content-Length next to it
    web.from_guest(&get("/three"), None, &mut rng);
    let (text, _) = answer(
        &mut web,
        200,
        &[h("Content-Length", "2"), h("Transfer-Encoding", "chunked")],
        &[b"xyz"],
    );
    assert!(
        text.contains("transfer-encoding: chunked\r\n") && !text.contains("content-length"),
        "{text}"
    );
    assert!(text.ends_with("3\r\nxyz\r\n0\r\n\r\n"), "{text}");

    // The guest closes its idle connection
    assert!(web.guest_eof().close);
}

#[test]
fn keep_alive_no_body() {
    let mut web = Web::new(3, false, "a.test".into());
    let mut rng = Rng::default();
    web.from_guest(b"HEAD /f HTTP/1.1\r\nHost: a.test\r\n\r\n", None, &mut rng);
    // HEAD keeps the length the GET would have, and has no body
    let (text, out) = answer(&mut web, 200, &[h("Content-Length", "1234")], &[b"ignored"]);
    assert_eq!(text, "HTTP/1.1 200 OK\r\ncontent-length: 1234\r\n\r\n");
    assert!(!out.close);

    for status in [204, 304] {
        web.from_guest(&get("/n"), None, &mut rng);
        let (text, out) = answer(
            &mut web,
            status,
            &[h("Content-Length", "0"), h("ETag", "x")],
            &[],
        );
        assert_eq!(text, format!("HTTP/1.1 {status} OK\r\nETag: x\r\n\r\n"));
        assert!(!out.close);
    }
}

#[test]
fn connection_close_and_http10_end_with_the_connection() {
    let mut rng = Rng::default();
    for req in [
        &b"GET / HTTP/1.1\r\nHost: a.test\r\nConnection: keep-alive, close\r\n\r\n"[..],
        &b"GET / HTTP/1.0\r\nHost: a.test\r\n\r\n"[..],
        &b"GET / HTTP/1.0\r\nHost: a.test\r\nConnection: keep-alive\r\n\r\n"[..],
    ] {
        let mut web = Web::new(3, false, "a.test".into());
        let out = web.from_guest(req, None, &mut rng);
        assert_eq!(requests(&out).len(), 1);
        let (text, out) = answer(&mut web, 200, &[h("Content-Length", "3")], &[b"abc"]);
        assert_eq!(text, "HTTP/1.1 200 OK\r\nconnection: close\r\n\r\nabc");
        assert!(out.close);
    }
}

#[test]
fn pipelined_requests_wait_their_turn() {
    let mut web = Web::new(3, false, "a.test".into());
    let mut rng = Rng::default();
    // A chunked upload with a trailer, then two more requests, all in one write
    let out = web.from_guest(
        b"POST /up HTTP/1.1\r\nHost: a.test\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Trailer: t\r\n\r\n\
          GET /b HTTP/1.1\r\nHost: a.test\r\n\r\n\
          PUT /c HTTP/1.1\r\nHost: a.test\r\nContent-Length: 2\r\n\r\nhi",
        None,
        &mut rng,
    );
    assert_eq!(
        requests(&out),
        [("POST".into(), "http://a.test/up".into(), b"abc".to_vec())]
    );

    let (_, out) = answer(&mut web, 200, &[h("Content-Length", "1")], &[b"1"]);
    assert_eq!(
        requests(&out),
        [("GET".into(), "http://a.test/b".into(), vec![])]
    );
    let (_, out) = answer(&mut web, 200, &[], &[b"2"]);
    assert_eq!(
        requests(&out),
        [("PUT".into(), "http://a.test/c".into(), b"hi".to_vec())]
    );
    let (_, out) = answer(&mut web, 200, &[], &[b"3"]);
    assert!(requests(&out).is_empty() && !out.close);
}

#[test]
fn chunked_upload_then_another_request() {
    let mut web = Web::new(3, false, "a.test".into());
    let mut rng = Rng::default();
    let out = web.from_guest(
        b"POST /up HTTP/1.1\r\nHost: a.test\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n",
        None,
        &mut rng,
    );
    assert!(
        requests(&out).is_empty(),
        "the chunked body ends with an empty line"
    );
    let out = web.from_guest(b"\r\n", None, &mut rng);
    assert_eq!(requests(&out).len(), 1);
    answer(&mut web, 200, &[], &[b"ok"]);
    let out = web.from_guest(&get("/next"), None, &mut rng);
    assert_eq!(
        requests(&out),
        [("GET".into(), "http://a.test/next".into(), vec![])]
    );
}

#[test]
fn guest_done_sending_closes_after_its_last_response() {
    let mut web = Web::new(3, false, "a.test".into());
    let mut rng = Rng::default();
    let mut two = get("/a");
    two.extend_from_slice(&get("/b"));
    web.from_guest(&two, None, &mut rng);
    // Like `printf ... | nc`: the guest half-closes, and waits for the end
    assert!(!web.guest_eof().close);
    let (_, out) = answer(&mut web, 200, &[], &[b"a"]);
    assert_eq!(
        requests(&out).len(),
        1,
        "the buffered request still goes out"
    );
    assert!(!out.close);
    let (_, out) = answer(&mut web, 200, &[], &[b"b"]);
    assert!(out.close);
}

#[test]
fn wrong_length_from_the_host_closes() {
    let mut rng = Rng::default();
    // Short: the guest sees the connection end early
    let mut web = Web::new(3, false, "a.test".into());
    web.from_guest(&get("/"), None, &mut rng);
    let (text, out) = answer(&mut web, 200, &[h("Content-Length", "10")], &[b"abc"]);
    assert!(text.ends_with("\r\n\r\nabc"));
    assert!(out.close);
    // Long: cut at the promised length
    let mut web = Web::new(3, false, "a.test".into());
    web.from_guest(&get("/"), None, &mut rng);
    let (text, out) = answer(&mut web, 200, &[h("Content-Length", "2")], &[b"abc"]);
    assert!(text.ends_with("\r\n\r\nab"), "{text}");
    assert!(out.close);
    // A failure mid-body
    let mut web = Web::new(3, false, "a.test".into());
    web.from_guest(&get("/"), None, &mut rng);
    web.respond(200, "", &[]);
    web.body(b"abc");
    assert!(web.fail("the transfer broke").close);
}

#[test]
fn https_keep_alive() {
    let mut p = Pair::new("example.com", None);
    for i in 0..3u8 {
        p.client
            .writer()
            .write_all(format!("GET /{i} HTTP/1.1\r\nHost: example.com\r\n\r\n").as_bytes())
            .unwrap();
        p.pump();
        let Some(Event::HttpRequest { url, .. }) = p.events.pop() else {
            panic!("request {i}: {:?}", p.events)
        };
        assert_eq!(url, format!("https://example.com/{i}"));
        p.plain.clear();
        let out = p.web.respond(200, "OK", &[]);
        p.take(out);
        let out = p.web.body(&vec![b'a' + i; 40_000]); // several TLS records
        p.take(out);
        let out = p.web.end();
        p.take(out);
        let text = String::from_utf8_lossy(&p.plain).into_owned();
        assert!(
            text.starts_with("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n9c40\r\n"),
            "{}",
            &text[..60.min(text.len())]
        );
        assert!(text.ends_with("\r\n0\r\n\r\n"));
        assert!(!p.closed, "request {i} closed the connection");
    }
}
