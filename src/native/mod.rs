//! Host services for native programs: answer the machine's events.
//!
//! * blobs from a directory (`<dir>/<name>`)
//! * the web relay's requests (`HttpRequest`, `WsOpen`) made with a built-in HTTP client
//! * other TCP connections the guest opens, as real connections, or through a WebSocket
//!   tunnel for the destinations [`HostOptions::tunnels`] names
//! * connections into the guest (port forwarding) from host sockets
//! * the network's retransmit timer
//!
//! Everything that leaves goes through [`Egress`]: direct, or through the proxy the
//! environment names, with private addresses off limits. With the network off the guest's
//! requests fail and its connections are refused.
//!
//! Every event is also shown to an observer first (console output, which blobs were read...).

pub mod egress;

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use egress::Egress;

use crate::dev::Event;
use crate::vm::Board;

#[derive(Clone, Debug, Default)]
pub struct HostOptions {
    /// Directory holding the filesystem's blobs.
    pub blobs: Option<PathBuf>,
    /// Let the guest reach the outside world.
    pub network: bool,
    /// How (see [`Egress::from_env`]).
    pub egress: Egress,
    /// Guest TCP destinations carried over a WebSocket instead of a direct connection.
    pub tunnels: Vec<Tunnel>,
}

/// A guest TCP destination reached through a WebSocket: each connection the guest opens to it
/// becomes one WebSocket to `url`, its bytes sent and received as binary messages (what
/// websockify, websocat and Neon's wsproxy speak). The browser host does the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    /// The name the guest connected to; `None` for any
    pub host: Option<String>,
    pub port: u16,
    /// A ws:// or wss:// URL
    pub url: String,
}

impl Tunnel {
    /// `[HOST:]PORT=URL`, e.g. `db.internal:5432=wss://proxy.example.com/db`.
    pub fn parse(spec: &str) -> Option<Tunnel> {
        let (dest, url) = spec.split_once('=')?;
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return None;
        }
        let (host, port) = match dest.rsplit_once(':') {
            Some((h, p)) if !h.is_empty() => (Some(h.to_ascii_lowercase()), p),
            Some(_) => return None,
            None => (None, dest),
        };
        Some(Tunnel {
            host,
            port: port.parse().ok().filter(|p| *p != 0)?,
            url: url.to_string(),
        })
    }

    pub fn matches(&self, host: &str, port: u16) -> bool {
        self.port == port && self.host.as_ref().is_none_or(|h| h.eq_ignore_ascii_case(host))
    }
}

pub type Observer = Box<dyn FnMut(&Event) + Send>;

enum Msg {
    Wake,
    /// A host socket to connect to the guest's port.
    Forward(TcpStream, u16),
    Quit,
}

pub struct NativeHost {
    tx: Sender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Bytes the guest has not taken yet before a sender waits.
const BACKLOG: usize = 1 << 20;

/// Bytes from the system's random source.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        // No /dev/urandom: the clock and addresses are better than nothing
        let t = Instant::now();
        let seed = format!("{:?}{:?}{:p}", std::time::SystemTime::now(), t, &b);
        for (i, c) in seed.bytes().enumerate() {
            b[i % n] ^= c;
        }
    }
    b
}

/// What the host keeps per connection.
#[derive(Default)]
struct Conns {
    /// Raw TCP: the host side's socket
    sockets: HashMap<u32, (Sender<Option<Vec<u8>>>, TcpStream)>,
    /// WebSockets: frames to send
    websockets: HashMap<u32, Sender<WsOut>>,
    /// Tunnelled TCP: the guest's bytes (`None`: it has finished sending)
    tunnels: HashMap<u32, Sender<Option<Vec<u8>>>>,
    /// Connections the guest has closed (requests in flight stop)
    closed: HashSet<u32>,
}

enum WsOut {
    Message(bool, Vec<u8>),
    Close(u16),
}

type Shared = Arc<Mutex<Conns>>;

impl NativeHost {
    pub fn spawn(board: Arc<Board>, options: HostOptions, observer: Observer) -> NativeHost {
        board.seed(&random_bytes(32));
        let (tx, rx) = channel();
        {
            let tx = tx.clone();
            board.set_notify(Box::new(move || {
                let _ = tx.send(Msg::Wake);
            }));
        }
        let thread = {
            let board = board.clone();
            let tx = tx.clone();
            std::thread::Builder::new()
                .name("armless-host".into())
                .spawn(move || event_loop(board, options, observer, rx, tx))
                .expect("spawn host thread")
        };
        NativeHost {
            tx,
            thread: Some(thread),
        }
    }

    /// Connect `stream` to the guest's TCP `port`.
    pub fn forward(&self, stream: TcpStream, port: u16) {
        let _ = self.tx.send(Msg::Forward(stream, port));
    }

    /// Look at the events again (after something was delivered elsewhere).
    pub fn wake(&self) {
        let _ = self.tx.send(Msg::Wake);
    }
}

impl Drop for NativeHost {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn event_loop(
    board: Arc<Board>,
    options: HostOptions,
    mut observer: Observer,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
) {
    let conns: Shared = Arc::default();
    let egress = Arc::new(options.egress.clone());
    // Host sockets waiting for their guest connection to come up.
    let mut forwarding: HashMap<u32, TcpStream> = HashMap::new();
    let start = Instant::now();
    let mut last_tick = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Msg::Quit) => break,
            Ok(Msg::Forward(stream, port)) => match board.tcp_connect(port) {
                Some(id) => {
                    forwarding.insert(id, stream);
                },
                None => {
                    let _ = stream.shutdown(Shutdown::Both);
                },
            },
            Ok(Msg::Wake) | Err(RecvTimeoutError::Timeout) => {},
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if last_tick.elapsed() >= Duration::from_millis(200) {
            last_tick = Instant::now();
            board.tick(start.elapsed().as_millis() as u64);
        }
        for e in board.events() {
            observer(&e);
            match e {
                Event::Blob(name) => {
                    let data = options
                        .blobs
                        .as_ref()
                        .filter(|_| !name.contains('/') && !name.contains(".."))
                        .and_then(|dir| std::fs::read(dir.join(&name)).ok());
                    match data {
                        Some(d) => {
                            if board.provide_blob(&name, d).is_err() {
                                board.blob_failed(&name);
                            }
                        },
                        None => board.blob_failed(&name),
                    }
                },
                Event::HttpRequest {
                    id,
                    method,
                    url,
                    headers,
                    body,
                } => {
                    if !options.network {
                        board.http_fail(id, "this machine has no network");
                        continue;
                    }
                    let (board, conns, egress, tx) =
                        (board.clone(), conns.clone(), egress.clone(), tx.clone());
                    std::thread::spawn(move || {
                        http(&board, &conns, &egress, id, &method, &url, &headers, &body);
                        let _ = tx.send(Msg::Wake);
                    });
                },
                Event::WsOpen { id, url, protocols } => {
                    if !options.network {
                        board.http_fail(id, "this machine has no network");
                        continue;
                    }
                    let (wtx, wrx) = channel();
                    conns.lock().unwrap().websockets.insert(id, wtx);
                    let (board, conns, egress, tx) =
                        (board.clone(), conns.clone(), egress.clone(), tx.clone());
                    std::thread::spawn(move || {
                        websocket(&board, &conns, &egress, id, &url, &protocols, wrx);
                        let _ = tx.send(Msg::Wake);
                    });
                },
                Event::WsMessage { id, text, data } => {
                    if let Some(w) = conns.lock().unwrap().websockets.get(&id) {
                        let _ = w.send(WsOut::Message(text, data));
                    }
                },
                Event::WsClose { id, code } => {
                    if let Some(w) = conns.lock().unwrap().websockets.remove(&id) {
                        let _ = w.send(WsOut::Close(code));
                    }
                },
                Event::TcpOpen { id, host, port } => {
                    if !options.network {
                        board.tcp_refuse(id);
                        continue;
                    }
                    if let Some(t) = options.tunnels.iter().find(|t| t.matches(&host, port)) {
                        let (ttx, trx) = channel();
                        conns.lock().unwrap().tunnels.insert(id, ttx);
                        let url = t.url.clone();
                        let (board, conns, egress, tx) =
                            (board.clone(), conns.clone(), egress.clone(), tx.clone());
                        std::thread::spawn(move || {
                            tunnel(&board, &conns, &egress, id, &url, trx);
                            let _ = tx.send(Msg::Wake);
                        });
                        continue;
                    }
                    let (board, conns, egress, tx) =
                        (board.clone(), conns.clone(), egress.clone(), tx.clone());
                    std::thread::spawn(move || {
                        match egress.connect(&host, port) {
                            Ok(s) => {
                                board.tcp_accept(id);
                                attach(&board, &conns, id, s);
                            },
                            Err(_) => board.tcp_refuse(id),
                        }
                        let _ = tx.send(Msg::Wake);
                    });
                },
                Event::TcpConnected { id } => {
                    if let Some(s) = forwarding.remove(&id) {
                        attach(&board, &conns, id, s);
                    }
                },
                Event::TcpData { id, data } => {
                    let c = conns.lock().unwrap();
                    if let Some(w) = c.sockets.get(&id).map(|(w, _)| w).or(c.tunnels.get(&id)) {
                        let _ = w.send(Some(data));
                    }
                },
                Event::TcpEof { id } => {
                    let c = conns.lock().unwrap();
                    if let Some(w) = c.sockets.get(&id).map(|(w, _)| w).or(c.tunnels.get(&id)) {
                        let _ = w.send(None);
                    }
                },
                Event::TcpClosed { id } => {
                    if let Some(s) = forwarding.remove(&id) {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    let mut c = conns.lock().unwrap();
                    if let Some((_, s)) = c.sockets.remove(&id) {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    c.websockets.remove(&id);
                    c.tunnels.remove(&id);
                    c.closed.insert(id);
                },
                Event::Serial(_) | Event::Console(_) => {},
            }
        }
    }
    for (_, (_, s)) in conns.lock().unwrap().sockets.drain() {
        let _ = s.shutdown(Shutdown::Both);
    }
}

fn is_closed(conns: &Shared, id: u32) -> bool {
    conns.lock().unwrap().closed.contains(&id)
}

/// Wait while the guest is behind on what we sent it.
fn backpressure(board: &Board, conns: &Shared, id: u32) {
    while board.tcp_buffered(id) > BACKLOG && !is_closed(conns, id) {
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[allow(clippy::too_many_arguments)]
fn http(
    board: &Board,
    conns: &Shared,
    egress: &Egress,
    id: u32,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) {
    match egress.request(method, url, headers, body) {
        Ok(mut r) => {
            board.http_respond(id, r.status, &r.reason, &r.headers);
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                if is_closed(conns, id) {
                    return;
                }
                match r.body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        board.http_body(id, &buf[..n]);
                        backpressure(board, conns, id);
                    },
                    Err(_) => {
                        // A broken transfer: the guest sees the connection end early
                        board.http_fail(id, "the transfer broke");
                        return;
                    },
                }
            }
            board.http_end(id);
        },
        Err(e) => board.http_fail(id, &format!("could not reach {url}: {e}")),
    }
}

fn websocket(
    board: &Board,
    conns: &Shared,
    egress: &Egress,
    id: u32,
    url: &str,
    protocols: &[String],
    out: Receiver<WsOut>,
) {
    let key = crate::dev::web::base64_encode(&random_bytes(16));
    let (protocol, reader) = match egress.websocket(url, protocols, &key) {
        Ok(x) => x,
        Err(e) => {
            conns.lock().unwrap().websockets.remove(&id);
            board.http_fail(id, &format!("could not open {url}: {e}"));
            return;
        },
    };
    board.ws_accept(id, &protocol);
    // One thread owns the stream (TLS can't be split): poll it, and send what is queued
    let mut buf = reader.buffer().to_vec();
    let mut conn = reader.into_inner();
    let _ = conn.tcp().set_read_timeout(Some(Duration::from_millis(20)));
    let mut message = Vec::new();
    let mut text = false;
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if is_closed(conns, id) {
            let _ = conn.write_all(&egress::client_frame(8, &1001u16.to_be_bytes(), mask()));
            return;
        }
        loop {
            match out.try_recv() {
                Ok(WsOut::Message(t, data)) => {
                    if conn
                        .write_all(&egress::client_frame(if t { 1 } else { 2 }, &data, mask()))
                        .is_err()
                    {
                        board.ws_close(id, 1006);
                        return;
                    }
                },
                Ok(WsOut::Close(code)) => {
                    let payload = if code == 1005 { vec![] } else { code.to_be_bytes().to_vec() };
                    let _ = conn.write_all(&egress::client_frame(8, &payload, mask()));
                    let _ = conn.flush();
                    return;
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let _ = conn.flush();
        match conn.read(&mut chunk) {
            Ok(0) => {
                board.ws_close(id, 1006);
                return;
            },
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {},
            Err(_) => {
                board.ws_close(id, 1006);
                return;
            },
        }
        // Whole frames from the server
        while let Some((fin, opcode, payload, used)) = parse_frame(&buf) {
            buf.drain(..used);
            match opcode {
                8 => {
                    let code = if payload.len() >= 2 {
                        u16::from_be_bytes([payload[0], payload[1]])
                    } else {
                        1005
                    };
                    board.ws_close(id, code);
                    return;
                },
                9 => {
                    let _ = conn.write_all(&egress::client_frame(10, &payload, mask()));
                },
                10 => {},
                op => {
                    if op != 0 {
                        text = op == 1;
                        message.clear();
                    }
                    message.extend_from_slice(&payload);
                    if fin {
                        board.ws_send(id, text, &std::mem::take(&mut message));
                        backpressure(board, conns, id);
                    }
                },
            }
        }
    }
}

/// A guest TCP connection carried over a WebSocket to `url`. The guest's SYN waits until the
/// WebSocket is open, so a tunnel that can't be reached is refused like a direct connection.
fn tunnel(
    board: &Board,
    conns: &Shared,
    egress: &Egress,
    id: u32,
    url: &str,
    out: Receiver<Option<Vec<u8>>>,
) {
    let key = crate::dev::web::base64_encode(&random_bytes(16));
    let reader = match egress.websocket(url, &[], &key) {
        Ok((_, r)) => r,
        Err(_) => {
            conns.lock().unwrap().tunnels.remove(&id);
            board.tcp_refuse(id);
            return;
        },
    };
    board.tcp_accept(id);
    // As in websocket(): one thread owns the stream, polling it and sending what is queued
    let mut buf = reader.buffer().to_vec();
    let mut conn = reader.into_inner();
    let _ = conn.tcp().set_read_timeout(Some(Duration::from_millis(20)));
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if is_closed(conns, id) {
            let _ = conn.write_all(&egress::client_frame(8, &1000u16.to_be_bytes(), mask()));
            return;
        }
        loop {
            match out.try_recv() {
                Ok(Some(data)) => {
                    if conn.write_all(&egress::client_frame(2, &data, mask())).is_err() {
                        board.tcp_abort(id);
                        return;
                    }
                },
                // A WebSocket can't half-close: close it, and keep reading until the far side
                // answers with its own close
                Ok(None) => {
                    let _ = conn.write_all(&egress::client_frame(8, &1000u16.to_be_bytes(), mask()));
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let _ = conn.flush();
        match conn.read(&mut chunk) {
            Ok(0) => {
                board.tcp_close(id);
                return;
            },
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {},
            Err(_) => {
                board.tcp_abort(id);
                return;
            },
        }
        while let Some((_, opcode, payload, used)) = parse_frame(&buf) {
            buf.drain(..used);
            match opcode {
                8 => {
                    board.tcp_close(id);
                    return;
                },
                9 => {
                    let _ = conn.write_all(&egress::client_frame(10, &payload, mask()));
                },
                10 => {},
                // A byte stream: fragments and text frames are just more bytes
                _ => {
                    board.tcp_send(id, &payload);
                    backpressure(board, conns, id);
                },
            }
        }
    }
}

fn mask() -> [u8; 4] {
    random_bytes(4).try_into().unwrap()
}

/// A whole frame at the start of `b`: (fin, opcode, payload, bytes used).
fn parse_frame(b: &[u8]) -> Option<(bool, u8, Vec<u8>, usize)> {
    if b.len() < 2 {
        return None;
    }
    let mut len = (b[1] & 127) as usize;
    let mut at = 2;
    if len == 126 {
        len = u16::from_be_bytes([*b.get(2)?, *b.get(3)?]) as usize;
        at = 4;
    } else if len == 127 {
        len = u64::from_be_bytes(b.get(2..10)?.try_into().ok()?) as usize;
        at = 10;
    }
    let mask = if b[1] & 128 != 0 {
        let m: [u8; 4] = b.get(at..at + 4)?.try_into().ok()?;
        at += 4;
        Some(m)
    } else {
        None
    };
    let mut payload = b.get(at..at + len)?.to_vec();
    if let Some(m) = mask {
        for (i, x) in payload.iter_mut().enumerate() {
            *x ^= m[i & 3];
        }
    }
    Some((b[0] & 0x80 != 0, b[0] & 15, payload, at + len))
}

/// Pump a socket and a guest connection into each other.
fn attach(board: &Arc<Board>, conns: &Shared, id: u32, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let (wtx, wrx) = channel::<Option<Vec<u8>>>();
    let Ok(mut writer) = stream.try_clone() else {
        board.tcp_abort(id);
        return;
    };
    let Ok(mut reader) = stream.try_clone() else {
        board.tcp_abort(id);
        return;
    };
    conns.lock().unwrap().sockets.insert(id, (wtx, stream));
    // guest -> socket
    std::thread::spawn(move || {
        while let Ok(msg) = wrx.recv() {
            match msg {
                Some(data) => {
                    if writer.write_all(&data).is_err() {
                        break;
                    }
                },
                None => {
                    let _ = writer.shutdown(Shutdown::Write);
                },
            }
        }
    });
    // socket -> guest
    let board = board.clone();
    let conns = conns.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => {
                    board.tcp_close(id);
                    break;
                },
                Ok(n) => {
                    board.tcp_send(id, &buf[..n]);
                    backpressure(&board, &conns, id);
                },
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::Tunnel;

    #[test]
    fn tunnel_specs() {
        let t = Tunnel::parse("DB.internal:5432=wss://proxy.example.com/db?x=1").unwrap();
        assert_eq!(t.host.as_deref(), Some("db.internal"));
        assert_eq!((t.port, t.url.as_str()), (5432, "wss://proxy.example.com/db?x=1"));
        assert!(t.matches("db.INTERNAL", 5432));
        assert!(!t.matches("db.internal", 5433));
        assert!(!t.matches("other", 5432));

        let any = Tunnel::parse("6379=ws://127.0.0.1:7000").unwrap();
        assert_eq!(any.host, None);
        assert!(any.matches("anything", 6379));

        for bad in ["5432", "db:5432=tcp://x", ":5432=ws://x", "db:0=ws://x", "db:nope=ws://x", "=ws://x"] {
            assert_eq!(Tunnel::parse(bad), None, "{bad}");
        }
    }
}
