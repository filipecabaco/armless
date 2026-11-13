//! Sockets, with no kernel: AF_UNIX (stream and datagram, paths and abstract names, socketpair)
//! and AF_INET and AF_INET6 (TCP and UDP) are objects of the machine's state (`Net`, in its
//! `Files`), the way pipes are: a descriptor refers to a socket through its description, so a
//! fork's child and a program execve runs have the sockets their parent's descriptors do (one
//! with close-on-exec not), a listener is the one socket for every process that holds it, and a
//! blocked call of any process runs again when something changes. Two sockets meet in memory.
//!
//! What is not the machine's goes to the host, as the guest's TCP connections do (`Board::
//! local_*`, the network's stream calls: `Event::TcpOpen` out, `tcp_accept` or `tcp_refuse` back,
//! `tcp_send`, `tcp_close`): a `connect` to an address that is not loopback opens a stream there,
//! a host's `Board::tcp_connect(port)` reaches a socket a process listens on, and a datagram
//! to port 53 is answered by the network's name service. Without a network (a machine built with
//! `net: false`) only the processes' own sockets exist.
//!
//! Ancillary data goes between unix sockets, streams and datagrams: SCM_RIGHTS passes descriptors
//! (a datagram carries them, a stream the bytes of the send that had them), and a socket with
//! SO_PASSCRED receives SCM_CREDENTIALS, who sent what it reads (the sender's pid, uid and gid, or
//! what it gave, as root may). Not here: other ancillary data of a unix socket (EINVAL), UDP to
//! the outside (but DNS), IPv6 to the outside, raw and packet sockets, SOCK_SEQPACKET but a
//! socketpair's, MSG_OOB, and a stream's backpressure towards the host (what a process writes
//! waits in the network's outbox).

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use crate::errno::{
    EADDRINUSE, EADDRNOTAVAIL, EAFNOSUPPORT, EAGAIN, EALREADY, EBADF, ECONNREFUSED, ECONNRESET,
    EDESTADDRREQ, EEXIST, EFAULT, EINPROGRESS, EINVAL, EISCONN, EMFILE, EMSGSIZE, ENETUNREACH,
    ENOPROTOOPT, ENOSYS, ENOTCONN, ENOTSOCK, EOPNOTSUPP, EPERM, EPIPE, EPROTONOSUPPORT,
    ESOCKTNOSUPPORT, ESRCH,
};
use crate::files::{self, Desc, Got, Kind, O_NONBLOCK, S_IFSOCK};
use crate::host::Instant;
use crate::ser::{R, W};
use crate::{Process, Ret};

const AF_UNIX: u16 = 1;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;
/// A netlink route socket (netlink.rs)
const AF_NETLINK: u16 = 16;

const SOCK_STREAM: u8 = 1;
const SOCK_DGRAM: u8 = 2;
/// A connection that keeps its messages' boundaries (AF_UNIX pairs: socketpair)
const SOCK_SEQPACKET: u8 = 5;

/// Whether a socket of type `ty` carries messages (a datagram, or a seqpacket's) rather than a
/// stream of bytes
fn messages(ty: u8) -> bool {
    matches!(ty, SOCK_DGRAM | SOCK_SEQPACKET)
}


const MSG_PEEK: u64 = 0x2;
const MSG_TRUNC: u64 = 0x20;
const MSG_DONTWAIT: u64 = 0x40;
const MSG_NOSIGNAL: u64 = 0x4000;
const MSG_CTRUNC: u32 = 0x8;
const MSG_CMSG_CLOEXEC: u64 = 0x4000_0000;

const SOL_SOCKET: u32 = 1;
const SO_PASSCRED: i32 = 16;
const SCM_RIGHTS: u32 = 1;
const SCM_CREDENTIALS: u32 = 2;
/// Descriptors one message carries at most (the kernel's SCM_MAX_FD)
const SCM_MAX_FD: usize = 253;
/// sizeof(struct cmsghdr)
const CMSG_HDR: usize = 16;
/// The credentials of bytes nobody is known to have sent (from a state that did not keep them):
/// Linux's for a message that came with none, pid 0 and the overflow uid and gid
const NOBODY: [u32; 3] = [0, 65534, 65534];

/// What a stream holds for its reader at most, as a pipe does
const STREAM_MAX: usize = 64 << 10;
/// Datagrams waiting at a socket at most
const DGRAM_MAX: usize = 128;
/// What one write to the host takes
const HOST_WRITE_MAX: usize = 256 << 10;
/// The first port of those chosen for a socket that has none
const EPHEMERAL: u16 = 49152;
/// The guest's address on the network the host gives it, and the router's
pub(crate) const GUEST_IP: [u8; 4] = [192, 168, 86, 100];
const ROUTER_IP: [u8; 4] = [192, 168, 86, 1];

fn e(errno: i64) -> Ret {
    Ret::Val(-errno)
}

/// struct sockaddr_nl: family, pad, port, groups
fn sa_nl(pid: u32) -> Vec<u8> {
    let mut b = vec![AF_NETLINK as u8, 0, 0, 0];
    b.extend_from_slice(&pid.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b
}

/// The ancillary data a message is sent with: the descriptions of SCM_RIGHTS, and the
/// SCM_CREDENTIALS the sender gave (pid, uid, gid; none: its own).
#[derive(Default)]
pub(crate) struct Cm {
    pub rights: Vec<u32>,
    pub cred: Option<[u32; 3]>,
}

/// Puts a control message of SOL_SOCKET and `ty` with its data (padded to 8) in `out`, as much as
/// a buffer of `room` bytes holds, as Linux's put_cmsg: whether it was cut.
fn put_cmsg(out: &mut Vec<u8>, room: usize, ty: u32, data: &[u8]) -> bool {
    let left = room - out.len();
    if left < CMSG_HDR {
        return true;
    }
    let full = CMSG_HDR + data.len();
    let l = full.min(left);
    out.extend_from_slice(&(l as u64).to_le_bytes());
    out.extend_from_slice(&SOL_SOCKET.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&data[..l - CMSG_HDR]);
    out.resize(out.len() + full.next_multiple_of(8).min(left) - l, 0);
    l < full
}

/// Descriptions passed over a stream: the bytes of the send that carried them, `at..end` of the
/// stream, and the descriptions (a reference each until received)
#[derive(Clone, Debug)]
struct Rights {
    at: u64,
    end: u64,
    ds: Vec<u32>,
}

/// A datagram waiting at a socket: where from, its bytes, the descriptions it carries (a reference
/// each until it is received) and who sent it (pid, uid, gid).
#[derive(Clone)]
struct Dgram {
    from: Vec<u8>,
    data: Vec<u8>,
    rights: Vec<u32>,
    cred: [u32; 3],
}

/// A socket.
#[derive(Default)]
pub(crate) struct Sock {
    domain: u8,
    ty: u8,
    listening: bool,
    /// A stream with its peer (in memory or at the host), or a datagram socket `connect` gave an
    /// address
    connected: bool,
    /// A connection to the host is not answered yet
    connecting: bool,
    /// A blocking `connect` is waiting for that answer
    bc: bool,
    /// The sockaddr it is bound to (empty: not bound)
    name: Vec<u8>,
    /// What `Net::names` knows it by
    key: Option<Vec<u8>>,
    /// The inode of the path it is bound to, which it holds a reference of
    ino: Option<u64>,
    /// The sockaddr of the other end
    peer_name: Vec<u8>,
    /// The other end, a socket in memory (a datagram socket of a pair too)
    peer: Option<u32>,
    /// The stream at the host
    host: Option<u32>,
    /// Stream bytes not read yet
    rx: VecDeque<u8>,
    /// Where `rx` begins in the stream, for `rights` (not saved: a restored socket counts from 0)
    rx_base: u64,
    /// Descriptions passed with SCM_RIGHTS, by where in the stream the bytes sent with them begin.
    /// They hold a reference each until they are received.
    rights: VecDeque<Rights>,
    /// Who sent the stream's bytes (pid, uid, gid), by where in the stream each sender's begin:
    /// a read for a socket with SO_PASSCRED does not run on into another sender's
    creds: VecDeque<(u64, [u32; 3])>,
    /// Datagrams not read yet
    msgs: VecDeque<Dgram>,
    /// Connections made and not accepted yet, and how many may wait
    backlog: VecDeque<u32>,
    max_backlog: usize,
    /// The peer sent all it will (a read ends once rx is empty)
    fin: bool,
    /// The peer is closed (a write fails)
    gone: bool,
    rd_shut: bool,
    wr_shut: bool,
    /// A failure SO_ERROR gives once
    err: i64,
    opts: BTreeMap<(i32, i32), Vec<u8>>,
}

impl Sock {
    fn inet(&self) -> bool {
        self.domain != AF_UNIX as u8
    }

    /// Whether SO_PASSCRED asks for the senders' credentials with what it reads
    fn passcred(&self) -> bool {
        self.opts
            .get(&(SOL_SOCKET as i32, SO_PASSCRED))
            .is_some_and(|v| v.iter().any(|&b| b != 0))
    }

    fn port(&self) -> u16 {
        match self.name.get(2..4) {
            Some(p) if self.inet() => u16::from_be_bytes([p[0], p[1]]),
            _ => 0,
        }
    }

    /// The timeout SO_RCVTIMEO (20) or SO_SNDTIMEO (21) says
    fn timeout(&self, opt: i32) -> Option<Duration> {
        let v = self.opts.get(&(1, opt))?;
        let sec = i64::from_le_bytes(v.get(..8)?.try_into().ok()?);
        let usec = i64::from_le_bytes(v.get(8..16)?.try_into().ok()?);
        let d = Duration::new(sec.max(0) as u64, (usec.max(0) as u32 % 1_000_000) * 1000);
        (!d.is_zero()).then_some(d)
    }
}

/// What a closed socket leaves for the host's side to do.
pub(crate) enum Op {
    /// A stream at the host is done with
    Drop(u32),
    /// A port is not listened on any more
    Unlisten(u16),
    /// A reference of an inode a socket was bound to
    FsDrop(u64),
    /// A description passed over a socket that closed before anyone received it
    Release(u32),
}

/// Every socket, and the names they are bound to.
#[derive(Default)]
pub(crate) struct Net {
    socks: BTreeMap<u32, Sock>,
    next_id: u32,
    /// Bound names: 'a' + abstract name, 'i' + inode, 'p' + path (a machine with no
    /// filesystem), 't' or 'u' + port (TCP, UDP) -> socket
    names: BTreeMap<Vec<u8>, u32>,
    next_port: u16,
    /// For `Process::flush_net`
    pub ops: Vec<Op>,
    /// What `Board::local_gen` was when the process last looked (not saved)
    pub seen_gen: u64,
    /// The descriptions the last receive took with its bytes, for recvmsg to install (released
    /// by the next receive if a plain read took them: not saved)
    pub taken: Vec<u32>,
    /// Who sent what the last receive took, when its socket has SO_PASSCRED, for recvmsg (not
    /// saved)
    pub cred: Option<[u32; 3]>,
}

fn port_key(proto: u8, port: u16) -> Vec<u8> {
    vec![proto, (port >> 8) as u8, port as u8]
}

fn ino_key(ino: u64) -> Vec<u8> {
    let mut k = vec![b'i'];
    k.extend_from_slice(&ino.to_le_bytes());
    k
}

fn sa_in(ip: [u8; 4], port: u16) -> Vec<u8> {
    let mut b = vec![0; 16];
    b[0] = AF_INET as u8;
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[4..8].copy_from_slice(&ip);
    b
}

fn sa_in6(ip: [u8; 16], port: u16) -> Vec<u8> {
    let mut b = vec![0; 28];
    b[0] = AF_INET6 as u8;
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[8..24].copy_from_slice(&ip);
    b
}

/// A parsed sockaddr
enum Addr {
    /// A path, or an abstract name (starting with NUL)
    Unix(Vec<u8>),
    In([u8; 4], u16),
    In6([u8; 16], u16),
}

fn parse(sa: &[u8]) -> Result<Addr, i64> {
    if sa.len() < 2 {
        return Err(EINVAL);
    }
    match u16::from_le_bytes([sa[0], sa[1]]) {
        AF_UNIX => {
            let p = &sa[2..];
            Ok(Addr::Unix(if p.first() == Some(&0) {
                p.to_vec()
            } else {
                p[..p.iter().position(|&b| b == 0).unwrap_or(p.len())].to_vec()
            }))
        },
        AF_INET if sa.len() >= 8 => Ok(Addr::In(
            sa[4..8].try_into().unwrap(),
            u16::from_be_bytes([sa[2], sa[3]]),
        )),
        AF_INET6 if sa.len() >= 24 => Ok(Addr::In6(
            sa[8..24].try_into().unwrap(),
            u16::from_be_bytes([sa[2], sa[3]]),
        )),
        AF_INET | AF_INET6 => Err(EINVAL),
        _ => Err(EAFNOSUPPORT),
    }
}

/// The IPv4 address of an address that is one (a mapped IPv6 address too)
fn v4(a: &Addr) -> Option<([u8; 4], u16)> {
    match a {
        Addr::In(ip, p) => Some((*ip, *p)),
        Addr::In6(ip, p) if ip[..10] == [0; 10] && ip[10..12] == [0xff, 0xff] => {
            Some((ip[12..16].try_into().unwrap(), *p))
        },
        _ => None,
    }
}

/// Whether the address is the machine's own (loopback, unspecified, its address)
fn is_local(a: &Addr) -> bool {
    match a {
        Addr::Unix(_) => true,
        Addr::In6(ip, _) if *ip == [0; 16] || ip[..15] == [0; 15] && ip[15] == 1 => true,
        _ => v4(a).is_some_and(|(ip, _)| ip[0] == 127 || ip == [0; 4] || ip == GUEST_IP),
    }
}

/// The name a connection to this bound name has at its other end (the unspecified address is
/// the loopback)
fn concrete(sa: &[u8]) -> Vec<u8> {
    let mut b = sa.to_vec();
    match parse(sa) {
        Ok(Addr::In(ip, port)) if ip == [0; 4] => b = sa_in([127, 0, 0, 1], port),
        Ok(Addr::In6(ip, port)) if ip == [0; 16] => {
            let mut lo = [0; 16];
            lo[15] = 1;
            b = sa_in6(lo, port);
        },
        _ => {},
    }
    b
}

impl Net {
    fn add(&mut self, domain: u8, ty: u8) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.socks.insert(
            id,
            Sock {
                domain,
                ty,
                ..Sock::default()
            },
        );
        id
    }

    /// The process looks at the host for changes: whether it has sockets there to look for
    pub fn hosted(&self) -> bool {
        self.socks
            .values()
            .any(|s| s.host.is_some() || s.listening && s.inet())
    }

    /// A free port for `proto`
    fn ephemeral(&mut self, proto: u8) -> u16 {
        if self.next_port < EPHEMERAL {
            self.next_port = EPHEMERAL;
        }
        for _ in 0..(65536 - EPHEMERAL as u32) {
            let p = self.next_port;
            self.next_port = if p == 65535 { EPHEMERAL } else { p + 1 };
            if !self.names.contains_key(&port_key(proto, p)) {
                return p;
            }
        }
        0
    }

    fn proto(&self, id: u32) -> u8 {
        if self.socks[&id].ty == SOCK_STREAM {
            b't'
        } else {
            b'u'
        }
    }

    /// Binds `id` to the port of its address (a free one if 0), in `sa`.
    fn bind_inet(&mut self, id: u32, mut sa: Vec<u8>) -> Result<(), i64> {
        let proto = self.proto(id);
        let mut port = u16::from_be_bytes([sa[2], sa[3]]);
        if port == 0 {
            port = self.ephemeral(proto);
            if port == 0 {
                return Err(EADDRINUSE);
            }
            sa[2..4].copy_from_slice(&port.to_be_bytes());
        }
        let key = port_key(proto, port);
        if self.names.contains_key(&key) {
            return Err(EADDRINUSE);
        }
        self.names.insert(key.clone(), id);
        let s = self.socks.get_mut(&id).unwrap();
        s.key = Some(key);
        s.name = sa;
        Ok(())
    }

    /// Binds a socket with no name to a free port of the loopback.
    fn autobind(&mut self, id: u32) {
        let s = &self.socks[&id];
        if !s.name.is_empty() || s.domain == AF_UNIX as u8 {
            return;
        }
        let sa = if s.domain == AF_INET6 as u8 {
            let mut lo = [0; 16];
            lo[15] = 1;
            sa_in6(lo, 0)
        } else {
            sa_in([127, 0, 0, 1], 0)
        };
        let _ = self.bind_inet(id, sa);
    }

    /// Closes the socket: its peer sees it go, its name is free.
    /// A socket in a few words, for a host looking at what a program waits on: its type, its
    /// address and its peer's, what is buffered, and how it stands
    pub(crate) fn brief(&self, id: u32) -> String {
        let Some(s) = self.socks.get(&id) else {
            return format!("sock {id} gone");
        };
        let addr = |sa: &[u8]| match sa {
            [2, 0, p0, p1, a, b, c, d, ..] => format!("{a}.{b}.{c}.{d}:{}", u16::from_be_bytes([*p0, *p1])),
            [1, 0, rest @ ..] => format!("unix:{}", String::from_utf8_lossy(rest).trim_end_matches('\0')),
            [] => "-".into(),
            _ => "?".into(),
        };
        format!(
            "sock{id}[{} {}->{} peer={:?} host={:?} rx={} msgs={} backlog={}{}{}{}{}]",
            s.ty,
            addr(&s.name),
            addr(&s.peer_name),
            s.peer,
            s.host,
            s.rx.len(),
            s.msgs.len(),
            s.backlog.len(),
            if s.listening { " listening" } else { "" },
            if s.connecting { " connecting" } else { "" },
            if s.fin { " fin" } else { "" },
            if s.gone { " gone" } else { "" },
        )
    }

    pub fn close(&mut self, id: u32) {
        let Some(s) = self.socks.remove(&id) else {
            return;
        };
        if let Some(k) = &s.key {
            if self.names.get(k) == Some(&id) {
                self.names.remove(k);
            }
        }
        if let Some(ino) = s.ino {
            self.ops.push(Op::FsDrop(ino));
        }
        let unread = s.msgs.iter().map(|m| &m.rights);
        for ds in s.rights.iter().map(|r| &r.ds).chain(unread) {
            self.ops.extend(ds.iter().map(|&d| Op::Release(d)));
        }
        if let Some(h) = s.host {
            self.ops.push(Op::Drop(h));
        }
        if s.listening {
            if s.inet() {
                self.ops.push(Op::Unlisten(s.port()));
            }
            for c in s.backlog {
                self.close(c);
            }
        }
        if let Some(p) = s.peer.and_then(|p| self.socks.get_mut(&p)) {
            if p.peer == Some(id) {
                p.peer = None;
                p.gone = true;
                p.fin = true;
            }
        }
    }

    pub fn save(&self, w: &mut W) {
        w.u(self.next_id as u64);
        w.u(self.next_port as u64);
        w.u(self.socks.len() as u64);
        for (&id, s) in &self.socks {
            w.u(id as u64);
            w.u(s.domain as u64);
            w.u(s.ty as u64);
            let bits = [
                s.listening,
                s.connecting,
                s.connected,
                s.fin,
                s.gone,
                s.rd_shut,
                s.wr_shut,
                s.host.is_some(),
            ];
            w.u(bits
                .iter()
                .enumerate()
                .fold(0, |a, (i, &b)| a | (b as u64) << i));
            w.u(s.err as u64);
            w.b(&s.name);
            w.b(s.key.as_deref().unwrap_or(&[]));
            w.u(s.ino.unwrap_or(u64::MAX));
            w.b(&s.peer_name);
            w.u(s.peer.map_or(u64::MAX, |p| p as u64));
            w.b(&s.rx.iter().copied().collect::<Vec<u8>>());
            w.u(s.msgs.len() as u64);
            for m in &s.msgs {
                w.b(&m.from);
                w.b(&m.data);
            }
            w.u(s.backlog.len() as u64);
            for &c in &s.backlog {
                w.u(c as u64);
            }
            w.u(s.max_backlog as u64);
            w.u(s.opts.len() as u64);
            for (&(l, n), v) in &s.opts {
                w.u(l as u32 as u64);
                w.u(n as u32 as u64);
                w.b(v);
            }
        }
        w.u(self.names.len() as u64);
        for (k, &id) in &self.names {
            w.b(k);
            w.u(id as u64);
        }
    }

    /// The descriptions on their way over sockets (SCM_RIGHTS): socket, where the send that
    /// carried them begins and (since ARMLUSR7) ends, by how far into its unread bytes, and the
    /// descriptions. (Since ARMLUSR6; `rx_base` starts again at 0.)
    pub fn save_rights(&self, w: &mut W) {
        let all: Vec<(u32, u64, u64, &Vec<u32>)> = self
            .socks
            .iter()
            .flat_map(|(&id, s)| {
                s.rights
                    .iter()
                    .map(move |r| (id, r.at - s.rx_base, r.end.saturating_sub(s.rx_base), &r.ds))
            })
            .collect();
        w.u(all.len() as u64);
        for (id, at, end, ds) in all {
            w.u(id as u64);
            w.u(at);
            w.u(end);
            w.u(ds.len() as u64);
            for &d in ds {
                w.u(d as u64);
            }
        }
    }

    pub fn load_rights(&mut self, r: &mut R, ver: usize) -> Result<(), String> {
        for _ in 0..r.u()? {
            let (id, at) = (r.u()? as u32, r.u()?);
            // ARMLUSR6 didn't keep where a send ends: to the next one's, or the stream's end
            let end = if ver >= 7 { r.u()? } else { u64::MAX };
            let ds = (0..r.u()?).map(|_| r.u().map(|d| d as u32)).collect::<Result<Vec<_>, _>>()?;
            let s = self.socks.get_mut(&id).ok_or("descriptors passed over a socket that isn't there")?;
            s.rights.push_back(Rights { at, end, ds });
        }
        for s in self.socks.values_mut() {
            let ends: Vec<u64> = (0..s.rights.len())
                .map(|i| s.rights.get(i + 1).map_or(s.rx.len() as u64, |n| n.at))
                .collect();
            for (r, e) in s.rights.iter_mut().zip(ends) {
                if r.end == u64::MAX {
                    r.end = e;
                }
            }
        }
        Ok(())
    }

    /// What the datagrams waiting at each socket carry (their descriptions and senders) and who
    /// sent a stream's unread bytes, by how far into them. (Since ARMLUSR7; `rx_base` starts again
    /// at 0, as `save_rights` says.)
    pub fn save_cm(&self, w: &mut W) {
        w.u(self.socks.len() as u64);
        for (&id, s) in &self.socks {
            w.u(id as u64);
            w.u(s.msgs.len() as u64);
            for m in &s.msgs {
                m.cred.iter().for_each(|&v| w.u(v as u64));
                w.u(m.rights.len() as u64);
                m.rights.iter().for_each(|&d| w.u(d as u64));
            }
            w.u(s.creds.len() as u64);
            for (at, c) in &s.creds {
                w.u(at.saturating_sub(s.rx_base));
                c.iter().for_each(|&v| w.u(v as u64));
            }
        }
    }

    pub fn load_cm(&mut self, r: &mut R, ver: usize) -> Result<(), String> {
        // Before, nobody was known to have sent what waits (`NOBODY`), and datagrams carried nothing
        if ver < 7 {
            return Ok(());
        }
        let cred = |r: &mut R| -> Result<[u32; 3], String> {
            Ok([r.u()? as u32, r.u()? as u32, r.u()? as u32])
        };
        for _ in 0..r.u()? {
            let id = r.u()? as u32;
            let s = self.socks.get_mut(&id).ok_or("messages of a socket that isn't there")?;
            let n = r.u()? as usize;
            if n != s.msgs.len() {
                return Err("a socket's datagrams aren't the ones saved".into());
            }
            for m in s.msgs.iter_mut() {
                m.cred = cred(r)?;
                m.rights = (0..r.u()?).map(|_| r.u().map(|d| d as u32)).collect::<Result<_, _>>()?;
            }
            for _ in 0..r.u()? {
                let at = r.u()?;
                s.creds.push_back((at, cred(r)?));
            }
        }
        Ok(())
    }

    /// The sockets from `save`. A stream that was at the host is not any more: the host that
    /// restores did not open it, so it reads as closed.
    pub fn load(r: &mut R) -> Result<Net, String> {
        let mut n = Net {
            next_id: r.u()? as u32,
            next_port: r.u()? as u16,
            ..Net::default()
        };
        for _ in 0..r.u()? {
            let id = r.u()? as u32;
            let mut s = Sock {
                domain: r.u()? as u8,
                ty: r.u()? as u8,
                ..Sock::default()
            };
            let bits = r.u()?;
            let bit = |i: u32| bits >> i & 1 != 0;
            (s.listening, s.connecting, s.connected) = (bit(0), bit(1), bit(2));
            (s.fin, s.gone, s.rd_shut, s.wr_shut) = (bit(3), bit(4), bit(5), bit(6));
            s.err = r.u()? as i64;
            s.name = r.b()?.to_vec();
            let key = r.b()?;
            s.key = (!key.is_empty()).then(|| key.to_vec());
            s.ino = Some(r.u()?).filter(|&i| i != u64::MAX);
            s.peer_name = r.b()?.to_vec();
            s.peer = Some(r.u()?).filter(|&p| p != u64::MAX).map(|p| p as u32);
            s.rx = r.b()?.iter().copied().collect();
            for _ in 0..r.u()? {
                let (from, data) = (r.b()?.to_vec(), r.b()?.to_vec());
                s.msgs.push_back(Dgram {
                    from,
                    data,
                    rights: Vec::new(),
                    cred: NOBODY,
                });
            }
            for _ in 0..r.u()? {
                s.backlog.push_back(r.u()? as u32);
            }
            s.max_backlog = r.u()? as usize;
            for _ in 0..r.u()? {
                let (l, o) = (r.u()? as u32 as i32, r.u()? as u32 as i32);
                s.opts.insert((l, o), r.b()?.to_vec());
            }
            if bit(7) {
                if s.connecting {
                    s.err = ECONNRESET;
                }
                (s.connecting, s.gone, s.fin) = (false, true, true);
            }
            n.socks.insert(id, s);
        }
        for _ in 0..r.u()? {
            let k = r.b()?.to_vec();
            n.names.insert(k, r.u()? as u32);
        }
        Ok(n)
    }
}

/// Where a datagram or a message of a stream came from, and whether it was cut.
type Recvd = (Vec<u8>, Vec<u8>, bool);

impl Process {
    /// The ports the processes listen on are the network's to forward to, and a network it
    /// shares with a guest hands it the host's connections: at the start, and after a restore.
    pub(crate) fn net_attach(&self) {
        let Some(b) = &self.board else { return };
        if !b.local_enable() {
            return;
        }
        for s in self.files.net.socks.values() {
            if s.listening && s.inet() {
                b.local_listen(s.port());
            }
        }
    }

    /// What closing sockets left for the host's side.
    pub(crate) fn flush_net(&mut self) {
        if self.files.net.ops.is_empty() {
            return;
        }
        for op in std::mem::take(&mut self.files.net.ops) {
            match op {
                Op::Drop(h) => {
                    if let Some(b) = &self.board {
                        b.local_drop(h);
                    }
                },
                Op::Unlisten(p) => {
                    if let Some(b) = &self.board {
                        b.local_unlisten(p);
                    }
                },
                Op::FsDrop(ino) => {
                    self.fs(|fs| fs.drop_ref(ino));
                },
                Op::Release(d) => self.release_desc(d),
            }
        }
    }

    /// The machine is done: every socket closes, whichever process had it, and the host's side
    /// of them goes.
    pub(crate) fn net_exit(&mut self) {
        let socks: Vec<u32> = self
            .files
            .descs
            .values()
            .filter_map(|d| match d.kind {
                Kind::Sock(id) => Some(id),
                _ => None,
            })
            .collect();
        for id in socks {
            self.files.net.close(id);
        }
        self.flush_net();
    }

    /// Whether the host did something to a stream or a listener since the process looked.
    pub(crate) fn net_changed(&mut self) -> bool {
        if !self.files.net.hosted() {
            return false;
        }
        let Some(b) = &self.board else { return false };
        let g = b.local_gen();
        std::mem::replace(&mut self.files.net.seen_gen, g) != g
    }

    // ------------------------------------------------------------ helpers

    fn sock_id(&self, fd: i32) -> Result<u32, Ret> {
        match self.files.get(fd) {
            None => Err(e(EBADF)),
            Some(Desc {
                kind: Kind::Sock(id),
                ..
            }) => Ok(*id),
            _ => Err(e(ENOTSOCK)),
        }
    }

    fn sockaddr(&self, addr: u64, len: u64) -> Result<Vec<u8>, Ret> {
        if !(2..=128).contains(&len) {
            return Err(e(EINVAL));
        }
        self.bytes_at(addr, len as usize).ok_or(e(EFAULT))
    }

    /// The sockaddr `sa` to the program: `len` is a pointer to the room it has, and gets what
    /// the address takes.
    fn put_sockaddr(&self, addr: u64, len: u64, sa: &[u8]) -> bool {
        let Some(cap) = self.u32_at(len) else {
            return false;
        };
        let n = sa.len().min(cap as usize);
        self.write(addr, &sa[..n]) && self.put_u32(len, sa.len() as u32)
    }

    /// What a call that would block does: fails, or waits (until the socket's timeout).
    fn would_block(&self, id: u32, nonblock: bool, opt: i32) -> Ret {
        if nonblock {
            return e(EAGAIN);
        }
        let timeout = self.files.net.socks.get(&id).and_then(|s| s.timeout(opt));
        Ret::Retry {
            deadline: self
                .resume_deadline
                .or_else(|| timeout.and_then(|t| Instant::now().checked_add(t))),
            on_timeout: -EAGAIN,
        }
    }

    /// The inode a unix path names, as a name key.
    fn unix_key(&self, path: &[u8]) -> Result<Vec<u8>, Ret> {
        if path.first() == Some(&0) {
            return Ok([&b"a"[..], path].concat());
        }
        match self.fs(|fs| {
            files::resolve(fs, self.p.cwd, path, true).map(|ino| (ino, fs.get(ino).map(|i| i.mode)))
        }) {
            None => Ok([&b"p"[..], path].concat()),
            Some(Ok((ino, Some(mode)))) if mode & armless::dev::fs::S_IFMT == S_IFSOCK => {
                Ok(ino_key(ino))
            },
            Some(Ok(_)) => Err(e(ECONNREFUSED)),
            Some(Err(err)) => Err(Ret::Val(err)),
        }
    }

    /// A host stream's state, and the sockets that wait on it, updated.
    fn sock_refresh(&mut self, id: u32) {
        let Some(b) = self.board.clone() else { return };
        let net = &mut self.files.net;
        let Some(s) = net.socks.get_mut(&id) else {
            return;
        };
        let Some(h) = s.host else { return };
        if !s.connecting {
            return;
        }
        let st = b.local_state(h);
        if st.connecting {
            return;
        }
        s.connecting = false;
        if st.failed {
            s.err = ECONNREFUSED;
            s.host = None;
            net.ops.push(Op::Drop(h));
        } else {
            s.connected = true;
        }
    }

    // ------------------------------------------------------------ readiness

    /// (readable, writable, hung up), for poll, select and epoll
    pub(crate) fn sock_ready(&self, id: u32) -> (bool, bool, bool) {
        let Some(s) = self.files.net.socks.get(&id) else {
            return (true, true, true);
        };
        if s.err != 0 {
            return (true, true, true);
        }
        if s.listening {
            let waiting = s.inet()
                && self
                    .board
                    .as_ref()
                    .is_some_and(|b| b.local_pending(s.port()) > 0);
            return (!s.backlog.is_empty() || waiting, false, false);
        }
        if s.ty == SOCK_SEQPACKET {
            // Readable at the end too (a read gives 0), hung up once its peer is closed
            return (!s.msgs.is_empty() || s.fin, !s.gone, s.gone);
        }
        if s.ty == SOCK_DGRAM {
            return (!s.msgs.is_empty(), true, false);
        }
        if let (Some(h), Some(b)) = (s.host, &self.board) {
            let st = b.local_state(h);
            return if st.connecting {
                (false, false, false)
            } else if st.failed {
                (true, true, true)
            } else {
                (
                    st.readable > 0 || st.eof || s.rd_shut,
                    !s.wr_shut,
                    st.eof && s.wr_shut,
                )
            };
        }
        if !s.connected && !s.gone {
            return (false, true, true);
        }
        let room = s
            .peer
            .and_then(|p| self.files.net.socks.get(&p))
            .is_some_and(|p| p.rx.len() < STREAM_MAX && !p.rd_shut);
        (
            !s.rx.is_empty() || s.fin || s.rd_shut,
            room || s.gone || s.wr_shut,
            s.gone || s.fin && s.wr_shut,
        )
    }

    /// The bytes a read would take at once
    pub(crate) fn sock_pending(&self, id: u32) -> usize {
        let Some(s) = self.files.net.socks.get(&id) else {
            return 0;
        };
        if messages(s.ty) {
            return s.msgs.front().map_or(0, |m| m.data.len());
        }
        match (s.host, &self.board) {
            (Some(h), Some(b)) => b.local_state(h).readable,
            _ => s.rx.len(),
        }
    }

    // ------------------------------------------------------------ reading and writing

    /// Up to `max` bytes (a whole datagram: cut if longer, with the rest lost) that arrived at
    /// socket `id`, and where from; or what the call must do.
    pub(crate) fn sock_recv(
        &mut self,
        fd: i32,
        id: u32,
        max: usize,
        flags: u64,
    ) -> Result<Recvd, Ret> {
        let nonblock = self.nonblock(fd) || flags & MSG_DONTWAIT != 0;
        self.sock_refresh(id);
        // Descriptions a plain read took (Linux closes them): released now
        for d in std::mem::take(&mut self.files.net.taken) {
            self.release_desc(d);
        }
        self.files.net.cred = None;
        let peek = flags & MSG_PEEK != 0;
        let board = self.board.clone();
        let Some(s) = self.files.net.socks.get_mut(&id) else {
            return Err(e(EBADF));
        };
        if s.listening {
            return Err(e(EINVAL));
        }
        let passcred = s.passcred();
        if messages(s.ty) {
            let Some(m) = (if peek { s.msgs.front().cloned() } else { s.msgs.pop_front() }) else {
                // A seqpacket's end: its peer closed, and nothing is left to read
                if s.ty == SOCK_SEQPACKET && s.fin {
                    return Ok((Vec::new(), Vec::new(), false));
                }
                return Err(self.would_block(id, nonblock, 20));
            };
            // The descriptors go with the datagram, cut or not; a peek gives copies (a reference
            // each of its own), as Linux's does
            if peek {
                m.rights.iter().for_each(|&d| self.files.hold(d));
            }
            self.files.net.taken = m.rights;
            self.files.net.cred = passcred.then_some(m.cred);
            let trunc = m.data.len() > max;
            if !peek {
                self.poke();
            }
            return Ok((m.data[..max.min(m.data.len())].to_vec(), m.from, trunc));
        }
        if s.err != 0 {
            let err = std::mem::take(&mut s.err);
            return Err(e(err));
        }
        if !s.connected && !s.gone {
            return Err(if s.connecting {
                self.would_block(id, nonblock, 20)
            } else {
                e(ENOTCONN)
            });
        }
        if s.rd_shut {
            return Ok((Vec::new(), Vec::new(), false));
        }
        let data = match (s.host, &board) {
            (Some(h), Some(b)) => {
                let st = b.local_state(h);
                if st.readable > 0 {
                    b.local_read(h, max)
                } else if st.failed {
                    return Err(e(ECONNRESET));
                } else if st.eof {
                    return Ok((Vec::new(), Vec::new(), false));
                } else {
                    return Err(self.would_block(id, nonblock, 20));
                }
            },
            _ => {
                if s.rx.is_empty() {
                    return if s.fin || s.gone {
                        Ok((Vec::new(), Vec::new(), false))
                    } else {
                        Err(self.would_block(id, nonblock, 20))
                    };
                }
                let start = s.rx_base;
                let mut n = s.rx.len().min(max);
                // A read that reaches the bytes of a send that carried descriptors takes them
                // and ends with that send, as Linux's (unix_stream_read_generic)
                if let Some(r) = s.rights.iter().find(|r| r.end > start && r.at < start + n as u64) {
                    n = n.min((r.end - start) as usize);
                }
                // One that wants to know who sent them stops before another sender's bytes
                let cred = passcred.then(|| {
                    if let Some(&(at, _)) = s.creds.iter().find(|c| c.0 > start) {
                        n = n.min((at - start) as usize);
                    }
                    s.creds.iter().rfind(|c| c.0 <= start).map_or(NOBODY, |c| c.1)
                });
                let end = start + n as u64;
                let data = if peek {
                    // A peek at the bytes descriptors came with gives copies of them
                    let copies: Vec<u32> = s
                        .rights
                        .iter()
                        .take_while(|r| r.at < end)
                        .flat_map(|r| r.ds.iter().copied())
                        .collect();
                    let data = s.rx.iter().take(n).copied().collect();
                    copies.iter().for_each(|&d| self.files.hold(d));
                    self.files.net.taken = copies;
                    data
                } else {
                    s.rx_base += n as u64;
                    // The senders whose bytes are all read are done with
                    while s.creds.len() > 1 && s.creds[1].0 <= end {
                        s.creds.pop_front();
                    }
                    let mut took = Vec::new();
                    while s.rights.front().is_some_and(|r| r.at < end) {
                        took.extend(s.rights.pop_front().unwrap().ds);
                    }
                    let data = s.rx.drain(..n).collect();
                    self.files.net.taken = took;
                    data
                };
                self.files.net.cred = cred;
                data
            },
        };
        self.poke();
        Ok((data, Vec::new(), false))
    }

    /// The descriptions the last receive took, which nothing installs (a read or recvfrom),
    /// closed now, as Linux closes them
    pub(crate) fn drop_taken(&mut self) {
        for d in std::mem::take(&mut self.files.net.taken) {
            self.release_desc(d);
        }
    }

    /// One reference fewer to description `d`, as close gives one up
    pub(crate) fn release_desc(&mut self, d: u32) {
        if let Some(Some(ino)) = self.files.release(d) {
            self.fs(|fs| fs.drop_ref(ino));
        }
        self.poke();
    }

    /// Sends `data` on socket `id` (to `dest` for a datagram socket with no address of its own):
    /// how much it took, or what the call must do. A stream whose other end is gone is EPIPE
    /// with SIGPIPE for the thread, as Linux's TCP and unix streams send it, unless the call
    /// says MSG_NOSIGNAL.
    pub(crate) fn sock_send(
        &mut self,
        fd: i32,
        id: u32,
        data: &[u8],
        flags: u64,
        dest: Option<Vec<u8>>,
        cm: &Cm,
    ) -> Result<usize, Ret> {
        let stream = self.files.net.socks.get(&id).is_some_and(|s| s.ty == SOCK_STREAM);
        match self.sock_send_now(fd, id, data, flags, dest, cm) {
            Err(Ret::Val(e)) if e == -EPIPE && stream && flags & MSG_NOSIGNAL == 0 => {
                Err(self.broken_pipe())
            },
            r => r,
        }
    }

    fn sock_send_now(
        &mut self,
        fd: i32,
        id: u32,
        data: &[u8],
        flags: u64,
        dest: Option<Vec<u8>>,
        cm: &Cm,
    ) -> Result<usize, Ret> {
        let nonblock = self.nonblock(fd) || flags & MSG_DONTWAIT != 0;
        self.sock_refresh(id);
        let board = self.board.clone();
        // Who sends: the process and its real ids, or what it gave (sendmsg checked it)
        let c = self.p.creds;
        let who = cm.cred.unwrap_or([self.p.pid as u32, c.uid[0], c.gid[0]]);
        let rights = &cm.rights[..];
        let Some(s) = self.files.net.socks.get_mut(&id) else {
            return Err(e(EBADF));
        };
        // Descriptors go only between unix sockets, which are all in memory
        if !rights.is_empty() && (s.host.is_some() || s.inet()) {
            return Err(e(EINVAL));
        }
        if s.domain as u16 == AF_NETLINK {
            // A request to the kernel: its answer is the next datagram to read
            let pid = u32::from_le_bytes(s.name[4..8].try_into().unwrap_or([0; 4]));
            let network = board.as_ref().is_some_and(|b| b.has_network());
            let reply = crate::netlink::answer(data, pid, network, GUEST_IP, ROUTER_IP);
            if !reply.is_empty() {
                // From the kernel: no descriptors, and pid 0
                s.msgs.push_back(Dgram { from: sa_nl(0), data: reply, rights: Vec::new(), cred: [0; 3] });
            }
            self.poke();
            return Ok(data.len());
        }
        if s.ty == SOCK_SEQPACKET && (s.gone || s.wr_shut) {
            return Err(e(EPIPE));
        }
        if messages(s.ty) {
            let (peer, default) = (s.peer, s.peer_name.clone());
            let msg = Dgram {
                from: Vec::new(),
                data: data.to_vec(),
                rights: rights.to_vec(),
                cred: who,
            };
            return self.dgram_send(id, nonblock, msg, peer, dest.unwrap_or(default));
        }
        if dest.is_some() && (s.connected || s.listening) {
            return Err(e(EISCONN));
        }
        if s.err != 0 {
            let err = std::mem::take(&mut s.err);
            return Err(e(err));
        }
        if s.connecting {
            return Err(self.would_block(id, nonblock, 21));
        }
        if !s.connected || s.listening {
            return Err(e(if s.inet() { EPIPE } else { ENOTCONN }));
        }
        if s.wr_shut || s.gone {
            return Err(e(EPIPE));
        }
        if let (Some(h), Some(b)) = (s.host, &board) {
            if b.local_state(h).failed {
                return Err(e(EPIPE));
            }
            let n = data.len().min(HOST_WRITE_MAX);
            if n > 0 && !b.local_write(h, &data[..n]) {
                return Err(e(EPIPE));
            }
            return Ok(n);
        }
        let Some(p) = s.peer.and_then(|p| self.files.net.socks.get_mut(&p)) else {
            return Err(e(EPIPE));
        };
        if p.rd_shut {
            return Err(e(EPIPE));
        }
        let room = STREAM_MAX.saturating_sub(p.rx.len());
        if room == 0 && !data.is_empty() {
            return Err(self.would_block(id, nonblock, 21));
        }
        let n = room.min(data.len());
        let at = p.rx_base + p.rx.len() as u64;
        if !rights.is_empty() {
            // With the first of these bytes; each holds its description until received
            p.rights.push_back(Rights { at, end: at + n as u64, ds: rights.to_vec() });
        }
        if n > 0 && p.creds.back().is_none_or(|c| c.1 != who) {
            p.creds.push_back((at, who));
        }
        p.rx.extend(&data[..n]);
        for &d in rights {
            self.files.hold(d);
        }
        self.poke();
        Ok(n)
    }

    /// A datagram to the socket `peer` of a pair, or to the address `dest`. Once it is queued,
    /// each description it carries has a reference of its own.
    fn dgram_send(
        &mut self,
        id: u32,
        nonblock: bool,
        mut msg: Dgram,
        peer: Option<u32>,
        dest: Vec<u8>,
    ) -> Result<usize, Ret> {
        let len = msg.data.len();
        if len > 65507 {
            return Err(e(EMSGSIZE));
        }
        if let Some(p) = peer {
            return match self.files.net.socks.get_mut(&p) {
                None => Err(e(ECONNREFUSED)),
                Some(t) if t.msgs.len() >= DGRAM_MAX => Err(self.would_block(id, nonblock, 21)),
                Some(t) => {
                    let rights = msg.rights.clone();
                    t.msgs.push_back(msg);
                    rights.iter().for_each(|&d| self.files.hold(d));
                    self.poke();
                    Ok(len)
                },
            };
        }
        if dest.is_empty() {
            return Err(e(EDESTADDRREQ));
        }
        let addr = parse(&dest).map_err(e)?;
        let domain = self.files.net.socks[&id].domain;
        let ok = matches!(
            (&addr, domain as u16),
            (Addr::Unix(_), AF_UNIX) | (Addr::In(..), AF_INET) | (Addr::In6(..), AF_INET6)
        );
        if !ok && !(domain as u16 == AF_INET6 && v4(&addr).is_some()) {
            return Err(e(EINVAL));
        }
        self.files.net.autobind(id);
        msg.from = self.files.net.socks[&id].name.clone();
        let (target, inet) = match &addr {
            Addr::Unix(path) => {
                let key = self.unix_key(path)?;
                (self.files.net.names.get(&key).copied(), false)
            },
            _ => {
                let port = match &addr {
                    Addr::In(_, p) | Addr::In6(_, p) => *p,
                    Addr::Unix(_) => 0,
                };
                let t = self.files.net.names.get(&port_key(b'u', port)).copied();
                if t.is_none() && port == 53 {
                    // The network's name service
                    if let Some(r) = self.board.as_ref().and_then(|b| b.local_dns(&msg.data)) {
                        let answer = Dgram {
                            from: dest,
                            data: r,
                            rights: Vec::new(),
                            cred: NOBODY,
                        };
                        self.files.net.socks.get_mut(&id).unwrap().msgs.push_back(answer);
                        self.poke();
                    }
                    return Ok(len);
                }
                if t.is_none() && !is_local(&addr) {
                    return Err(e(ENETUNREACH));
                }
                (t, true)
            },
        };
        match target.and_then(|t| self.files.net.socks.get_mut(&t)) {
            Some(t) if messages(t.ty) && t.msgs.len() < DGRAM_MAX => {
                let rights = msg.rights.clone();
                t.msgs.push_back(msg);
                rights.iter().for_each(|&d| self.files.hold(d));
                self.poke();
                Ok(len)
            },
            // UDP drops what a full socket cannot take, and what nobody listens for
            _ if inet => Ok(len),
            Some(t) if messages(t.ty) => Err(self.would_block(id, nonblock, 21)),
            _ => Err(e(ECONNREFUSED)),
        }
    }

    // ------------------------------------------------------------ the system calls

    /// socket, socketpair, bind, listen, accept, connect, getsockname, getpeername, sendto,
    /// recvfrom, setsockopt, getsockopt, shutdown, sendmsg, recvmsg, accept4, recvmmsg, sendmmsg
    pub(crate) fn net_syscall(&mut self, nr: u64, a: [u64; 6]) -> Ret {
        let fd = a[0] as i32;
        let r = match nr {
            198 => self.sys_socket(a[0], a[1], a[2]),
            199 => self.sys_socketpair(a[0], a[1], a[2], a[3]),
            200 => self.sys_bind(fd, a[1], a[2]),
            201 => self.sys_listen(fd, a[1] as i32),
            202 | 242 => self.sys_accept(fd, a[1], a[2], if nr == 242 { a[3] } else { 0 }),
            203 => self.sys_connect(fd, a[1], a[2]),
            204 | 205 => self.sys_name(fd, a[1], a[2], nr == 205),
            206 => self.sys_sendto(fd, a),
            207 => self.sys_recvfrom(fd, a),
            208 => self.sys_setsockopt(fd, a),
            209 => self.sys_getsockopt(fd, a),
            210 => self.sys_shutdown(fd, a[1]),
            211 => self
                .sendmsg_one(fd, a[1], a[2])
                .map_or_else(|r| r, |n| Ret::Val(n as i64)),
            212 => self
                .recvmsg_one(fd, a[1], a[2])
                .map_or_else(|r| r, |n| Ret::Val(n as i64)),
            243 => self.sys_mmsg(fd, a, true),
            269 => self.sys_mmsg(fd, a, false),
            _ => e(ENOSYS),
        };
        if matches!(r, Ret::Val(_)) && !matches!(nr, 198 | 204 | 205 | 208 | 209) {
            self.poke();
        }
        r
    }

    fn install_sock(&mut self, id: u32, flags: u64) -> i64 {
        let nb = if flags & O_NONBLOCK as u64 != 0 { O_NONBLOCK } else { 0 };
        let d = self.files.add(Kind::Sock(id), 2 | nb);
        self.new_fd(d, 0, flags & files::O_CLOEXEC as u64 != 0)
    }

    fn sys_socket(&mut self, domain: u64, ty: u64, proto: u64) -> Ret {
        let (kind, flags) = (ty & 0xf, ty & !0xf);
        if flags & !((O_NONBLOCK | files::O_CLOEXEC) as u64) != 0 {
            return e(EINVAL);
        }
        // A netlink route socket: a datagram socket the kernel (netlink.rs) answers
        if domain as u16 == AF_NETLINK {
            if !matches!(kind, 2 | 3) {
                return e(ESOCKTNOSUPPORT);
            }
            if proto != 0 {
                return e(EPROTONOSUPPORT);
            }
            let id = self.files.net.add(AF_NETLINK as u8, SOCK_DGRAM);
            let pid = self.p.pid as u32;
            if let Some(s) = self.files.net.socks.get_mut(&id) {
                s.name = sa_nl(pid);
            }
            return Ret::Val(self.install_sock(id, flags));
        }
        if !matches!(domain as u16, AF_UNIX | AF_INET | AF_INET6) {
            return e(EAFNOSUPPORT);
        }
        if !matches!(kind as u8, SOCK_STREAM | SOCK_DGRAM) {
            return e(ESOCKTNOSUPPORT);
        }
        let own = if kind as u8 == SOCK_STREAM { 6 } else { 17 };
        if proto != 0 && (domain as u16 == AF_UNIX || proto != own) {
            return e(EPROTONOSUPPORT);
        }
        let id = self.files.net.add(domain as u8, kind as u8);
        Ret::Val(self.install_sock(id, flags))
    }

    fn sys_socketpair(&mut self, domain: u64, ty: u64, proto: u64, sv: u64) -> Ret {
        let (kind, flags) = (ty & 0xf, ty & !0xf);
        if flags & !((O_NONBLOCK | files::O_CLOEXEC) as u64) != 0 || proto != 0 {
            return e(EINVAL);
        }
        if domain as u16 != AF_UNIX {
            return e(if matches!(domain as u16, AF_INET | AF_INET6) {
                EOPNOTSUPP
            } else {
                EAFNOSUPPORT
            });
        }
        // (SOCK_SEQPACKET as a pair only: socket() of one, to bind and connect, is not here yet)
        if !matches!(kind as u8, SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET) {
            return e(ESOCKTNOSUPPORT);
        }
        let net = &mut self.files.net;
        let (x, y) = (
            net.add(AF_UNIX as u8, kind as u8),
            net.add(AF_UNIX as u8, kind as u8),
        );
        for (me, other) in [(x, y), (y, x)] {
            let s = net.socks.get_mut(&me).unwrap();
            (s.peer, s.connected) = (Some(other), true);
        }
        let (fx, fy) = (self.install_sock(x, flags), self.install_sock(y, flags));
        if fx < 0 || fy < 0 {
            for fd in [fx, fy].into_iter().filter(|&f| f >= 0) {
                self.files.close(fd as i32);
            }
            return e(EMFILE);
        }
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&(fx as u32).to_le_bytes());
        b[4..].copy_from_slice(&(fy as u32).to_le_bytes());
        if !self.write(sv, &b) {
            self.files.close(fx as i32);
            self.files.close(fy as i32);
            return e(EFAULT);
        }
        Ret::Val(0)
    }

    fn sys_bind(&mut self, fd: i32, addr: u64, len: u64) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // A netlink socket's port is its process's pid, from the start
        if self.files.net.socks[&id].domain as u16 == AF_NETLINK {
            return Ret::Val(0);
        }
        let sa = match self.sockaddr(addr, len) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let a = match parse(&sa) {
            Ok(a) => a,
            Err(n) => return e(n),
        };
        let s = &self.files.net.socks[&id];
        if !s.name.is_empty() || s.key.is_some() {
            return e(EINVAL);
        }
        let domain = s.domain as u16;
        match a {
            Addr::Unix(path) => {
                if domain != AF_UNIX {
                    return e(EINVAL);
                }
                let (key, ino, name) = if path.is_empty() {
                    // Autobind: an abstract name of its own
                    let n = self.files.net.next_id;
                    let mut name = vec![0u8];
                    name.extend_from_slice(format!("{n:05x}").as_bytes());
                    let mut sa = vec![AF_UNIX as u8, 0];
                    sa.extend_from_slice(&name);
                    ([&b"a"[..], &name].concat(), None, sa)
                } else if path[0] == 0 {
                    ([&b"a"[..], &path].concat(), None, sa)
                } else {
                    match self.unix_create(&path) {
                        Ok((k, i)) => (k, i, sa),
                        Err(r) => return r,
                    }
                };
                let net = &mut self.files.net;
                if net.names.contains_key(&key) {
                    return e(EADDRINUSE);
                }
                net.names.insert(key.clone(), id);
                let s = net.socks.get_mut(&id).unwrap();
                (s.key, s.ino, s.name) = (Some(key), ino, name);
                Ret::Val(0)
            },
            a => {
                let ok = matches!(
                    (&a, domain),
                    (Addr::In(..), AF_INET) | (Addr::In6(..), AF_INET6)
                );
                if !ok {
                    return e(if domain == AF_UNIX { EINVAL } else { EAFNOSUPPORT });
                }
                if !is_local(&a) {
                    return e(EADDRNOTAVAIL);
                }
                match self.files.net.bind_inet(id, sa) {
                    Ok(()) => Ret::Val(0),
                    Err(n) => e(n),
                }
            },
        }
    }

    /// The inode a bound unix socket makes at `path` (EADDRINUSE if there is one), and its key.
    fn unix_create(&mut self, path: &[u8]) -> Result<(Vec<u8>, Option<u64>), Ret> {
        let (now, mode) = (self.now_secs(), files::S_IFSOCK | (0o777 & !self.p.umask));
        let cwd = self.p.cwd;
        let (uid, gid) = (self.p.creds.euid(), self.p.creds.egid());
        match self.fs(|fs| {
            let (dir, name) = files::resolve_parent(fs, cwd, path).map_err(Got::Err)?;
            let ino = match files::got(fs.create(dir, &name, mode, uid, gid, now)) {
                Got::Ok(i) => i,
                Got::Err(n) if n == -EEXIST => return Err(Got::Err(-EADDRINUSE)),
                Got::Err(n) => return Err(Got::Err(n)),
                Got::Need(b) => return Err(Got::Need(b)),
            };
            fs.add_ref(ino);
            Ok(ino)
        }) {
            None => Ok(([&b"p"[..], path].concat(), None)),
            Some(Ok(ino)) => Ok((ino_key(ino), Some(ino))),
            Some(Err(Got::Err(n))) => Err(Ret::Val(n)),
            Some(Err(Got::Need(b))) => Err(Ret::Need(b)),
            Some(Err(Got::Ok(()))) => unreachable!(),
        }
    }

    fn sys_listen(&mut self, fd: i32, backlog: i32) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let net = &mut self.files.net;
        let s = &net.socks[&id];
        if s.ty != SOCK_STREAM {
            return e(EOPNOTSUPP);
        }
        if s.listening {
            net.socks.get_mut(&id).unwrap().max_backlog = backlog.clamp(0, 4096) as usize;
            return Ret::Val(0);
        }
        if s.connected || s.connecting {
            return e(EINVAL);
        }
        if s.name.is_empty() {
            if !s.inet() {
                return e(EINVAL);
            }
            net.autobind(id);
            // A listener of the unspecified address, as a server that never bound is
            let s = net.socks.get_mut(&id).unwrap();
            s.name = if s.domain == AF_INET6 as u8 {
                sa_in6([0; 16], s.port())
            } else {
                sa_in([0; 4], s.port())
            };
        }
        let s = net.socks.get_mut(&id).unwrap();
        s.listening = true;
        s.max_backlog = backlog.clamp(0, 4096) as usize;
        if s.inet() {
            if let Some(b) = &self.board {
                b.local_listen(s.port());
            }
        }
        Ret::Val(0)
    }

    fn sys_accept(&mut self, fd: i32, addr: u64, len: u64, flags: u64) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        if flags & !((O_NONBLOCK | files::O_CLOEXEC) as u64) != 0 {
            return e(EINVAL);
        }
        let nonblock = self.nonblock(fd);
        let board = self.board.clone();
        let net = &mut self.files.net;
        let s = net.socks.get_mut(&id).unwrap();
        if !s.listening {
            return e(EINVAL);
        }
        let (inet, port, name, domain) = (s.inet(), s.port(), s.name.clone(), s.domain);
        let child = match s.backlog.pop_front() {
            Some(c) => c,
            None => {
                let h = board
                    .as_ref()
                    .filter(|_| inet)
                    .and_then(|b| b.local_accept(port));
                let Some(h) = h else {
                    return self.would_block(id, nonblock, 20);
                };
                // A connection the host opened
                let c = net.add(domain, SOCK_STREAM);
                let s = net.socks.get_mut(&c).unwrap();
                s.host = Some(h);
                s.connected = true;
                s.name = concrete(&name);
                s.peer_name = if domain == AF_INET6 as u8 {
                    let mut ip = [0; 16];
                    ip[10..12].copy_from_slice(&[0xff, 0xff]);
                    ip[12..].copy_from_slice(&ROUTER_IP);
                    sa_in6(ip, 40000 + (h % 20000) as u16)
                } else {
                    sa_in(ROUTER_IP, 40000 + (h % 20000) as u16)
                };
                c
            },
        };
        let peer = net.socks[&child].peer_name.clone();
        let fd = self.install_sock(child, flags);
        if fd < 0 {
            return Ret::Val(fd);
        }
        if addr != 0 && len != 0 && !self.put_sockaddr(addr, len, &peer) {
            self.files.close(fd as i32);
            return e(EFAULT);
        }
        Ret::Val(fd)
    }

    fn sys_connect(&mut self, fd: i32, addr: u64, len: u64) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        // A netlink socket talks to the kernel whatever it connects to
        if self.files.net.socks[&id].domain as u16 == AF_NETLINK {
            return Ret::Val(0);
        }
        let sa = match self.sockaddr(addr, len) {
            Ok(s) => s,
            Err(r) => return r,
        };
        let nonblock = self.nonblock(fd);
        self.sock_refresh(id);
        let s = self.files.net.socks.get_mut(&id).unwrap();
        if s.bc {
            // A blocking connect to the host, run again
            if s.connecting {
                return Ret::Retry {
                    deadline: None,
                    on_timeout: -EAGAIN,
                };
            }
            s.bc = false;
            let err = std::mem::take(&mut s.err);
            return Ret::Val(-err);
        }
        let a = match parse(&sa) {
            Ok(a) => a,
            Err(n) => return e(n),
        };
        let domain = s.domain as u16;
        let ok = matches!(
            (&a, domain),
            (Addr::Unix(_), AF_UNIX) | (Addr::In(..), AF_INET) | (Addr::In6(..), AF_INET6)
        );
        if !ok {
            return e(EAFNOSUPPORT);
        }
        if s.ty == SOCK_DGRAM {
            if matches!(a, Addr::Unix(_)) && !self.files.net.socks[&id].name.is_empty() {
                // fine: a bound datagram socket may connect too
            }
            let s = self.files.net.socks.get_mut(&id).unwrap();
            (s.peer_name, s.connected) = (sa, true);
            return Ret::Val(0);
        }
        if s.listening || s.connected {
            return e(EISCONN);
        }
        if s.connecting {
            return e(EALREADY);
        }
        let target = match &a {
            Addr::Unix(path) => {
                if path.is_empty() {
                    return e(EINVAL);
                }
                match self.unix_key(path) {
                    Ok(k) => self.files.net.names.get(&k).copied(),
                    Err(r) => return r,
                }
            },
            _ if is_local(&a) => {
                let port = match a {
                    Addr::In(_, p) | Addr::In6(_, p) => p,
                    Addr::Unix(_) => 0,
                };
                self.files.net.names.get(&port_key(b't', port)).copied()
            },
            _ => return self.connect_host(id, &a, sa, nonblock),
        };
        match target {
            Some(t) if self.files.net.socks[&t].listening => self.link(id, t, nonblock),
            _ => e(ECONNREFUSED),
        }
    }

    /// Connects stream `id` to the listening socket `target`, in memory.
    fn link(&mut self, id: u32, target: u32, nonblock: bool) -> Ret {
        let l = &self.files.net.socks[&target];
        if l.backlog.len() > l.max_backlog {
            return self.would_block(id, nonblock, 20);
        }
        let (domain, lname) = (l.domain, concrete(&l.name));
        let net = &mut self.files.net;
        net.autobind(id);
        let mut cname = net.socks[&id].name.clone();
        if cname.is_empty() && domain as u16 == AF_UNIX {
            // An unbound unix client's address is its family alone, as accept and getpeername
            // say it (Postgres matches a connection to pg_hba.conf's "local" lines by it)
            cname = vec![AF_UNIX as u8, 0];
        }
        let child = net.add(domain, SOCK_STREAM);
        let c = net.socks.get_mut(&child).unwrap();
        (c.connected, c.peer, c.name, c.peer_name) = (true, Some(id), lname.clone(), cname);
        let s = net.socks.get_mut(&id).unwrap();
        (s.connected, s.peer, s.peer_name) = (true, Some(child), lname);
        net.socks.get_mut(&target).unwrap().backlog.push_back(child);
        Ret::Val(0)
    }

    /// Connects stream `id` to an address that is not the machine's: at the host.
    fn connect_host(&mut self, id: u32, a: &Addr, sa: Vec<u8>, nonblock: bool) -> Ret {
        let (Some((ip, port)), Some(b)) = (v4(a), self.board.clone()) else {
            return e(ENETUNREACH);
        };
        let Some(h) = b.local_open(ip, port) else {
            return e(ENETUNREACH);
        };
        let net = &mut self.files.net;
        if net.socks[&id].name.is_empty() {
            let eph = net.ephemeral(b't');
            let name = if net.socks[&id].domain == AF_INET6 as u8 {
                let mut m = [0; 16];
                m[10..12].copy_from_slice(&[0xff, 0xff]);
                m[12..].copy_from_slice(&GUEST_IP);
                sa_in6(m, eph)
            } else {
                sa_in(GUEST_IP, eph)
            };
            let key = port_key(b't', eph);
            net.names.insert(key.clone(), id);
            let s = net.socks.get_mut(&id).unwrap();
            (s.name, s.key) = (name, Some(key));
        }
        let s = net.socks.get_mut(&id).unwrap();
        (s.host, s.connecting, s.peer_name) = (Some(h), true, sa);
        if nonblock {
            return e(EINPROGRESS);
        }
        s.bc = true;
        Ret::Retry {
            deadline: None,
            on_timeout: -EAGAIN,
        }
    }

    /// getsockname, getpeername
    fn sys_name(&mut self, fd: i32, addr: u64, len: u64, peer: bool) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        self.sock_refresh(id);
        let s = &self.files.net.socks[&id];
        let sa = if s.domain as u16 == AF_NETLINK {
            // Its own port, or the kernel's (0)
            if peer { sa_nl(0) } else { s.name.clone() }
        } else if peer {
            if !s.connected && !s.gone
                || messages(s.ty) && s.peer_name.is_empty() && s.peer.is_none()
            {
                return e(ENOTCONN);
            }
            s.peer_name.clone()
        } else if !s.name.is_empty() {
            s.name.clone()
        } else {
            match s.domain as u16 {
                AF_UNIX => vec![AF_UNIX as u8, 0],
                AF_INET => sa_in([0; 4], 0),
                _ => sa_in6([0; 16], 0),
            }
        };
        Ret::Val(if self.put_sockaddr(addr, len, &sa) { 0 } else { -EFAULT })
    }

    fn sys_sendto(&mut self, fd: i32, a: [u64; 6]) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let Some(data) = self.bytes_at(a[1], self.rw_count(a[2])) else {
            return e(EFAULT);
        };
        let dest = if a[4] != 0 && a[5] != 0 {
            match self.sockaddr(a[4], a[5]) {
                Ok(s) => Some(s),
                Err(r) => return r,
            }
        } else {
            None
        };
        match self.sock_send(fd, id, &data, a[3], dest, &Cm::default()) {
            Ok(n) => Ret::Val(n as i64),
            Err(r) => r,
        }
    }

    fn sys_recvfrom(&mut self, fd: i32, a: [u64; 6]) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let got = self.sock_recv(fd, id, self.rw_count(a[2]), a[3]);
        // recvfrom takes no control data: descriptors that came are closed, as Linux's
        self.drop_taken();
        match got {
            Ok((data, from, _)) => {
                if !self.write(a[1], &data) {
                    return e(EFAULT);
                }
                if a[4] != 0 && a[5] != 0 {
                    self.put_sockaddr(a[4], a[5], &from);
                }
                Ret::Val(data.len() as i64)
            },
            Err(r) => r,
        }
    }

    /// struct msghdr: name, namelen, iov, iovlen, control, controllen, flags
    fn sendmsg_one(&mut self, fd: i32, at: u64, flags: u64) -> Result<usize, Ret> {
        let id = self.sock_id(fd)?;
        let (Some(name), Some(namelen), Some(iov), Some(n), Some(ctl), Some(ctllen)) = (
            self.u64_at(at),
            self.u32_at(at + 8),
            self.u64_at(at + 16),
            self.u64_at(at + 24),
            self.u64_at(at + 32),
            self.u64_at(at + 40),
        ) else {
            return Err(e(EFAULT));
        };
        let cm = self.cmsgs_in(id, ctl, ctllen)?;
        let iov = self.iovecs(iov, n).map_err(Ret::Val)?;
        let mut data = Vec::new();
        for (base, len) in iov {
            data.extend_from_slice(&self.bytes_at(base, len as usize).ok_or(e(EFAULT))?);
        }
        let dest = if name != 0 && namelen != 0 {
            Some(self.sockaddr(name, namelen as u64)?)
        } else {
            None
        };
        self.sock_send(fd, id, &data, flags, dest, &cm)
    }

    /// The control data of a sendmsg: struct cmsghdr (len, level, type) and its data, each padded
    /// to 8. A unix socket passes SCM_RIGHTS, descriptors whose descriptions go with the bytes
    /// (nginx's master hands its workers their sockets this way, Erlang erl_child_setup its port
    /// programs' pipes), and SCM_CREDENTIALS, which a process may give as its own, or anyone's as
    /// root; any other kind of SOL_SOCKET is EINVAL. A network socket passes neither, and the rest
    /// (its own levels, SO_MARK and the like) says nothing here.
    fn cmsgs_in(&self, id: u32, ctl: u64, ctllen: u64) -> Result<Cm, Ret> {
        let unix = self.files.net.socks.get(&id).is_some_and(|s| !s.inet());
        let mut cm = Cm::default();
        let mut off = 0;
        while off + CMSG_HDR as u64 <= ctllen {
            let (Some(l), Some(level), Some(ty)) = (
                self.u64_at(ctl + off),
                self.u32_at(ctl + off + 8),
                self.u32_at(ctl + off + 12),
            ) else {
                return Err(e(EFAULT));
            };
            if l < CMSG_HDR as u64 || off + l > ctllen {
                return Err(e(EINVAL));
            }
            let at = ctl + off + CMSG_HDR as u64;
            let len = l - CMSG_HDR as u64;
            match (level, ty) {
                (SOL_SOCKET, SCM_RIGHTS) if unix => {
                    for i in 0..len / 4 {
                        let f = self.u32_at(at + 4 * i).ok_or(e(EFAULT))? as i32;
                        cm.rights.push(self.files.desc_of(f).ok_or(e(EBADF))?);
                    }
                    if cm.rights.len() > SCM_MAX_FD {
                        return Err(e(EINVAL));
                    }
                },
                (SOL_SOCKET, SCM_CREDENTIALS) if unix => {
                    // struct ucred: pid, uid, gid
                    if len != 12 {
                        return Err(e(EINVAL));
                    }
                    let (Some(pid), Some(uid), Some(gid)) =
                        (self.u32_at(at), self.u32_at(at + 4), self.u32_at(at + 8))
                    else {
                        return Err(e(EFAULT));
                    };
                    if uid == u32::MAX || gid == u32::MAX {
                        return Err(e(EINVAL));
                    }
                    // As Linux's scm_check_creds: its own pid and ids it has, unless root
                    let c = self.p.creds;
                    let root = c.euid() == 0;
                    if !root
                        && (pid as i64 != self.p.pid || !c.uid.contains(&uid) || !c.gid.contains(&gid))
                    {
                        return Err(e(EPERM));
                    }
                    if self.proc_ref(pid as i32 as i64).is_none() {
                        return Err(e(ESRCH));
                    }
                    cm.cred = Some([pid, uid, gid]);
                },
                (SOL_SOCKET, _) if unix => return Err(e(EINVAL)),
                (SOL_SOCKET, SCM_RIGHTS | SCM_CREDENTIALS) => return Err(e(EINVAL)),
                _ => {},
            }
            off += (l + 7) & !7;
        }
        Ok(cm)
    }

    fn recvmsg_one(&mut self, fd: i32, at: u64, flags: u64) -> Result<usize, Ret> {
        let id = self.sock_id(fd)?;
        let (Some(name), Some(iov), Some(n), Some(ctl), Some(ctllen)) = (
            self.u64_at(at),
            self.u64_at(at + 16),
            self.u64_at(at + 24),
            self.u64_at(at + 32),
            self.u64_at(at + 40),
        ) else {
            return Err(e(EFAULT));
        };
        let iov = self.iovecs(iov, n).map_err(Ret::Val)?;
        let total: u64 = iov.iter().map(|&(_, l)| l).sum();
        let (data, from, trunc) = self.sock_recv(fd, id, total as usize, flags)?;
        let mut off = 0;
        for (base, len) in iov {
            let n = (len as usize).min(data.len() - off);
            if !self.write(base, &data[off..off + n]) {
                return Err(e(EFAULT));
            }
            off += n;
        }
        if name != 0 {
            let cap = self.u32_at(at + 8).unwrap_or(0) as usize;
            self.write(name, &from[..from.len().min(cap)]);
            self.put_u32(at + 8, from.len() as u32);
        }
        // The control data, as Linux puts it: who sent the bytes (with SO_PASSCRED), then the
        // descriptors that came with them, new ones in this process, as many as the room left
        // holds (the rest closed). What does not fit is cut, and MSG_CTRUNC says so.
        let taken = std::mem::take(&mut self.files.net.taken);
        let room = if ctl == 0 { 0 } else { ctllen as usize };
        let mut out = Vec::new();
        let mut ctrunc = false;
        if let Some(c) = self.files.net.cred.take() {
            let ucred: Vec<u8> = c.iter().flat_map(|v| v.to_le_bytes()).collect();
            ctrunc |= put_cmsg(&mut out, room, SCM_CREDENTIALS, &ucred);
        }
        let left = room - out.len();
        let fit = if left > CMSG_HDR { (left - CMSG_HDR) / 4 } else { 0 };
        let mut fds = Vec::new();
        for (i, &d) in taken.iter().enumerate() {
            // (One the table has no room for is dropped, and the control data is cut short)
            if i < fit {
                if let Ok(fd) = self.files.install(d, 0, flags & MSG_CMSG_CLOEXEC != 0) {
                    fds.extend_from_slice(&fd.to_le_bytes());
                }
            }
            // The reference it had on its way
            self.release_desc(d);
        }
        ctrunc |= fds.len() / 4 < taken.len();
        if !fds.is_empty() {
            put_cmsg(&mut out, room, SCM_RIGHTS, &fds);
        }
        if !out.is_empty() && !self.write(ctl, &out) {
            return Err(e(EFAULT));
        }
        let used = out.len() as u64;
        self.put_u64(at + 40, used);
        let mut mflags = if trunc { MSG_TRUNC as u32 } else { 0 };
        if ctrunc {
            mflags |= MSG_CTRUNC;
        }
        self.put_u32(at + 48, mflags);
        Ok(data.len())
    }

    /// recvmmsg, sendmmsg: struct mmsghdr is a msghdr and the byte count
    fn sys_mmsg(&mut self, fd: i32, a: [u64; 6], recv: bool) -> Ret {
        let (vec, n, flags) = (a[1], a[2].min(1024), a[3]);
        let mut done = 0;
        for i in 0..n {
            let at = vec + 64 * i;
            let r = if recv {
                // Once one is here, the rest are taken only if they are too
                let f = if done > 0 { flags | MSG_DONTWAIT } else { flags };
                self.recvmsg_one(fd, at, f)
            } else {
                self.sendmsg_one(fd, at, flags)
            };
            match r {
                Ok(len) => {
                    if !self.put_u32(at + 56, len as u32) {
                        return e(EFAULT);
                    }
                    done += 1;
                },
                Err(r) if done == 0 => return r,
                Err(_) => break,
            }
        }
        Ret::Val(done as i64)
    }

    fn sys_shutdown(&mut self, fd: i32, how: u64) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        if how > 2 {
            return e(EINVAL);
        }
        self.sock_refresh(id);
        let board = self.board.clone();
        let net = &mut self.files.net;
        let s = net.socks.get_mut(&id).unwrap();
        if !s.connected && !s.gone || s.listening {
            return e(ENOTCONN);
        }
        let (rd, wr) = (how != 1, how != 0);
        s.rd_shut |= rd;
        let was = s.wr_shut;
        s.wr_shut |= wr;
        let (host, peer) = (s.host, s.peer);
        if wr && !was {
            if let (Some(h), Some(b)) = (host, board) {
                b.local_shutdown(h);
            }
            if let Some(p) = peer.and_then(|p| net.socks.get_mut(&p)) {
                p.fin = true;
            }
        }
        Ret::Val(0)
    }

    fn sys_setsockopt(&mut self, fd: i32, a: [u64; 6]) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let (level, name) = (a[1] as i32, a[2] as i32);
        if !matches!(level, 0 | 1 | 6 | 17 | 41 | 270) {
            return e(ENOPROTOOPT);
        }
        if a[4] > 256 {
            return e(EINVAL);
        }
        let Some(mut v) = self.bytes_at(a[3], a[4] as usize) else {
            return e(EFAULT);
        };
        if level == 1 && matches!(name, 7 | 8) && v.len() >= 4 {
            // The kernel keeps twice what it is asked for
            let n = u32::from_le_bytes(v[..4].try_into().unwrap())
                .saturating_mul(2)
                .max(4608);
            v = n.to_le_bytes().to_vec();
        }
        self.files
            .net
            .socks
            .get_mut(&id)
            .unwrap()
            .opts
            .insert((level, name), v);
        Ret::Val(0)
    }

    fn sys_getsockopt(&mut self, fd: i32, a: [u64; 6]) -> Ret {
        let id = match self.sock_id(fd) {
            Ok(i) => i,
            Err(r) => return r,
        };
        let (level, name) = (a[1] as i32, a[2] as i32);
        if !matches!(level, 0 | 1 | 6 | 17 | 41 | 270) {
            return e(ENOPROTOOPT);
        }
        self.sock_refresh(id);
        let s = self.files.net.socks.get_mut(&id).unwrap();
        let int = |n: i64| (n as i32).to_le_bytes().to_vec();
        let v = match (level, name) {
            (1, 3) => int(s.ty as i64),
            (1, 4) => int(std::mem::take(&mut s.err)),
            (1, 30) => int(s.listening as i64),
            (1, 39) => int(s.domain as i64),
            (1, 38) => int(match (s.domain as u16, s.ty) {
                (AF_UNIX, _) => 0,
                (_, SOCK_STREAM) => 6,
                _ => 17,
            }),
            // struct ucred: the process, root
            (1, 17) => [1i32, 0, 0].iter().flat_map(|n| n.to_le_bytes()).collect(),
            (1, 7 | 8) => s
                .opts
                .get(&(level, name))
                .cloned()
                .unwrap_or_else(|| int(212992)),
            _ => s
                .opts
                .get(&(level, name))
                .cloned()
                .unwrap_or_else(|| int(0)),
        };
        let Some(cap) = self.u32_at(a[4]) else {
            return e(EFAULT);
        };
        let n = v.len().min(cap as usize);
        Ret::Val(
            if self.write(a[3], &v[..n]) && self.put_u32(a[4], n as u32) { 0 } else { -EFAULT },
        )
    }
}
