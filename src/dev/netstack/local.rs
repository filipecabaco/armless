//! Streams that never see Ethernet: the sockets of a process with no kernel (the `armless-user`
//! crate) reach the host through the same events and calls as the guest's TCP connections do,
//! with no frames and no guest TCP stack in between.
//!
//! The process's side is `local_*`; the host's side is the usual `accept`, `refuse`, `send`,
//! `close`, `abort`, `buffered` and `connect`, which answer for these ids as well. A stream's
//! bytes from the host wait here until the process reads them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::{Event, Outbox, Stack};

#[derive(Default)]
struct Local {
    /// `TcpOpen` is sent and the host has not answered.
    connecting: bool,
    /// The host refused (or gave up on) the connection.
    refused: bool,
    /// The host reset it.
    reset: bool,
    /// Bytes from the host the process has not read.
    rx: VecDeque<u8>,
    /// The host closed its sending side.
    host_fin: bool,
    /// The process closed its sending side.
    guest_fin: bool,
    /// `TcpClosed` is out already.
    closed_sent: bool,
}

#[derive(Default)]
pub(super) struct Locals {
    /// A process with no kernel uses this network: a connection to a port nobody listens on is
    /// refused at once, and not sent to a guest that is not there.
    active: bool,
    conns: BTreeMap<u32, Local>,
    /// TCP ports the process listens on.
    listening: BTreeSet<u16>,
    /// Connections the host opened, by port, until the process accepts them.
    pending: BTreeMap<u16, VecDeque<u32>>,
    /// Changes whatever the host does: lets the process see what happened while it waited.
    gen: u64,
}

/// What a process sees of one stream.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LocalState {
    /// The host has not answered yet.
    pub connecting: bool,
    /// Refused, reset, or gone.
    pub failed: bool,
    /// Bytes the process can read.
    pub readable: usize,
    /// The host will send no more.
    pub eof: bool,
}

impl Stack {
    /// The process side of the network is in use (see `Locals::active`).
    pub fn local_enable(&mut self) {
        self.locals.active = true;
    }

    /// Anything the host did to a stream or a listener changes this.
    pub fn local_gen(&self) -> u64 {
        self.locals.gen
    }

    /// A connection to `ip`:`port` from the process: `TcpOpen` goes to the host, which answers
    /// `tcp_accept` or `tcp_refuse`. The id is the connection's from now on.
    pub fn local_open(&mut self, ip: [u8; 4], port: u16, out: &Outbox) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.locals.conns.insert(
            id,
            Local {
                connecting: true,
                ..Default::default()
            },
        );
        out.push(Event::TcpOpen {
            id,
            host: self.host_of(ip),
            port,
        });
        id
    }

    pub fn local_state(&self, id: u32) -> LocalState {
        match self.locals.conns.get(&id) {
            Some(c) => LocalState {
                connecting: c.connecting,
                failed: c.refused || c.reset,
                readable: c.rx.len(),
                eof: c.host_fin,
            },
            None => LocalState {
                failed: true,
                ..Default::default()
            },
        }
    }

    pub fn local_read(&mut self, id: u32, max: usize) -> Vec<u8> {
        let Some(c) = self.locals.conns.get_mut(&id) else {
            return Vec::new();
        };
        let n = c.rx.len().min(max);
        c.rx.drain(..n).collect()
    }

    /// Data from the process to the host; false if the connection is not there to take it.
    pub fn local_write(&mut self, id: u32, data: &[u8], out: &Outbox) -> bool {
        match self.locals.conns.get(&id) {
            Some(c) if !c.connecting && !c.refused && !c.reset && !c.guest_fin => {
                out.push(Event::TcpData {
                    id,
                    data: data.to_vec(),
                });
                true
            },
            _ => false,
        }
    }

    /// The process will send no more.
    pub fn local_shutdown(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            if !c.guest_fin && !c.connecting && !c.refused && !c.reset {
                c.guest_fin = true;
                out.push(Event::TcpEof { id });
            }
            self.finish_local(id, out);
        }
    }

    /// The process is done with the stream (whatever its state).
    pub fn local_drop(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.locals.conns.remove(&id) {
            if !c.closed_sent && !c.reset {
                out.push(Event::TcpClosed { id });
            }
        }
    }

    /// Accept connections the host opens to `port` (`connect`) from now on.
    pub fn local_listen(&mut self, port: u16) {
        self.locals.listening.insert(port);
    }

    /// Stop; the connections nobody accepted are reset.
    pub fn local_unlisten(&mut self, port: u16) {
        self.locals.listening.remove(&port);
        for id in self.locals.pending.remove(&port).unwrap_or_default() {
            if let Some(c) = self.locals.conns.get_mut(&id) {
                c.reset = true;
            }
        }
    }

    /// Whether the process listens on `port` right now.
    pub fn local_listening(&self, port: u16) -> bool {
        self.locals.listening.contains(&port)
    }

    /// The next connection the host opened to the listening `port`.
    pub fn local_accept(&mut self, port: u16) -> Option<u32> {
        self.locals.pending.get_mut(&port)?.pop_front()
    }

    /// How many are waiting.
    pub fn local_pending(&self, port: u16) -> usize {
        self.locals.pending.get(&port).map_or(0, |q| q.len())
    }

    /// The host connects to the process's TCP `port`: `TcpConnected` at once, or `TcpClosed` when
    /// nobody listens.
    pub fn connect_local(&mut self, port: u16, out: &Outbox) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        if self.locals.listening.contains(&port) {
            self.locals.conns.insert(id, Local::default());
            self.locals.pending.entry(port).or_default().push_back(id);
            self.locals.gen += 1;
            out.push(Event::TcpConnected { id });
        } else {
            out.push(Event::TcpClosed { id });
        }
        id
    }

    /// Forget every stream and listener (a snapshot was loaded; the process listens again).
    pub(super) fn local_forget(&mut self) {
        self.locals.conns.clear();
        self.locals.pending.clear();
        self.locals.listening.clear();
    }

    pub(super) fn is_local(&self, id: u32) -> bool {
        self.locals.conns.contains_key(&id)
    }

    /// Whether `connect_local` is the one to open a connection to the process's `port`.
    pub fn wants_local(&self, port: u16) -> bool {
        self.locals.active || self.locals.listening.contains(&port)
    }

    /// A DNS query from the process: the answer the router gives the guest.
    pub fn local_dns(&mut self, q: &[u8]) -> Option<Vec<u8>> {
        self.dns(q)
    }

    // ------------------------------------------------ the host's calls, for these ids

    pub(super) fn local_accepted(&mut self, id: u32) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            c.connecting = false;
            self.locals.gen += 1;
        }
    }

    pub(super) fn local_refused(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            c.connecting = false;
            c.refused = true;
            c.closed_sent = true;
            self.locals.gen += 1;
            out.push(Event::TcpClosed { id });
        }
    }

    pub(super) fn local_sent(&mut self, id: u32, data: &[u8]) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            if !c.host_fin {
                c.rx.extend(data);
                self.locals.gen += 1;
            }
        }
    }

    pub(super) fn local_closed(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            c.host_fin = true;
            self.locals.gen += 1;
            self.finish_local(id, out);
        }
    }

    pub(super) fn local_aborted(&mut self, id: u32) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            c.reset = true;
            c.rx.clear();
            self.locals.gen += 1;
        }
    }

    pub(super) fn local_buffered(&self, id: u32) -> usize {
        self.locals.conns.get(&id).map_or(0, |c| c.rx.len())
    }

    /// Both sides are done sending: the host's `TcpClosed`.
    fn finish_local(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.locals.conns.get_mut(&id) {
            if c.host_fin && c.guest_fin && !c.closed_sent {
                c.closed_sent = true;
                out.push(Event::TcpClosed { id });
            }
        }
    }
}
