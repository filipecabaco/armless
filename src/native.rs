//! Host services for native programs: answer the machine's events.
//!
//! * blobs from a directory (`<dir>/<name>`)
//! * TCP: connections the guest opens go to real sockets (the name the guest
//!   resolved is resolved here), or are refused when the network is off
//! * connections into the guest (port forwarding) from host sockets
//! * the network's retransmit timer
//!
//! Every event is also shown to an observer first (console output, which
//! blobs were read, ...).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::dev::Event;
use crate::vm::Board;

#[derive(Clone, Debug, Default)]
pub struct HostOptions {
    /// Directory holding the filesystem's blobs.
    pub blobs: Option<PathBuf>,
    /// Let the guest open TCP connections to the outside world.
    pub nat: bool,
}

pub type Observer = Box<dyn FnMut(&Event) + Send>;

enum Msg {
    Wake,
    /// A host socket to connect to the guest's port.
    Forward(TcpStream, u16),
    Quit,
}

struct Conn {
    writer: Sender<Option<Vec<u8>>>,
    stream: TcpStream,
}

pub struct NativeHost {
    tx: Sender<Msg>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Bytes the guest has not taken yet before a socket reader waits.
const BACKLOG: usize = 1 << 20;

impl NativeHost {
    pub fn spawn(board: Arc<Board>, options: HostOptions, observer: Observer) -> NativeHost {
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
    let conns: Arc<Mutex<HashMap<u32, Conn>>> = Arc::default();
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
                Event::TcpOpen { id, host, port } => {
                    if !options.nat {
                        board.tcp_refuse(id);
                        continue;
                    }
                    let board = board.clone();
                    let conns = conns.clone();
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let stream =
                            (host.as_str(), port)
                                .to_socket_addrs()
                                .ok()
                                .and_then(|addrs| {
                                    addrs.filter(|a| a.is_ipv4() || a.is_ipv6()).find_map(|a| {
                                        TcpStream::connect_timeout(&a, Duration::from_secs(10)).ok()
                                    })
                                });
                        match stream {
                            Some(s) => {
                                board.tcp_accept(id);
                                attach(&board, &conns, id, s);
                            },
                            None => board.tcp_refuse(id),
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
                    if let Some(c) = conns.lock().unwrap().get(&id) {
                        let _ = c.writer.send(Some(data));
                    }
                },
                Event::TcpEof { id } => {
                    if let Some(c) = conns.lock().unwrap().get(&id) {
                        let _ = c.writer.send(None);
                    }
                },
                Event::TcpClosed { id } => {
                    if let Some(s) = forwarding.remove(&id) {
                        let _ = s.shutdown(Shutdown::Both);
                    }
                    if let Some(c) = conns.lock().unwrap().remove(&id) {
                        let _ = c.stream.shutdown(Shutdown::Both);
                    }
                },
                Event::Serial(_) | Event::Console(_) => {},
            }
        }
    }
    for (_, c) in conns.lock().unwrap().drain() {
        let _ = c.stream.shutdown(Shutdown::Both);
    }
}

/// Pump a socket and a guest connection into each other.
fn attach(board: &Arc<Board>, conns: &Arc<Mutex<HashMap<u32, Conn>>>, id: u32, stream: TcpStream) {
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
    conns.lock().unwrap().insert(
        id,
        Conn {
            writer: wtx,
            stream,
        },
    );
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
                    while board.tcp_buffered(id) > BACKLOG {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                },
            }
        }
    });
}
