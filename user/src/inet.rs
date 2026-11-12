//! TCP and UDP sockets (AF_INET) of a program in user mode. With no guest kernel there are no
//! packets: a connection is the network stack's (`armless::dev::netstack`'s programs), with the
//! same ids, host calls and events as a kernel guest's TCP, so the host serves both alike.
//!
//! - **Out:** `connect` to an address DNS gave (or any other) opens one; ports 80 and 443 go to
//!   the machine's web relay (HTTP, and HTTPS with the site's CA) and are open at once, others
//!   reach the host as `TcpOpen` and are open once it accepts (`EINPROGRESS` for a non-blocking
//!   socket; writable, and `SO_ERROR`, when it is).
//! - **In:** `listen` on a port takes the connections the host opens there (`Board::tcp_connect`:
//!   snowglobe's previews and links), for `accept`.
//! - **Itself:** a connection to 127.0.0.1 (or the machine's address) on a port the program
//!   listens on needs no host: its two ends are a socket pair's pipes.
//! - **DNS:** what a UDP socket sends to port 53, wherever, is answered as the router's DNS
//!   answers a kernel guest (each name an address of its own), so `getaddrinfo` works. Other UDP
//!   goes nowhere.
//!
//! A listening socket is kept in snapshots and listens again after a restore; a connection is
//! not (the host's end belonged to the host that saved it): it reads ECONNRESET after a restore,
//! as a kernel guest's gets a reset. IPv6 sockets are refused (EAFNOSUPPORT), so programs use
//! IPv4.

use std::collections::VecDeque;

use armless::dev::netstack::{ProgEnd, ProgStatus, GUEST_IP};
use armless::dev::Outbox;

use crate::files::{Addr, Kind, Tcp, TcpState, Udp, O_CLOEXEC, O_NONBLOCK, SHUT_RD, SHUT_WR};
use crate::{Process, Ret};

const AF_INET: u64 = 2;
const AF_INET6: u64 = 10;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_TYPE_MASK: u64 = 0xf;
const IPPROTO_TCP: u64 = 6;
const IPPROTO_UDP: u64 = 17;
const O_RDWR: u32 = 2;
const MSG_DONTWAIT: u64 = 0x40;
const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_ACCEPTCONN: u64 = 30;
const SO_DOMAIN: u64 = 39;
const SO_PROTOCOL: u64 = 38;
const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
const ANY: [u8; 4] = [0, 0, 0, 0];
const DNS_PORT: u16 = 53;
/// Where ports a program didn't choose start
const EPHEMERAL: u16 = 40000;

const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const EPIPE: i64 = 32;
const EDESTADDRREQ: i64 = 89;
const EPROTONOSUPPORT: i64 = 93;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EADDRINUSE: i64 = 98;
const ENETUNREACH: i64 = 101;
const ECONNRESET: i64 = 104;
const EISCONN: i64 = 106;
const ENOTCONN: i64 = 107;
const ECONNREFUSED: i64 = 111;
const EALREADY: i64 = 114;
const EINPROGRESS: i64 = 115;

fn wait() -> Ret {
    Ret::Retry {
        deadline: None,
        on_timeout: 0,
    }
}

fn errno(end: ProgEnd) -> i64 {
    match end {
        ProgEnd::Closed => EPIPE,
        ProgEnd::Refused => ECONNREFUSED,
        ProgEnd::Reset => ECONNRESET,
    }
}

fn local(ip: [u8; 4]) -> bool {
    ip[0] == 127 || ip == GUEST_IP
}

impl Process {
    /// `f` on the network stack, as a program's (None without a network).
    fn net<T>(
        &self,
        f: impl FnOnce(&mut armless::dev::netstack::Stack, &Outbox) -> T,
    ) -> Option<T> {
        self.board.as_ref()?.with_prog_net(f)
    }

    // ------------------------------------------------------------ addresses

    /// The sockaddr_in at `addr`: AF_INET6 is refused, as the program's IPv6 sockets are.
    fn sockaddr_in(&self, addr: u64, len: u64) -> Result<Addr, i64> {
        if len < 2 {
            return Err(EINVAL);
        }
        let Some(b) = self.bytes_at(addr, (len as usize).min(16)) else {
            return Err(EFAULT);
        };
        match u16::from_le_bytes([b[0], b[1]]) as u64 {
            AF_INET if b.len() >= 8 => {
                Ok(([b[4], b[5], b[6], b[7]], u16::from_be_bytes([b[2], b[3]])))
            },
            AF_INET6 => Err(EAFNOSUPPORT),
            _ => Err(EAFNOSUPPORT),
        }
    }

    /// A sockaddr_in at `addr` (as much as `*len` allows), and its size in `*len`.
    fn put_sockaddr_in(&self, addr: u64, len: u64, (ip, port): Addr) -> bool {
        if addr == 0 {
            return true;
        }
        let Some(room) = self.u32_at(len) else {
            return false;
        };
        let mut b = [0u8; 16];
        b[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
        b[2..4].copy_from_slice(&port.to_be_bytes());
        b[4..8].copy_from_slice(&ip);
        let n = (room as usize).min(16);
        (n == 0 || self.write(addr, &b[..n])) && self.put_u32(len, 16)
    }

    /// Whether another socket of the program (TCP or UDP, as `tcp`) has `port`.
    fn port_used(&self, port: u16, tcp: bool, except: u32) -> bool {
        self.files.descs.iter().any(|(&d, desc)| {
            d != except
                && match &desc.kind {
                    Kind::Tcp(t) if tcp => {
                        t.port == port
                            && matches!(t.state, TcpState::Idle | TcpState::Listening { .. })
                    },
                    Kind::Udp(u) if !tcp => u.port == port,
                    _ => false,
                }
        })
    }

    /// A port no socket of the program has.
    fn ephemeral(&self, tcp: bool) -> u16 {
        let used: std::collections::BTreeSet<u16> = self
            .files
            .descs
            .values()
            .filter_map(|d| match &d.kind {
                Kind::Tcp(t) if tcp => Some(t.port),
                Kind::Udp(u) if !tcp => Some(u.port),
                _ => None,
            })
            .collect();
        (EPHEMERAL..=u16::MAX)
            .find(|p| !used.contains(p))
            .unwrap_or(EPHEMERAL)
    }

    fn tcp_of(&self, fd: i32) -> Option<Tcp> {
        match &self.files.get(fd)?.kind {
            Kind::Tcp(t) => Some(t.clone()),
            _ => None,
        }
    }

    fn set_tcp(&mut self, fd: i32, f: impl FnOnce(&mut Tcp)) {
        if let Some(Kind::Tcp(t)) = self.files.get_mut(fd).map(|d| &mut d.kind) {
            f(t);
        }
    }

    // ------------------------------------------------------------ the calls

    /// socket(domain, type, protocol): AF_INET stream (TCP) or datagram (UDP).
    pub(crate) fn inet_socket(&mut self, domain: u64, ty: u64, protocol: u64) -> Ret {
        let v = Ret::Val;
        if domain != AF_INET {
            // IPv6, AF_UNIX by path, netlink: programs fall back without them
            return v(-EAFNOSUPPORT);
        }
        if ty & !(SOCK_TYPE_MASK | O_NONBLOCK as u64 | O_CLOEXEC as u64) != 0 {
            return v(-EINVAL);
        }
        let kind = match (ty & SOCK_TYPE_MASK, protocol) {
            (SOCK_STREAM, 0 | IPPROTO_TCP) => Kind::Tcp(Tcp {
                port: 0,
                peer: (ANY, 0),
                state: TcpState::Idle,
            }),
            (SOCK_DGRAM, 0 | IPPROTO_UDP) => Kind::Udp(Udp::default()),
            (SOCK_STREAM | SOCK_DGRAM, _) => return v(-EPROTONOSUPPORT),
            _ => return v(-EINVAL),
        };
        if self.files.fds.len() >= 1024 {
            return v(-EMFILE);
        }
        let d = self.files.add(kind, O_RDWR | ty as u32 & O_NONBLOCK);
        v(self.files.install(d, 0, ty & O_CLOEXEC as u64 != 0) as i64)
    }

    /// bind, listen, accept, connect, getsockname, getpeername, sendto, recvfrom, setsockopt,
    /// getsockopt, shutdown, sendmsg, recvmsg, accept4 on a TCP or UDP socket.
    pub(crate) fn inet_call(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        let fd = a[0] as i32;
        let r = match nr {
            200 => self.bind(fd, a[1], a[2]),
            201 => self.listen(fd),
            202 => return self.accept(fd, a[1], a[2], 0),
            242 => return self.accept(fd, a[1], a[2], a[3]),
            203 => return self.connect(fd, a[1], a[2]),
            204 => self.sockname(fd, a[1], a[2], false),
            205 => self.sockname(fd, a[1], a[2], true),
            206 => return self.sendto(fd, a[1], a[2], a[3], a[4], a[5]),
            207 => return self.recvfrom(fd, a[1], a[2], a[3], a[4], a[5]),
            208 => Ok(0),
            209 => self.getsockopt(fd, a[1], a[2], a[3], a[4]),
            210 => self.inet_shutdown(fd, a[1]),
            211 | 212 => return self.inet_msg(nr == 211, fd, a[1], a[2]),
            _ => Err(EINVAL),
        };
        v(r.unwrap_or_else(|e| -e))
    }

    fn bind(&mut self, fd: i32, addr: u64, len: u64) -> Result<i64, i64> {
        let (_, port) = self.sockaddr_in(addr, len)?;
        let d = self.files.desc_of(fd).ok_or(EINVAL)?;
        let tcp = matches!(self.files.get(fd).map(|d| &d.kind), Some(Kind::Tcp(_)));
        let port = if port == 0 { self.ephemeral(tcp) } else { port };
        if self.port_used(port, tcp, d) {
            return Err(EADDRINUSE);
        }
        match &mut self.files.get_mut(fd).ok_or(EINVAL)?.kind {
            Kind::Tcp(t) if t.port == 0 && t.state == TcpState::Idle => t.port = port,
            Kind::Udp(u) if u.port == 0 => u.port = port,
            _ => return Err(EINVAL),
        }
        Ok(0)
    }

    fn listen(&mut self, fd: i32) -> Result<i64, i64> {
        let t = self.tcp_of(fd).ok_or(EOPNOTSUPP)?;
        match t.state {
            TcpState::Listening { .. } => return Ok(0),
            TcpState::Idle => {},
            _ => return Err(EINVAL),
        }
        let d = self.files.desc_of(fd).ok_or(EINVAL)?;
        let port = if t.port == 0 { self.ephemeral(true) } else { t.port };
        if self.port_used(port, true, d) {
            return Err(EADDRINUSE);
        }
        // Without a network it still takes the program's own connections
        if self.net(|s, _| s.prog_listen(port)) == Some(false) {
            return Err(EADDRINUSE);
        }
        self.set_tcp(fd, |t| {
            t.port = port;
            t.state = TcpState::Listening {
                local: VecDeque::new(),
            };
        });
        Ok(0)
    }

    /// accept4(fd, addr, len, flags): the next connection waiting, the program's own first.
    fn accept(&mut self, fd: i32, addr: u64, len: u64, flags: u64) -> Ret {
        let v = Ret::Val;
        let Some(t) = self.tcp_of(fd) else {
            return v(-EOPNOTSUPP);
        };
        let TcpState::Listening { local } = &t.state else {
            return v(-EINVAL);
        };
        if flags & !(O_NONBLOCK as u64 | O_CLOEXEC as u64) != 0 {
            return v(-EINVAL);
        }
        let nb = flags as u32 & O_NONBLOCK;
        let cloexec = flags & O_CLOEXEC as u64 != 0;
        let d = if let Some(&d) = local.front() {
            self.set_tcp(fd, |t| {
                if let TcpState::Listening { local } = &mut t.state {
                    local.pop_front();
                }
            });
            if let Some(desc) = self.files.descs.get_mut(&d) {
                desc.flags = O_RDWR | nb;
            }
            d
        } else if let Some(Some((id, peer))) = self.net(|s, _| {
            s.prog_accept(t.port)
                .map(|id| (id, s.prog_peer(id).unwrap_or((ANY, 0))))
        }) {
            self.files.add(
                Kind::Tcp(Tcp {
                    port: t.port,
                    peer,
                    state: TcpState::Conn { id, shut: 0 },
                }),
                O_RDWR | nb,
            )
        } else if self.nonblock(fd) {
            return v(-EAGAIN);
        } else {
            return wait();
        };
        let peer = match &self.files.descs[&d].kind {
            Kind::Tcp(t) => t.peer,
            _ => (ANY, 0),
        };
        let new = self.files.install(d, 0, cloexec);
        if !self.put_sockaddr_in(addr, len, peer) {
            return v(-EFAULT);
        }
        self.poked = true;
        v(new as i64)
    }

    fn connect(&mut self, fd: i32, addr: u64, len: u64) -> Ret {
        let v = Ret::Val;
        let to = match self.sockaddr_in(addr, len) {
            Ok(a) => a,
            Err(e) => return v(-e),
        };
        if let Some(Kind::Udp(_)) = self.files.get(fd).map(|d| &d.kind) {
            if let Some(Kind::Udp(u)) = self.files.get_mut(fd).map(|d| &mut d.kind) {
                u.peer = Some(to);
            }
            return v(0);
        }
        let Some(t) = self.tcp_of(fd) else {
            return v(-EOPNOTSUPP);
        };
        let nonblock = self.nonblock(fd);
        match t.state {
            TcpState::Idle => {},
            // Called again: a blocking connect waiting, or a program asking how it went
            TcpState::Conn { id, .. } => {
                return match self.net(|s, _| s.prog_status(id)) {
                    Some(ProgStatus::Open) => v(if nonblock { -EISCONN } else { 0 }),
                    Some(ProgStatus::Connecting) if nonblock => v(-EALREADY),
                    Some(ProgStatus::Connecting) => wait(),
                    Some(ProgStatus::Ended(e)) => {
                        self.set_tcp(fd, |t| t.state = TcpState::Idle);
                        v(-errno(e))
                    },
                    None => v(-ENETUNREACH),
                };
            },
            TcpState::Local { .. } => return v(-EISCONN),
            TcpState::Listening { .. } => return v(-EINVAL),
            TcpState::Failed(e) => {
                self.set_tcp(fd, |t| t.state = TcpState::Idle);
                return v(-e);
            },
        }
        let me = if t.port == 0 { self.ephemeral(true) } else { t.port };
        if local(to.0) {
            return v(match self.connect_local(fd, me, to.1) {
                Ok(()) => 0,
                Err(e) => -e,
            });
        }
        let Some(id) = self.net(|s, out| s.prog_open(to.0, to.1, me, out)) else {
            return v(-ENETUNREACH);
        };
        self.set_tcp(fd, |t| {
            t.port = me;
            t.peer = to;
            t.state = TcpState::Conn { id, shut: 0 };
        });
        match self.net(|s, _| s.prog_status(id)) {
            Some(ProgStatus::Open) => v(0),
            _ if nonblock => v(-EINPROGRESS),
            _ => wait(),
        }
    }

    /// A connection to the program's own listener on `port`: a socket pair's two pipes, the other
    /// end waiting at the listener for accept.
    fn connect_local(&mut self, fd: i32, me: u16, port: u16) -> Result<(), i64> {
        let listener = self
            .files
            .descs
            .iter()
            .find(|(_, d)| {
                matches!(&d.kind, Kind::Tcp(t) if t.port == port && matches!(t.state, TcpState::Listening { .. }))
            })
            .map(|(&d, _)| d)
            .ok_or(ECONNREFUSED)?;
        let (p, q) = (self.files.new_pipe(), self.files.new_pipe());
        let theirs = self.files.add(
            Kind::Tcp(Tcp {
                port,
                peer: (LOOPBACK, me),
                state: TcpState::Local {
                    rx: q,
                    tx: p,
                    shut: 0,
                },
            }),
            O_RDWR,
        );
        if let Some(Kind::Tcp(Tcp {
            state: TcpState::Listening { local },
            ..
        })) = self.files.descs.get_mut(&listener).map(|d| &mut d.kind)
        {
            local.push_back(theirs);
        }
        self.files.activity += 1;
        // The pipes' ends move from a placeholder to this socket
        let mine = Kind::Tcp(Tcp {
            port: me,
            peer: (LOOPBACK, port),
            state: TcpState::Local {
                rx: p,
                tx: q,
                shut: 0,
            },
        });
        if let Some(d) = self.files.desc_of(fd) {
            if let Some(pipe) = self.files.pipes.get_mut(&p) {
                pipe.readers += 1;
            }
            if let Some(pipe) = self.files.pipes.get_mut(&q) {
                pipe.writers += 1;
            }
            if let Some(desc) = self.files.descs.get_mut(&d) {
                desc.kind = mine;
            }
        }
        self.poked = true;
        Ok(())
    }

    fn sockname(&mut self, fd: i32, addr: u64, len: u64, peer: bool) -> Result<i64, i64> {
        let a = match &self.files.get(fd).ok_or(EINVAL)?.kind {
            Kind::Tcp(t) => match (&t.state, peer) {
                (TcpState::Conn { .. } | TcpState::Local { .. }, true) => t.peer,
                (_, true) => return Err(ENOTCONN),
                (TcpState::Conn { .. }, false) => (GUEST_IP, t.port),
                (TcpState::Local { .. }, false) => (LOOPBACK, t.port),
                (_, false) => (ANY, t.port),
            },
            Kind::Udp(u) if peer => u.peer.ok_or(ENOTCONN)?,
            Kind::Udp(u) => (ANY, u.port),
            _ => return Err(EINVAL),
        };
        if self.put_sockaddr_in(addr, len, a) {
            Ok(0)
        } else {
            Err(EFAULT)
        }
    }

    fn getsockopt(
        &mut self,
        fd: i32,
        level: u64,
        name: u64,
        val: u64,
        len: u64,
    ) -> Result<i64, i64> {
        let kind = self.files.get(fd).ok_or(EINVAL)?.kind.clone();
        let value: u32 = match (level, name) {
            (SOL_SOCKET, SO_TYPE) => match kind {
                Kind::Udp(_) => SOCK_DGRAM as u32,
                _ => SOCK_STREAM as u32,
            },
            (SOL_SOCKET, SO_DOMAIN) => AF_INET as u32,
            (SOL_SOCKET, SO_PROTOCOL) => match kind {
                Kind::Udp(_) => IPPROTO_UDP as u32,
                _ => IPPROTO_TCP as u32,
            },
            (SOL_SOCKET, SO_ACCEPTCONN) => {
                matches!(&kind, Kind::Tcp(t) if matches!(t.state, TcpState::Listening { .. }))
                    as u32
            },
            (SOL_SOCKET, SO_SNDBUF | SO_RCVBUF) => crate::files::PIPE_BUF_MAX as u32,
            // How a non-blocking connect went: once
            (SOL_SOCKET, SO_ERROR) => match kind {
                Kind::Tcp(Tcp {
                    state: TcpState::Failed(e),
                    ..
                }) => {
                    self.set_tcp(fd, |t| t.state = TcpState::Idle);
                    e as u32
                },
                Kind::Tcp(Tcp {
                    state: TcpState::Conn { id, .. },
                    ..
                }) => match self.net(|s, _| s.prog_status(id)) {
                    Some(ProgStatus::Ended(e @ (ProgEnd::Refused | ProgEnd::Reset))) => {
                        self.set_tcp(fd, |t| t.state = TcpState::Idle);
                        errno(e) as u32
                    },
                    _ => 0,
                },
                _ => 0,
            },
            // SO_REUSEADDR, SO_KEEPALIVE, TCP_NODELAY and the like: taken, and off
            _ => 0,
        };
        let ok = self.u32_at(len).is_some_and(|l| l >= 4)
            && self.put_u32(val, value)
            && self.put_u32(len, 4);
        if ok {
            Ok(0)
        } else {
            Err(EFAULT)
        }
    }

    fn inet_shutdown(&mut self, fd: i32, how: u64) -> Result<i64, i64> {
        let how = match how {
            0 => SHUT_RD,
            1 => SHUT_WR,
            2 => SHUT_RD | SHUT_WR,
            _ => return Err(EINVAL),
        };
        let t = self.tcp_of(fd).ok_or(ENOTCONN)?;
        match t.state {
            TcpState::Conn { id, shut } => {
                if how & SHUT_WR != 0 && shut & SHUT_WR == 0 {
                    self.net(|s, out| s.prog_shutdown(id, out));
                }
                self.set_tcp(fd, |t| {
                    t.state = TcpState::Conn {
                        id,
                        shut: shut | how,
                    }
                });
            },
            TcpState::Local { shut, .. } => {
                if let Some(d) = self.files.desc_of(fd) {
                    self.files.shutdown(d, how & !shut);
                }
            },
            _ => return Err(ENOTCONN),
        }
        self.poked = true;
        Ok(0)
    }

    // ------------------------------------------------------------ data

    /// What a TCP socket is ready for: (readable, writable, hung up).
    pub(crate) fn tcp_ready(&self, t: &Tcp) -> (bool, bool, bool) {
        match &t.state {
            TcpState::Listening { local } => {
                let net = self.net(|s, _| s.prog_backlog(t.port)).unwrap_or(0);
                (!local.is_empty() || net > 0, false, false)
            },
            TcpState::Conn { id, shut } => {
                match self.net(|s, _| (s.prog_status(*id), s.prog_pending(*id))) {
                    Some((ProgStatus::Open, (n, eof))) => (
                        n > 0 || eof || shut & SHUT_RD != 0,
                        shut & SHUT_WR == 0,
                        eof && shut & SHUT_WR != 0,
                    ),
                    Some((ProgStatus::Connecting, _)) => (false, false, false),
                    // Ended (or no network): an error or the end, for whatever asks
                    _ => (true, true, true),
                }
            },
            TcpState::Local { rx, tx, shut } => {
                let (r, rx_done) = self.pipe_readable(*rx);
                let (w, tx_done) = self.pipe_writable(*tx);
                (
                    r || shut & SHUT_RD != 0,
                    w || shut & SHUT_WR != 0,
                    rx_done && tx_done,
                )
            },
            TcpState::Failed(_) => (true, true, true),
            TcpState::Idle => (false, false, false),
        }
    }

    pub(crate) fn tcp_read(&mut self, fd: i32, max: usize, nonblock: bool) -> Result<Vec<u8>, Ret> {
        let Some(t) = self.tcp_of(fd) else {
            return Err(Ret::Val(-EINVAL));
        };
        match t.state {
            TcpState::Conn { shut, .. } if shut & SHUT_RD != 0 => Ok(Vec::new()),
            TcpState::Conn { id, .. } => match self.net(|s, _| s.prog_status(id)) {
                Some(ProgStatus::Open) => {
                    let (b, eof) = self
                        .net(|s, _| s.prog_read(id, max))
                        .unwrap_or((Vec::new(), true));
                    if !b.is_empty() || eof {
                        if !b.is_empty() {
                            self.poked = true;
                        }
                        Ok(b)
                    } else if nonblock {
                        Err(Ret::Val(-EAGAIN))
                    } else {
                        Err(wait())
                    }
                },
                Some(ProgStatus::Connecting) if nonblock => Err(Ret::Val(-EAGAIN)),
                Some(ProgStatus::Connecting) => Err(wait()),
                Some(ProgStatus::Ended(ProgEnd::Closed)) => Ok(Vec::new()),
                Some(ProgStatus::Ended(e)) => {
                    // Once: the next read is the end
                    self.set_tcp(fd, |t| {
                        t.state = TcpState::Conn {
                            id,
                            shut: SHUT_RD | SHUT_WR,
                        }
                    });
                    Err(Ret::Val(-errno(e)))
                },
                None => Err(Ret::Val(-ENETUNREACH)),
            },
            TcpState::Local { shut, .. } if shut & SHUT_RD != 0 => Ok(Vec::new()),
            TcpState::Local { rx, .. } => self.pipe_read(rx, max, nonblock),
            TcpState::Failed(e) => Err(Ret::Val(-e)),
            TcpState::Idle | TcpState::Listening { .. } => Err(Ret::Val(-ENOTCONN)),
        }
    }

    pub(crate) fn tcp_write(&mut self, fd: i32, data: &[u8], nonblock: bool) -> Result<usize, Ret> {
        let Some(t) = self.tcp_of(fd) else {
            return Err(Ret::Val(-EINVAL));
        };
        match t.state {
            TcpState::Conn { shut, .. } if shut & SHUT_WR != 0 => Err(Ret::Val(-EPIPE)),
            TcpState::Conn { id, .. } => match self.net(|s, _| s.prog_status(id)) {
                Some(ProgStatus::Open) => {
                    let n = self.net(|s, out| s.prog_write(id, data, out)).unwrap_or(0);
                    if n == 0 && !data.is_empty() {
                        return Err(Ret::Val(-EPIPE));
                    }
                    self.poked = true;
                    Ok(n)
                },
                Some(ProgStatus::Connecting) if nonblock => Err(Ret::Val(-EAGAIN)),
                Some(ProgStatus::Connecting) => Err(wait()),
                Some(ProgStatus::Ended(e)) => Err(Ret::Val(-errno(e))),
                None => Err(Ret::Val(-ENETUNREACH)),
            },
            TcpState::Local { shut, .. } if shut & SHUT_WR != 0 => Err(Ret::Val(-EPIPE)),
            TcpState::Local { tx, .. } => self.pipe_write(tx, data, nonblock),
            TcpState::Failed(e) => Err(Ret::Val(-e)),
            TcpState::Idle | TcpState::Listening { .. } => Err(Ret::Val(-ENOTCONN)),
        }
    }

    /// The next datagram for a UDP socket, and where it came from.
    pub(crate) fn udp_recv(
        &mut self,
        fd: i32,
        max: usize,
        nonblock: bool,
    ) -> Result<(Addr, Vec<u8>), Ret> {
        let Some(Kind::Udp(u)) = self.files.get_mut(fd).map(|d| &mut d.kind) else {
            return Err(Ret::Val(-EINVAL));
        };
        match u.rx.pop_front() {
            Some((from, mut data)) => {
                data.truncate(max);
                Ok((from, data))
            },
            None if nonblock => Err(Ret::Val(-EAGAIN)),
            None => Err(wait()),
        }
    }

    /// A datagram from a UDP socket, to `to` or its peer: a DNS query (to port 53) is answered,
    /// and the answer waits to be read; others go nowhere.
    pub(crate) fn udp_send(
        &mut self,
        fd: i32,
        to: Option<Addr>,
        data: &[u8],
    ) -> Result<usize, Ret> {
        let Some(Kind::Udp(u)) = self.files.get(fd).map(|d| d.kind.clone()) else {
            return Err(Ret::Val(-EINVAL));
        };
        let Some(dest) = to.or(u.peer) else {
            return Err(Ret::Val(-EDESTADDRREQ));
        };
        let port = if u.port == 0 { self.ephemeral(false) } else { u.port };
        let answer =
            if dest.1 == DNS_PORT { self.net(|s, _| s.prog_dns(data)).flatten() } else { None };
        if let Some(Kind::Udp(u)) = self.files.get_mut(fd).map(|d| &mut d.kind) {
            u.port = port;
            if let Some(a) = answer {
                u.rx.push_back((dest, a));
            }
        }
        self.files.activity += 1;
        self.poked = true;
        Ok(data.len())
    }

    fn sendto(&mut self, fd: i32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Ret {
        let v = Ret::Val;
        let Some(b) = self.bytes_at(buf, len as usize) else {
            return v(-EFAULT);
        };
        let r = match self.files.get(fd).map(|d| &d.kind) {
            Some(Kind::Udp(_)) => {
                let to = if addr == 0 {
                    None
                } else {
                    match self.sockaddr_in(addr, alen) {
                        Ok(a) => Some(a),
                        Err(e) => return v(-e),
                    }
                };
                self.udp_send(fd, to, &b)
            },
            _ => {
                let nonblock = self.nonblock(fd) || flags & MSG_DONTWAIT != 0;
                self.tcp_write(fd, &b, nonblock)
            },
        };
        r.map_or_else(|r| r, |n| v(n as i64))
    }

    fn recvfrom(&mut self, fd: i32, buf: u64, len: u64, flags: u64, addr: u64, alen: u64) -> Ret {
        let v = Ret::Val;
        let nonblock = self.nonblock(fd) || flags & MSG_DONTWAIT != 0;
        let r = match self.files.get(fd).map(|d| &d.kind) {
            Some(Kind::Udp(_)) => self.udp_recv(fd, len as usize, nonblock),
            _ => {
                let peer = self.tcp_of(fd).map_or((ANY, 0), |t| t.peer);
                self.tcp_read(fd, len as usize, nonblock).map(|b| (peer, b))
            },
        };
        match r {
            Ok((from, b)) => {
                if !self.write(buf, &b) || addr != 0 && !self.put_sockaddr_in(addr, alen, from) {
                    return v(-EFAULT);
                }
                v(b.len() as i64)
            },
            Err(r) => r,
        }
    }

    /// sendmsg and recvmsg: the msghdr's iovecs, its address (UDP), no control messages.
    fn inet_msg(&mut self, send: bool, fd: i32, msg: u64, flags: u64) -> Ret {
        let v = Ret::Val;
        let field = |o: u64| self.u64_at(msg + o);
        let (Some(name), Some(namelen), Some(iov), Some(iovlen), Some(controllen)) = (
            field(0),
            self.u32_at(msg + 8),
            field(16),
            field(24),
            field(40),
        ) else {
            return v(-EFAULT);
        };
        let Some(iov) = self.iovecs(iov, iovlen) else {
            return v(-EFAULT);
        };
        let udp = matches!(self.files.get(fd).map(|d| &d.kind), Some(Kind::Udp(_)));
        let nonblock = self.nonblock(fd) || flags & MSG_DONTWAIT != 0;
        if send {
            if controllen != 0 {
                return v(-EOPNOTSUPP);
            }
            let mut all = Vec::new();
            for (base, len) in iov {
                let Some(b) = self.bytes_at(base, len as usize) else {
                    return v(-EFAULT);
                };
                all.extend_from_slice(&b);
            }
            let r = if udp {
                let to = if name == 0 {
                    None
                } else {
                    match self.sockaddr_in(name, namelen as u64) {
                        Ok(a) => Some(a),
                        Err(e) => return v(-e),
                    }
                };
                self.udp_send(fd, to, &all)
            } else {
                self.tcp_write(fd, &all, nonblock)
            };
            return r.map_or_else(|r| r, |n| v(n as i64));
        }
        let total: u64 = iov.iter().map(|&(_, l)| l).sum();
        let r = if udp {
            self.udp_recv(fd, total as usize, nonblock)
        } else {
            let peer = self.tcp_of(fd).map_or((ANY, 0), |t| t.peer);
            self.tcp_read(fd, total as usize, nonblock)
                .map(|b| (peer, b))
        };
        match r {
            Ok((from, b)) => {
                let mut off = 0;
                for (base, len) in iov {
                    let n = (len as usize).min(b.len() - off);
                    if !self.write(base, &b[off..off + n]) {
                        return v(-EFAULT);
                    }
                    off += n;
                }
                // The sender's address goes where msg_name points, its size in msg_namelen
                let named = name == 0 || self.put_sockaddr_in(name, msg + 8, from);
                let ok = named && self.put_u64(msg + 40, 0) && self.put_u32(msg + 48, 0);
                v(if ok { b.len() as i64 } else { -EFAULT })
            },
            Err(r) => r,
        }
    }

    // ------------------------------------------------------------ bookkeeping

    /// Sockets whose last descriptor closed: their connections end, their listeners stop.
    pub(crate) fn sockets_gone(&mut self) {
        for kind in std::mem::take(&mut self.files.gone) {
            match kind {
                Kind::Tcp(Tcp {
                    state: TcpState::Conn { id, .. },
                    ..
                }) => {
                    self.net(|s, out| s.prog_close(id, out));
                },
                Kind::Tcp(Tcp {
                    port,
                    state: TcpState::Listening { .. },
                    ..
                }) => {
                    self.net(|s, out| s.prog_unlisten(port, out));
                },
                _ => {},
            }
        }
    }

    /// The machine's network (if it has one) is this program's: `Stack::user`.
    pub(crate) fn claim_network(&self) {
        self.net(|_, _| ());
    }

    /// What has arrived so far, for edge-triggered epoll (`Watch::seen`): the process's own
    /// (pipes, its loopback, DNS) and the network's.
    pub(crate) fn activity(&self) -> u64 {
        self.files.activity + self.net(|s, _| s.changes()).unwrap_or(0)
    }

    /// The program ended: every socket it has is gone (once).
    pub(crate) fn close_sockets(&mut self) {
        let fds: Vec<i32> = self
            .files
            .fds
            .iter()
            .filter(|(_, (d, _))| {
                matches!(
                    self.files.descs.get(d).map(|d| &d.kind),
                    Some(Kind::Tcp(_) | Kind::Udp(_))
                )
            })
            .map(|(&fd, _)| fd)
            .collect();
        for fd in fds {
            self.files.close(fd);
        }
        self.sockets_gone();
    }

    /// Whether the network changed for the program since it last looked (a thread waiting on a
    /// socket looks again).
    pub(crate) fn net_changed(&mut self) -> bool {
        let Some(n) = self
            .board
            .as_ref()
            .and_then(|b| b.with_prog_net(|s, _| s.changes()))
        else {
            return false;
        };
        std::mem::replace(&mut self.net_seen, n) != n
    }

    /// After a restore: listeners listen again; connections are gone (ECONNRESET).
    pub(crate) fn sockets_restored(&mut self) {
        let mut listen = Vec::new();
        for desc in self.files.descs.values_mut() {
            if let Kind::Tcp(t) = &mut desc.kind {
                match t.state {
                    TcpState::Listening { .. } => listen.push(t.port),
                    TcpState::Conn { .. } => t.state = TcpState::Failed(ECONNRESET),
                    _ => {},
                }
            }
        }
        for port in listen {
            self.net(|s, _| s.prog_listen(port));
        }
    }
}
