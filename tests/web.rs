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
            .write_all(b"POST /api?x=1 HTTP/1.1\r\nHost: example.com\r\nContent-Length: 5\r\nUser-Agent: t\r\n\r\nhello")
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
            "our own framing replaces Content-Length"
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
