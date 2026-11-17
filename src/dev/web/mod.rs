//! The web relay: the guest's connections to ports 80 and 443 end here, inside the machine.
//! HTTPS is terminated with certificates from the site's own CA (which the guest trusts), the
//! HTTP inside is parsed, and the host only ever sees what to do — an HTTP request to make
//! (`Event::HttpRequest`) or a WebSocket to open (`Event::WsOpen`) — whatever it can reach the
//! outside with: a browser's fetch() and WebSocket, or a native HTTP client behind a proxy.
//!
//! HTTP/1.1 connections stay open: requests are answered one after another (pipelined ones
//! wait their turn), each response framed by the host's Content-Length or chunked. HTTP/1.0,
//! `connection: close` and responses that can't be framed end with the connection.

pub mod cert;
pub mod tls;
pub(crate) mod ws;

pub use ws::MAX_MESSAGE as MAX_WS_MESSAGE;

use sha2::{Digest, Sha256};

use super::Event;
use cert::TlsKeys;
use tls::Tls;

/// A deterministic random generator the host seeds (no OS randomness inside the machine).
pub struct Rng {
    key: [u8; 32],
    counter: u64,
}

impl Default for Rng {
    fn default() -> Self {
        Rng {
            key: [0x5a; 32],
            counter: 0,
        }
    }
}

impl Rng {
    pub fn seed(&mut self, entropy: &[u8]) {
        self.key = Sha256::new()
            .chain_update(self.key)
            .chain_update(entropy)
            .finalize()
            .into();
    }

    pub fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(32) {
            let block: [u8; 32] = Sha256::new()
                .chain_update(self.key)
                .chain_update(self.counter.to_le_bytes())
                .finalize()
                .into();
            self.counter += 1;
            chunk.copy_from_slice(&block[..chunk.len()]);
        }
        // Forward secrecy: nothing derived so far can be recomputed from the new key
        self.key = Sha256::new()
            .chain_update(self.key)
            .chain_update(b"ratchet")
            .finalize()
            .into();
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The largest request body the relay takes (the host needs it whole before it makes the
/// request): beyond it the guest gets 413.
pub const MAX_BODY: usize = 64 << 20;
/// What a request's head may take before it is refused (431).
const MAX_HEAD: usize = 1 << 16;
/// The most the guest may have buffered while the relay can't use it (a request it is answering,
/// a WebSocket being opened): a body and a head more is pipelining, past that it is a flood.
const MAX_PENDING: usize = MAX_BODY + MAX_HEAD;

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Headers about the connection or the message's framing (RFC 9110 7.6.1), which each hop sets
/// for itself: never passed on, either way. `name` is lowercase.
pub(crate) fn per_hop(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-connection"
            | "transfer-encoding"
            | "content-length"
            | "upgrade"
            | "te"
            | "trailer"
    )
}

/// Request headers not passed on: those, and the ones the next hop makes itself (the host, from
/// the URL; whether to wait for 100 Continue). `name` is lowercase.
pub(crate) fn not_forwarded(name: &str) -> bool {
    per_hop(name) || matches!(name, "host" | "expect")
}

struct Head {
    method: String,
    target: String,
    /// HTTP/1.1 or later (anything else is HTTP/1.0)
    http11: bool,
    headers: Vec<(String, String)>,
    body_start: usize,
}

impl Head {
    fn get(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The client wants the connection to stay open after this request.
    fn keep_alive(&self) -> bool {
        self.http11
            && !self
                .get("connection")
                .is_some_and(|c| c.split(',').any(|t| t.trim().eq_ignore_ascii_case("close")))
    }
}

/// A token (RFC 9110 5.6.2): what a method and a header name are made of.
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn parse_head(b: &[u8]) -> Option<Result<Head, ()>> {
    let end = find(b, b"\r\n\r\n")?;
    // A line ends with CRLF and holds no other control character but tab: a bare CR or LF would
    // end a line for the next hop (a proxy, a server) that it doesn't end here, so what the guest
    // put after it would become a header or a request of its own there
    let head = &b[..end];
    for (i, &c) in head.iter().enumerate() {
        let ok = match c {
            b'\r' => head.get(i + 1) == Some(&b'\n'),
            b'\n' => i > 0 && head[i - 1] == b'\r',
            b'\t' => true,
            0..=0x1f | 0x7f => false,
            _ => true,
        };
        if !ok {
            return Some(Err(()));
        }
    }
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Some(Err(()));
    };
    if !token(method) || target.is_empty() || target.contains('\t') {
        return Some(Err(()));
    }
    let version = parts.next();
    if parts.next().is_some() || version.is_some_and(|v| !v.starts_with("HTTP/")) {
        return Some(Err(()));
    }
    let http11 = version
        .and_then(|v| v.strip_prefix("HTTP/"))
        .is_some_and(|v| v != "1.0" && v != "0.9");
    let mut headers = Vec::new();
    for line in lines {
        // No folded lines (obsolete, RFC 9112 5.2), and a name that is a token, with no space
        // before its colon
        let Some((k, v)) = line.split_once(':') else {
            return Some(Err(()));
        };
        if !token(k) {
            return Some(Err(()));
        }
        headers.push((k.to_ascii_lowercase(), v.trim_matches([' ', '\t']).to_string()));
    }
    Some(Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        http11,
        headers,
        body_start: end + 4,
    }))
}

/// How far a chunked body has been read: its chunks so far, and where the next chunk's size
/// line starts (relative to the body). Kept between calls, so each byte is looked at once.
#[derive(Default)]
struct Chunks {
    at: usize,
    out: Vec<u8>,
}

/// Why a request can't be taken.
#[derive(Debug, PartialEq)]
enum BadBody {
    /// Unparseable framing (400)
    Malformed,
    /// More than MAX_BODY (413)
    TooLarge,
}

/// The whole body once it is in, and where the request ends: Ok(None) while it is still
/// arriving.
fn body(head: &Head, b: &[u8], chunks: &mut Chunks) -> Result<Option<(Vec<u8>, usize)>, BadBody> {
    let body = &b[head.body_start..];
    if let Some(te) = head.get("transfer-encoding") {
        // Chunked is the only coding a request may end with that the relay can frame
        if !te.to_ascii_lowercase().split(',').next_back().is_some_and(|t| t.trim() == "chunked") {
            return Err(BadBody::Malformed);
        }
        loop {
            let at = chunks.at;
            let Some(eol) = find(&body[at..], b"\r\n") else {
                return if body.len() - at > 4096 { Err(BadBody::Malformed) } else { Ok(None) };
            };
            let line = String::from_utf8_lossy(&body[at..at + eol]);
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| BadBody::Malformed)?;
            let data = at + eol + 2;
            if size == 0 {
                // Trailers (ignored), then an empty line
                let end = if body[data..].starts_with(b"\r\n") {
                    data + 2
                } else {
                    match find(&body[data..], b"\r\n\r\n") {
                        Some(e) => data + e + 4,
                        None if body.len() - data > MAX_HEAD => return Err(BadBody::Malformed),
                        None => return Ok(None),
                    }
                };
                return Ok(Some((std::mem::take(&mut chunks.out), head.body_start + end)));
            }
            if chunks.out.len().saturating_add(size) > MAX_BODY {
                return Err(BadBody::TooLarge);
            }
            let next = data + size + 2;
            if body.len() < next {
                return Ok(None);
            }
            if &body[data + size..next] != b"\r\n" {
                return Err(BadBody::Malformed);
            }
            chunks.out.extend_from_slice(&body[data..data + size]);
            chunks.at = next;
        }
    }
    // One length, or the same one repeated (RFC 9112 6.3): anything else can't frame the request
    let mut lengths = head.headers.iter().filter(|(k, _)| k == "content-length");
    let n = match lengths.next() {
        None => 0,
        Some((_, v)) => {
            if v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit()) {
                return Err(BadBody::Malformed);
            }
            let n: usize = v.parse().map_err(|_| BadBody::TooLarge)?;
            if lengths.any(|(_, w)| w != v) {
                return Err(BadBody::Malformed);
            }
            n
        },
    };
    if n > MAX_BODY {
        return Err(BadBody::TooLarge);
    }
    Ok((body.len() >= n).then(|| (body[..n].to_vec(), head.body_start + n)))
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Phase {
    /// Reading the request
    Request,
    /// Waiting for the host to answer
    Waiting,
    /// Sending the response body
    Responding,
    /// Waiting for the host's WebSocket
    WsOpening,
    WebSocket,
    Done,
}

/// How the response body is delimited for the guest.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Framing {
    /// No body (HEAD, 204, 304)
    None,
    /// Content-Length: what is left of it
    Length(u64),
    Chunked,
    /// The body ends with the connection
    Close,
}

/// What to do after a step.
#[derive(Default)]
pub struct Out {
    /// Bytes for the guest
    pub to_guest: Vec<u8>,
    pub events: Vec<Event>,
    /// Close the connection after sending
    pub close: bool,
}

/// One relayed connection.
pub struct Web {
    id: u32,
    https: bool,
    /// The name the guest connected to (from its DNS lookup)
    host: String,
    tls: Option<Tls>,
    phase: Phase,
    buf: Vec<u8>,
    head_only: bool,
    /// The connection stays open after this response
    keep_alive: bool,
    framing: Framing,
    /// The guest has finished sending: no request after the ones buffered
    guest_done: bool,
    ws_messages: ws::Messages,
    ws_key: String,
    /// "100 Continue" was sent
    continued: bool,
    /// The request's chunked body, as far as it has been read
    chunks: Chunks,
}

impl Web {
    pub fn new(id: u32, https: bool, host: String) -> Web {
        Web {
            id,
            https,
            host,
            tls: https.then(Tls::default),
            phase: Phase::Request,
            buf: Vec::new(),
            head_only: false,
            keep_alive: false,
            framing: Framing::Close,
            guest_done: false,
            ws_messages: ws::Messages::default(),
            ws_key: String::new(),
            continued: false,
            chunks: Chunks::default(),
        }
    }

    /// Plaintext for the guest (encrypted if this is HTTPS).
    fn send(&mut self, data: &[u8], out: &mut Out) {
        match &mut self.tls {
            Some(t) => t.send(data, &mut out.to_guest),
            None => out.to_guest.extend_from_slice(data),
        }
    }

    /// The response is complete: on to the guest's next request, or close.
    fn next(&mut self, out: &mut Out) {
        if !self.keep_alive {
            return self.finish(out);
        }
        self.phase = Phase::Request;
        self.head_only = false;
        self.continued = false;
        self.chunks = Chunks::default();
        self.request(out);
        if self.phase == Phase::Request && self.guest_done {
            self.finish(out);
        }
    }

    fn finish(&mut self, out: &mut Out) {
        if let Some(t) = &mut self.tls {
            t.close(&mut out.to_guest);
        }
        self.phase = Phase::Done;
        out.close = true;
    }

    fn respond_error(&mut self, status: u16, reason: &str, message: &str, out: &mut Out) {
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            message.len()
        );
        self.send(head.as_bytes(), out);
        self.send(message.as_bytes(), out);
        self.finish(out);
    }

    /// Bytes from the guest.
    pub fn from_guest(&mut self, data: &[u8], keys: Option<&mut TlsKeys>, rng: &mut Rng) -> Out {
        let mut out = Out::default();
        let plain = match &mut self.tls {
            Some(t) => {
                let r = t.receive(data, keys, rng);
                out.to_guest.extend_from_slice(&r.out);
                if r.closed {
                    self.phase = Phase::Done;
                    out.close = true;
                    return out;
                }
                r.data
            },
            None => data.to_vec(),
        };
        if plain.is_empty() {
            return out;
        }
        self.buf.extend_from_slice(&plain);
        match self.phase {
            Phase::Request => self.request(&mut out),
            Phase::WebSocket => self.frames(&mut out),
            // Anything else the guest sends while we answer (a pipelined request, or WebSocket
            // frames before the socket opened) waits in the buffer, up to a limit
            Phase::Waiting | Phase::Responding | Phase::WsOpening => {
                if self.buf.len() > MAX_PENDING {
                    if self.phase == Phase::WsOpening {
                        out.events.push(Event::WsClose { id: self.id, code: ws::TOO_BIG });
                    }
                    self.buf = Vec::new();
                    self.finish(&mut out);
                }
            },
            Phase::Done => self.buf.clear(),
        }
        out
    }

    /// The guest closed its side.
    pub fn guest_eof(&mut self) -> Out {
        let mut out = Out::default();
        self.guest_done = true;
        if matches!(
            self.phase,
            Phase::Request | Phase::WsOpening | Phase::WebSocket
        ) {
            if self.phase == Phase::WebSocket {
                out.events.push(Event::WsClose {
                    id: self.id,
                    code: 1006,
                });
            }
            self.finish(&mut out);
        }
        out
    }

    fn url(&self, head: &Head, scheme: &str) -> String {
        if head.target.starts_with("http://") || head.target.starts_with("https://") {
            return head.target.clone();
        }
        // Plain HTTP names its host (and port) in the request; TLS named it in the handshake
        let host = match (&self.tls, head.get("host")) {
            (Some(t), _) if t.host.is_some() => t.host.clone().unwrap(),
            (None, Some(h)) if !h.is_empty() => h.to_string(),
            _ => self.host.clone(),
        };
        format!("{scheme}://{host}{}", head.target)
    }

    fn request(&mut self, out: &mut Out) {
        // Empty lines before a request are allowed (RFC 9112 2.2)
        let blank = self
            .buf
            .iter()
            .take_while(|&&b| b == b'\r' || b == b'\n')
            .count();
        self.buf.drain(..blank);
        if self.buf.is_empty() {
            return;
        }
        let head = match parse_head(&self.buf) {
            None => {
                if self.buf.len() > MAX_HEAD {
                    self.respond_error(
                        431,
                        "Request Header Fields Too Large",
                        "armless: request headers too large\n",
                        out,
                    );
                }
                return;
            },
            Some(Err(())) => {
                return self.respond_error(400, "Bad Request", "armless: malformed request\n", out)
            },
            Some(Ok(h)) => h,
        };
        let websocket = head
            .get("upgrade")
            .is_some_and(|u| u.to_ascii_lowercase().contains("websocket"));
        if websocket {
            let scheme = if self.https { "wss" } else { "ws" };
            let protocols = head
                .get("sec-websocket-protocol")
                .map(|p| {
                    p.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            let url = self.url(&head, scheme);
            self.ws_key = head.get("sec-websocket-key").unwrap_or("").to_string();
            self.buf.drain(..head.body_start);
            self.phase = Phase::WsOpening;
            out.events.push(Event::WsOpen {
                id: self.id,
                url,
                protocols,
            });
            return;
        }
        let body = match body(&head, &self.buf, &mut self.chunks) {
            Err(BadBody::Malformed) => {
                return self.respond_error(
                    400,
                    "Bad Request",
                    "armless: malformed request body\n",
                    out,
                )
            },
            Err(BadBody::TooLarge) => {
                return self.respond_error(
                    413,
                    "Content Too Large",
                    "armless: request body too large\n",
                    out,
                )
            },
            Ok(None) if self.buf.len() > MAX_PENDING + MAX_BODY => {
                // A chunked body of tiny chunks: framing costs more than the body is allowed
                return self.respond_error(
                    413,
                    "Content Too Large",
                    "armless: request body too large\n",
                    out,
                );
            },
            Ok(None) => {
                // curl waits a second for this before sending a large body
                if head
                    .get("expect")
                    .is_some_and(|e| e.eq_ignore_ascii_case("100-continue"))
                    && !self.continued
                {
                    self.continued = true;
                    self.send(b"HTTP/1.1 100 Continue\r\n\r\n", out);
                }
                return;
            },
            Ok(Some(b)) => b,
        };
        let (body, end) = body;
        // A pipelined request after this one waits in the buffer
        self.buf.drain(..end);
        self.chunks = Chunks::default();
        self.head_only = head.method.eq_ignore_ascii_case("HEAD");
        self.keep_alive = head.keep_alive();
        let url = self.url(&head, if self.https { "https" } else { "http" });
        let headers = head
            .headers
            .iter()
            .filter(|(k, _)| !not_forwarded(k))
            .cloned()
            .collect();
        self.phase = Phase::Waiting;
        out.events.push(Event::HttpRequest {
            id: self.id,
            method: head.method,
            url,
            headers,
            body,
        });
    }

    // ------------------------------------------------------------ host answers

    /// The response's status and headers. The transfer's framing is ours: the host's
    /// Content-Length is kept when there is one (and no Transfer-Encoding), else the body is
    /// chunked, or ends with the connection.
    pub fn respond(&mut self, status: u16, reason: &str, headers: &[(String, String)]) -> Out {
        let mut out = Out::default();
        if self.phase != Phase::Waiting {
            return out;
        }
        let get = |name: &str| {
            headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
                .collect::<Vec<_>>()
        };
        // Any Transfer-Encoding overrides Content-Length, and conflicting lengths mean none
        let lengths: Vec<Option<u64>> = get("content-length")
            .iter()
            .map(|v| v.parse().ok())
            .collect();
        let length = match (lengths.first(), get("transfer-encoding").is_empty()) {
            (Some(&Some(n)), true) if lengths.iter().all(|&l| l == Some(n)) => Some(n),
            _ => None,
        };
        let interim = (100..200).contains(&status);
        let no_body = self.head_only || status == 204 || status == 304 || interim;
        if interim {
            // Only a final response can be followed by another
            self.keep_alive = false;
        }
        self.framing = if no_body {
            Framing::None
        } else if !self.keep_alive {
            Framing::Close
        } else if let Some(n) = length {
            Framing::Length(n)
        } else {
            Framing::Chunked
        };
        let mut head = format!(
            "HTTP/1.1 {status} {}\r\n",
            if reason.is_empty() { "OK" } else { reason }
        );
        for (k, v) in headers {
            if per_hop(&k.to_ascii_lowercase()) {
                continue;
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        match self.framing {
            // A HEAD response says how long the body would have been
            Framing::None if self.head_only && status != 204 && status != 304 => {
                if let Some(n) = length {
                    head.push_str(&format!("content-length: {n}\r\n"));
                }
            },
            Framing::Length(n) => head.push_str(&format!("content-length: {n}\r\n")),
            Framing::Chunked => head.push_str("transfer-encoding: chunked\r\n"),
            _ => {},
        }
        if !self.keep_alive {
            head.push_str("connection: close\r\n");
        }
        head.push_str("\r\n");
        self.send(head.as_bytes(), &mut out);
        self.phase = Phase::Responding;
        out
    }

    pub fn body(&mut self, data: &[u8]) -> Out {
        let mut out = Out::default();
        if self.phase != Phase::Responding || data.is_empty() {
            return out;
        }
        match self.framing {
            Framing::None => {},
            Framing::Close => self.send(data, &mut out),
            Framing::Chunked => {
                let mut chunk = format!("{:x}\r\n", data.len()).into_bytes();
                chunk.extend_from_slice(data);
                chunk.extend_from_slice(b"\r\n");
                self.send(&chunk, &mut out);
            },
            Framing::Length(left) => {
                let n = (data.len() as u64).min(left) as usize;
                self.send(&data[..n], &mut out);
                self.framing = Framing::Length(left - n as u64);
                if n < data.len() {
                    // More than the host said: the guest has what it was promised, and the
                    // connection can't carry on
                    self.finish(&mut out);
                }
            },
        }
        out
    }

    pub fn end(&mut self) -> Out {
        let mut out = Out::default();
        match (self.phase, self.framing) {
            // Less than the host said: the guest sees the connection end early
            (Phase::Responding, Framing::Length(n)) if n > 0 => self.finish(&mut out),
            (Phase::Responding, Framing::Close) => self.finish(&mut out),
            (Phase::Responding, Framing::Chunked) => {
                self.send(b"0\r\n\r\n", &mut out);
                self.next(&mut out);
            },
            (Phase::Responding, _) => self.next(&mut out),
            (Phase::Waiting, _) => self.finish(&mut out),
            _ => {},
        }
        out
    }

    /// The request could not be made: a 502 if nothing was sent yet.
    pub fn fail(&mut self, message: &str) -> Out {
        let mut out = Out::default();
        match self.phase {
            Phase::Waiting | Phase::WsOpening => self.respond_error(
                502,
                "Bad Gateway",
                &format!("armless: {message}\n"),
                &mut out,
            ),
            Phase::Responding | Phase::WebSocket => self.finish(&mut out),
            _ => {},
        }
        out
    }

    pub fn ws_accept(&mut self, protocol: &str) -> Out {
        let mut out = Out::default();
        if self.phase != Phase::WsOpening {
            return out;
        }
        let mut head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\n",
            ws::accept_key(&self.ws_key)
        );
        if !protocol.is_empty() {
            head.push_str(&format!("sec-websocket-protocol: {protocol}\r\n"));
        }
        head.push_str("\r\n");
        self.send(head.as_bytes(), &mut out);
        self.phase = Phase::WebSocket;
        // Frames the guest sent while the host's socket opened
        self.frames(&mut out);
        out
    }

    pub fn ws_send(&mut self, text: bool, data: &[u8]) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::WebSocket {
            let opcode = if text { ws::TEXT } else { ws::BINARY };
            self.send(&ws::encode(opcode, data, None), &mut out);
        }
        out
    }

    pub fn ws_close(&mut self, code: u16) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::WebSocket {
            self.send(
                &ws::encode(ws::CLOSE, &ws::close_payload(code), None),
                &mut out,
            );
            self.finish(&mut out);
        }
        out
    }

    /// The guest's frames: masked, and a message may span several.
    fn frames(&mut self, out: &mut Out) {
        while self.phase == Phase::WebSocket {
            if ws::oversized(&self.buf) {
                return self.ws_too_big(out);
            }
            let Some((frame, used)) = ws::parse(&self.buf) else {
                return;
            };
            self.buf.drain(..used);
            match frame.opcode {
                ws::CLOSE => {
                    // Close: echo it, and tell the host
                    let p = &frame.payload;
                    self.send(&ws::encode(ws::CLOSE, &p[..p.len().min(2)], None), out);
                    out.events.push(Event::WsClose {
                        id: self.id,
                        code: ws::close_code(p),
                    });
                    self.finish(out);
                    return;
                },
                // Ping: answered here; the host keeps its own connection alive
                ws::PING => self.send(&ws::encode(ws::PONG, &frame.payload, None), out),
                ws::PONG => {},
                _ => match self.ws_messages.push(frame) {
                    Ok(Some((text, data))) => out.events.push(Event::WsMessage {
                        id: self.id,
                        text,
                        data,
                    }),
                    Ok(None) => {},
                    Err(ws::TooBig) => return self.ws_too_big(out),
                },
            }
        }
    }

    /// A frame or message over ws::MAX_MESSAGE from the guest: close both sides with 1009.
    fn ws_too_big(&mut self, out: &mut Out) {
        self.buf = Vec::new();
        self.send(&ws::encode(ws::CLOSE, &ws::close_payload(ws::TOO_BIG), None), out);
        out.events.push(Event::WsClose { id: self.id, code: ws::TOO_BIG });
        self.finish(out);
    }
}
