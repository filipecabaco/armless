//! The web relay: the guest's connections to ports 80 and 443 end here, inside the machine.
//! HTTPS is terminated with certificates from the site's own CA (which the guest trusts), the
//! HTTP inside is parsed, and the host only ever sees what to do — an HTTP request to make
//! (`Event::HttpRequest`) or a WebSocket to open (`Event::WsOpen`) — whatever it can reach the
//! outside with: a browser's fetch() and WebSocket, or a native HTTP client behind a proxy.
//!
//! One request per connection (responses say `connection: close` and end with the
//! connection), streamed both ways.

pub mod cert;
pub mod tls;

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

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Request headers that describe this hop, not the request.
fn hop_by_hop(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "keep-alive"
            | "proxy-connection"
            | "transfer-encoding"
            | "content-length"
            | "upgrade"
            | "te"
            | "trailer"
            | "expect"
    )
}

struct Head {
    method: String,
    target: String,
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
}

fn parse_head(b: &[u8]) -> Option<Result<Head, ()>> {
    let end = find(b, b"\r\n\r\n")?;
    let text = String::from_utf8_lossy(&b[..end]);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Some(Err(()));
    };
    let mut headers = Vec::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            return Some(Err(()));
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    Some(Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        body_start: end + 4,
    }))
}

/// The whole body once it is in: Ok(None) while it is still arriving.
fn body(head: &Head, b: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    let body = &b[head.body_start..];
    if head
        .get("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"))
    {
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let Some(eol) = find(&body[at..], b"\r\n") else {
                return Ok(None);
            };
            let line = String::from_utf8_lossy(&body[at..at + eol]);
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| ())?;
            let data = at + eol + 2;
            if size == 0 {
                return Ok(Some(out));
            }
            if body.len() < data + size + 2 {
                return Ok(None);
            }
            out.extend_from_slice(&body[data..data + size]);
            at = data + size + 2;
        }
    }
    let n: usize = head
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Ok((body.len() >= n).then(|| body[..n].to_vec()))
}

fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut f = vec![0x80 | opcode];
    if n < 126 {
        f.push(n as u8);
    } else if n < 65536 {
        f.extend_from_slice(&[126, (n >> 8) as u8, n as u8]);
    } else {
        f.push(127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    f.extend_from_slice(payload);
    f
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
    ws_message: Vec<u8>,
    ws_text: bool,
    ws_key: String,
    /// "100 Continue" was sent
    continued: bool,
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
            ws_message: Vec::new(),
            ws_text: false,
            ws_key: String::new(),
            continued: false,
        }
    }

    /// Plaintext for the guest (encrypted if this is HTTPS).
    fn send(&mut self, data: &[u8], out: &mut Out) {
        match &mut self.tls {
            Some(t) => t.send(data, &mut out.to_guest),
            None => out.to_guest.extend_from_slice(data),
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
            // frames before the socket opened) waits in the buffer
            _ => {},
        }
        out
    }

    /// The guest closed its side.
    pub fn guest_eof(&mut self) -> Out {
        let mut out = Out::default();
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
        let head = match parse_head(&self.buf) {
            None => {
                if self.buf.len() > 1 << 16 {
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
        let body = match body(&head, &self.buf) {
            Err(()) => {
                return self.respond_error(
                    400,
                    "Bad Request",
                    "armless: malformed chunked body\n",
                    out,
                )
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
        self.buf.clear();
        self.head_only = head.method.eq_ignore_ascii_case("HEAD");
        let url = self.url(&head, if self.https { "https" } else { "http" });
        let headers = head
            .headers
            .iter()
            .filter(|(k, _)| !hop_by_hop(k))
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

    /// The response's status and headers. Lengths and encodings of the transfer are ours
    /// (the body is streamed and ends with the connection).
    pub fn respond(&mut self, status: u16, reason: &str, headers: &[(String, String)]) -> Out {
        let mut out = Out::default();
        if self.phase != Phase::Waiting {
            return out;
        }
        let mut head = format!(
            "HTTP/1.1 {status} {}\r\n",
            if reason.is_empty() { "OK" } else { reason }
        );
        for (k, v) in headers {
            let lk = k.to_ascii_lowercase();
            if matches!(
                lk.as_str(),
                "content-length" | "transfer-encoding" | "connection" | "keep-alive"
            ) {
                continue;
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("connection: close\r\n\r\n");
        self.send(head.as_bytes(), &mut out);
        self.phase = Phase::Responding;
        out
    }

    pub fn body(&mut self, data: &[u8]) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::Responding && !self.head_only {
            self.send(data, &mut out);
        }
        out
    }

    pub fn end(&mut self) -> Out {
        let mut out = Out::default();
        if matches!(self.phase, Phase::Responding | Phase::Waiting) {
            self.finish(&mut out);
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
        let accept = sha1::Sha1::new()
            .chain_update(self.ws_key.as_bytes())
            .chain_update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
            .finalize();
        let mut head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\n",
            base64_encode(&accept)
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
            self.send(&frame(if text { 1 } else { 2 }, data), &mut out);
        }
        out
    }

    pub fn ws_close(&mut self, code: u16) -> Out {
        let mut out = Out::default();
        if self.phase == Phase::WebSocket {
            let payload =
                if code == 0 || code == 1005 { vec![] } else { code.to_be_bytes().to_vec() };
            self.send(&frame(8, &payload), &mut out);
            self.finish(&mut out);
        }
        out
    }

    /// The guest's frames: masked, and a message may span several.
    fn frames(&mut self, out: &mut Out) {
        while self.phase == Phase::WebSocket && self.buf.len() >= 2 {
            let b = &self.buf;
            let mut len = (b[1] & 127) as usize;
            let mut at = 2;
            if len == 126 {
                if b.len() < 4 {
                    return;
                }
                len = u16::from_be_bytes([b[2], b[3]]) as usize;
                at = 4;
            } else if len == 127 {
                if b.len() < 10 {
                    return;
                }
                len = u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize;
                at = 10;
            }
            let masked = b[1] & 128 != 0;
            let mask_at = at;
            if masked {
                at += 4;
            }
            if b.len() < at + len {
                return;
            }
            let fin = b[0] & 0x80 != 0;
            let opcode = b[0] & 15;
            let mut payload = b[at..at + len].to_vec();
            if masked {
                let mask: [u8; 4] = b[mask_at..mask_at + 4].try_into().unwrap();
                for (i, x) in payload.iter_mut().enumerate() {
                    *x ^= mask[i & 3];
                }
            }
            self.buf.drain(..at + len);
            match opcode {
                8 => {
                    // Close: echo it, and tell the host
                    let code = if payload.len() >= 2 {
                        u16::from_be_bytes([payload[0], payload[1]])
                    } else {
                        1005
                    };
                    self.send(&frame(8, &payload[..payload.len().min(2)]), out);
                    out.events.push(Event::WsClose { id: self.id, code });
                    self.finish(out);
                    return;
                },
                // Ping: answered here; the host keeps its own connection alive
                9 => self.send(&frame(10, &payload), out),
                10 => {},
                op => {
                    if op != 0 {
                        self.ws_text = op == 1;
                        self.ws_message.clear();
                    }
                    self.ws_message.extend_from_slice(&payload);
                    if fin {
                        out.events.push(Event::WsMessage {
                            id: self.id,
                            text: self.ws_text,
                            data: std::mem::take(&mut self.ws_message),
                        });
                    }
                },
            }
        }
    }
}
