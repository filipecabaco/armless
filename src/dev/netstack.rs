//! The guest's network: a router at 192.168.86.1 that answers ARP, DHCP,
//! DNS and ping itself, and relays TCP connections to the host as streams.
//!
//! DNS answers every name with an address of its own (10.x.y.z, one per name,
//! kept in snapshots), so the host learns which *name* a connection is for:
//! the browser hands it to fetch() or a WebSocket, a native host resolves it.
//! The link never drops frames (they wait for receive buffers), so TCP
//! towards the guest only has to respect its window; a retransmit timer
//! (`tick`) covers what the guest's stack drops on its own.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::{Event, Outbox};
use crate::ser::{R, W};

pub const ROUTER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x01, 0x02, 0x03];
pub const ROUTER_IP: [u8; 4] = [192, 168, 86, 1];
pub const GUEST_IP: [u8; 4] = [192, 168, 86, 100];
const NETMASK: [u8; 4] = [255, 255, 255, 0];
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

const ETH_IPV4: u16 = 0x0800;
const ETH_ARP: u16 = 0x0806;
const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;

const FIN: u8 = 0x01;
const SYN: u8 = 0x02;
const RST: u8 = 0x04;
const PSH: u8 = 0x08;
const ACK: u8 = 0x10;

const MSS: u16 = 1460;
/// Receive window we advertise (no window scaling).
const OUR_WINDOW: u16 = 65535;
/// Bytes in flight towards the guest at most.
const MAX_IN_FLIGHT: u32 = 256 * 1024;
const RTO_MS: u64 = 1000;
/// Frames queued for the guest at most (it is not reading them).
const MAX_QUEUED: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// Guest sent SYN; waiting for the host to accept.
    SynReceived,
    /// We sent SYN to the guest (host-initiated connection).
    SynSent,
    Established,
}

struct Conn {
    id: u32,
    guest_port: u16,
    remote_ip: [u8; 4],
    remote_port: u16,
    state: State,
    accepted: bool,
    /// Next sequence number expected from the guest.
    rcv_nxt: u32,
    /// Oldest byte of ours not acknowledged.
    snd_una: u32,
    snd_nxt: u32,
    /// Bytes from `snd_una` on (unacknowledged, then unsent).
    buf: VecDeque<u8>,
    guest_window: u32,
    mss: u16,
    /// Host asked to close: send FIN once `buf` is out.
    closing: bool,
    fin_sent: bool,
    guest_fin: bool,
    dup_acks: u32,
    last_progress: u64,
    rto: u64,
    retries: u32,
}

impl Conn {
    fn in_flight(&self) -> u32 {
        self.snd_nxt.wrapping_sub(self.snd_una)
    }
    fn unsent(&self) -> usize {
        let sent = self.in_flight() as usize - (self.fin_sent as usize);
        self.buf.len().saturating_sub(sent)
    }
    fn fin_acked(&self) -> bool {
        self.fin_sent && self.snd_una == self.snd_nxt
    }
}

pub struct Stack {
    pub guest_mac: [u8; 6],
    names: Vec<String>,
    by_name: HashMap<String, u32>,
    conns: BTreeMap<u32, Conn>,
    next_id: u32,
    next_port: u16,
    isn: u32,
    ip_id: u16,
    now: u64,
    /// Ethernet frames for the guest.
    pub frames: VecDeque<Vec<u8>>,
}

fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum = 0u32;
    for p in parts {
        let mut chunks = p.chunks_exact(2);
        for c in &mut chunks {
            sum += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        if let [b] = chunks.remainder() {
            sum += (*b as u32) << 8;
        }
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn dotted(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

impl Stack {
    pub fn new(guest_mac: [u8; 6]) -> Stack {
        Stack {
            guest_mac,
            names: Vec::new(),
            by_name: HashMap::new(),
            conns: BTreeMap::new(),
            next_id: 1,
            next_port: 40000,
            isn: 0x1000_0000,
            ip_id: 1,
            now: 0,
            frames: VecDeque::new(),
        }
    }

    // ------------------------------------------------------------ frames out

    fn ethernet(&mut self, dst: [u8; 6], ty: u16, payload: &[u8]) {
        if self.frames.len() >= MAX_QUEUED {
            return;
        }
        let mut f = Vec::with_capacity(14 + payload.len());
        f.extend_from_slice(&dst);
        f.extend_from_slice(&ROUTER_MAC);
        f.extend_from_slice(&ty.to_be_bytes());
        f.extend_from_slice(payload);
        self.frames.push_back(f);
    }

    fn ipv4(&mut self, src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) {
        let mut h = [0u8; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        h[4..6].copy_from_slice(&self.ip_id.to_be_bytes());
        self.ip_id = self.ip_id.wrapping_add(1);
        h[6] = 0x40; // don't fragment
        h[8] = 64;
        h[9] = proto;
        h[12..16].copy_from_slice(&src);
        h[16..20].copy_from_slice(&dst);
        let c = checksum(&[&h]);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        let mut p = h.to_vec();
        p.extend_from_slice(payload);
        let mac = if dst == [255; 4] { BROADCAST_MAC } else { self.guest_mac };
        self.ethernet(mac, ETH_IPV4, &p);
    }

    fn udp(&mut self, src: [u8; 4], sport: u16, dst: [u8; 4], dport: u16, payload: &[u8]) {
        let mut u = Vec::with_capacity(8 + payload.len());
        u.extend_from_slice(&sport.to_be_bytes());
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(payload);
        let len = (u.len() as u16).to_be_bytes();
        let c = checksum(&[&src, &dst, &[0, PROTO_UDP], &len, &u]);
        u[6..8].copy_from_slice(&(if c == 0 { 0xffff } else { c }).to_be_bytes());
        self.ipv4(src, dst, PROTO_UDP, &u);
    }

    #[allow(clippy::too_many_arguments)]
    fn tcp_raw(
        &mut self,
        src: [u8; 4],
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        options: &[u8],
        payload: &[u8],
    ) {
        let hl = 20 + options.len();
        let mut t = Vec::with_capacity(hl + payload.len());
        t.extend_from_slice(&sport.to_be_bytes());
        t.extend_from_slice(&dport.to_be_bytes());
        t.extend_from_slice(&seq.to_be_bytes());
        t.extend_from_slice(&ack.to_be_bytes());
        t.push(((hl / 4) as u8) << 4);
        t.push(flags);
        t.extend_from_slice(&OUR_WINDOW.to_be_bytes());
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(options);
        t.extend_from_slice(payload);
        let len = (t.len() as u16).to_be_bytes();
        let c = checksum(&[&src, &GUEST_IP, &[0, PROTO_TCP], &len, &t]);
        t[16..18].copy_from_slice(&c.to_be_bytes());
        self.ipv4(src, GUEST_IP, PROTO_TCP, &t);
    }

    fn segment(&mut self, id: u32, flags: u8, seq: u32, payload: &[u8], options: &[u8]) {
        let Some(c) = self.conns.get(&id) else { return };
        let (ip, sport, dport, ack) = (c.remote_ip, c.remote_port, c.guest_port, c.rcv_nxt);
        let ack_flag = if c.state == State::SynSent { 0 } else { ACK };
        self.tcp_raw(
            ip,
            sport,
            dport,
            seq,
            ack,
            flags | ack_flag,
            options,
            payload,
        );
    }

    fn send_ack(&mut self, id: u32) {
        if let Some(c) = self.conns.get(&id) {
            let seq = c.snd_nxt;
            self.segment(id, 0, seq, &[], &[]);
        }
    }

    /// Send what the guest's window allows, then a FIN if the host closed.
    fn pump(&mut self, id: u32) {
        loop {
            let Some(c) = self.conns.get(&id) else { return };
            if c.state != State::Established {
                return;
            }
            let window = c.guest_window.min(MAX_IN_FLIGHT);
            let in_flight = c.in_flight();
            let unsent = c.unsent();
            if unsent > 0 && in_flight < window {
                let n = (unsent as u32).min(window - in_flight).min(c.mss as u32) as usize;
                let off = in_flight as usize;
                let data: Vec<u8> = c.buf.range(off..off + n).copied().collect();
                let seq = c.snd_nxt;
                let push = if n == unsent { PSH } else { 0 };
                self.segment(id, push, seq, &data, &[]);
                let c = self.conns.get_mut(&id).unwrap();
                if c.in_flight() == 0 {
                    c.last_progress = self.now;
                }
                c.snd_nxt = c.snd_nxt.wrapping_add(n as u32);
                continue;
            }
            if unsent == 0 && c.closing && !c.fin_sent {
                let seq = c.snd_nxt;
                self.segment(id, FIN, seq, &[], &[]);
                let c = self.conns.get_mut(&id).unwrap();
                if c.in_flight() == 0 {
                    c.last_progress = self.now;
                }
                c.fin_sent = true;
                c.snd_nxt = c.snd_nxt.wrapping_add(1);
            }
            return;
        }
    }

    fn finish_if_done(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.conns.get(&id) {
            if c.guest_fin && c.fin_acked() {
                self.conns.remove(&id);
                out.push(Event::TcpClosed { id });
            }
        }
    }

    // ------------------------------------------------------------ host API

    pub fn accept(&mut self, id: u32) {
        let isn = self.next_isn();
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        if c.state != State::SynReceived || c.accepted {
            return;
        }
        c.accepted = true;
        c.snd_una = isn;
        c.snd_nxt = isn.wrapping_add(1);
        c.last_progress = self.now;
        self.segment(id, SYN, isn, &[], &[2, 4, (MSS >> 8) as u8, MSS as u8]);
    }

    pub fn refuse(&mut self, id: u32) {
        if let Some(c) = self.conns.get(&id) {
            let ack = c.rcv_nxt;
            let (ip, sport, dport) = (c.remote_ip, c.remote_port, c.guest_port);
            self.tcp_raw(ip, sport, dport, 0, ack, RST | ACK, &[], &[]);
            self.conns.remove(&id);
        }
    }

    pub fn send(&mut self, id: u32, data: &[u8]) {
        if let Some(c) = self.conns.get_mut(&id) {
            if !c.closing {
                c.buf.extend(data);
            }
        }
        self.pump(id);
    }

    pub fn close(&mut self, id: u32, out: &Outbox) {
        if let Some(c) = self.conns.get_mut(&id) {
            c.closing = true;
        }
        self.pump(id);
        self.finish_if_done(id, out);
    }

    pub fn abort(&mut self, id: u32) {
        if let Some(c) = self.conns.get(&id) {
            let seq = c.snd_nxt;
            self.segment(id, RST, seq, &[], &[]);
            self.conns.remove(&id);
        }
    }

    /// Open a connection from the router to the guest's `port`.
    pub fn connect(&mut self, port: u16) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let rport = self.next_port;
        self.next_port = if self.next_port >= 60999 { 40000 } else { self.next_port + 1 };
        let isn = self.next_isn();
        self.conns.insert(
            id,
            self.new_conn(id, port, ROUTER_IP, rport, State::SynSent, isn),
        );
        let c = self.conns.get_mut(&id).unwrap();
        c.snd_nxt = isn.wrapping_add(1);
        self.segment(id, SYN, isn, &[], &[2, 4, (MSS >> 8) as u8, MSS as u8]);
        id
    }

    /// Bytes the host gave a connection that the guest has not acknowledged.
    pub fn buffered(&self, id: u32) -> usize {
        self.conns.get(&id).map_or(0, |c| c.buf.len())
    }

    fn next_isn(&mut self) -> u32 {
        self.isn = self.isn.wrapping_add(0x0101_0101);
        self.isn
    }

    fn new_conn(
        &self,
        id: u32,
        guest_port: u16,
        ip: [u8; 4],
        port: u16,
        state: State,
        isn: u32,
    ) -> Conn {
        Conn {
            id,
            guest_port,
            remote_ip: ip,
            remote_port: port,
            state,
            accepted: false,
            rcv_nxt: 0,
            snd_una: isn,
            snd_nxt: isn,
            buf: VecDeque::new(),
            guest_window: 0,
            mss: 536,
            closing: false,
            fin_sent: false,
            guest_fin: false,
            dup_acks: 0,
            last_progress: self.now,
            rto: RTO_MS,
            retries: 0,
        }
    }

    /// Retransmissions; call every few hundred milliseconds.
    pub fn tick(&mut self, now_ms: u64, out: &Outbox) {
        self.now = now_ms;
        let ids: Vec<u32> = self.conns.keys().copied().collect();
        for id in ids {
            let Some(c) = self.conns.get_mut(&id) else {
                continue;
            };
            let waiting = match c.state {
                State::SynReceived => c.accepted,
                State::SynSent => true,
                State::Established => c.in_flight() > 0 || (c.unsent() > 0 && c.guest_window == 0),
            };
            if !waiting || now_ms.saturating_sub(c.last_progress) < c.rto {
                continue;
            }
            c.last_progress = now_ms;
            c.rto = (c.rto * 2).min(16_000);
            c.retries += 1;
            if c.retries > 12 {
                self.abort(id);
                out.push(Event::TcpClosed { id });
                continue;
            }
            match c.state {
                State::SynReceived => {
                    let isn = c.snd_una;
                    self.segment(id, SYN, isn, &[], &[2, 4, (MSS >> 8) as u8, MSS as u8]);
                },
                State::SynSent => {
                    let isn = c.snd_una;
                    self.segment(id, SYN, isn, &[], &[2, 4, (MSS >> 8) as u8, MSS as u8]);
                },
                State::Established => {
                    if c.in_flight() == 0 {
                        // Zero window: probe with one byte.
                        if let Some(&b) = c.buf.front() {
                            let seq = c.snd_una;
                            self.segment(id, 0, seq, &[b], &[]);
                        }
                    } else {
                        // Go back to the oldest unacknowledged byte.
                        c.snd_nxt = c.snd_una;
                        c.fin_sent = false;
                        self.pump(id);
                    }
                },
            }
        }
    }

    // ------------------------------------------------------------ frames in

    /// An Ethernet frame from the guest.
    pub fn receive(&mut self, f: &[u8], out: &Outbox) {
        if f.len() < 14 {
            return;
        }
        let src_mac: [u8; 6] = f[6..12].try_into().unwrap();
        let ty = be16(f, 12);
        let p = &f[14..];
        match ty {
            ETH_ARP => self.arp(p, src_mac),
            ETH_IPV4 => self.ip(p, src_mac, out),
            _ => {},
        }
    }

    fn arp(&mut self, p: &[u8], src_mac: [u8; 6]) {
        if p.len() < 28 || be16(p, 6) != 1 {
            return;
        }
        let sender_ip: [u8; 4] = p[14..18].try_into().unwrap();
        let target_ip: [u8; 4] = p[24..28].try_into().unwrap();
        if target_ip == sender_ip || target_ip == GUEST_IP {
            return; // gratuitous ARP, or probing for duplicates
        }
        self.guest_mac = src_mac;
        let mut r = Vec::with_capacity(28);
        r.extend_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
        r.extend_from_slice(&ROUTER_MAC);
        r.extend_from_slice(&target_ip);
        r.extend_from_slice(&src_mac);
        r.extend_from_slice(&sender_ip);
        self.ethernet(src_mac, ETH_ARP, &r);
    }

    fn ip(&mut self, p: &[u8], src_mac: [u8; 6], out: &Outbox) {
        if p.len() < 20 || p[0] >> 4 != 4 {
            return;
        }
        let ihl = (p[0] & 15) as usize * 4;
        let total = (be16(p, 2) as usize).min(p.len());
        if ihl < 20 || total < ihl {
            return;
        }
        let src: [u8; 4] = p[12..16].try_into().unwrap();
        let dst: [u8; 4] = p[16..20].try_into().unwrap();
        let proto = p[9];
        let body = &p[ihl..total];
        self.guest_mac = src_mac;
        match proto {
            PROTO_ICMP => self.icmp(src, dst, body),
            PROTO_UDP => self.udp_in(src, dst, body),
            PROTO_TCP if src == GUEST_IP => self.tcp_in(dst, body, out),
            _ => {},
        }
    }

    fn icmp(&mut self, src: [u8; 4], dst: [u8; 4], b: &[u8]) {
        if b.len() < 8 || b[0] != 8 || dst != ROUTER_IP {
            return;
        }
        let mut r = b.to_vec();
        r[0] = 0;
        r[2] = 0;
        r[3] = 0;
        let c = checksum(&[&r]);
        r[2..4].copy_from_slice(&c.to_be_bytes());
        self.ipv4(dst, src, PROTO_ICMP, &r);
    }

    fn udp_in(&mut self, src: [u8; 4], dst: [u8; 4], b: &[u8]) {
        if b.len() < 8 {
            return;
        }
        let sport = be16(b, 0);
        let dport = be16(b, 2);
        let len = (be16(b, 4) as usize).clamp(8, b.len());
        let data = &b[8..len];
        match dport {
            67 => self.dhcp(data),
            53 => {
                if let Some(answer) = self.dns(data) {
                    self.udp(dst, 53, src, sport, &answer);
                }
            },
            _ => {},
        }
    }

    fn dhcp(&mut self, d: &[u8]) {
        if d.len() < 240 || d[0] != 1 || d[236..240] != [99, 130, 83, 99] {
            return;
        }
        let mut kind = 0;
        let mut i = 240;
        while i + 1 < d.len() && d[i] != 255 {
            if d[i] == 0 {
                i += 1;
                continue;
            }
            let len = d[i + 1] as usize;
            if d[i] == 53 && len >= 1 && i + 2 < d.len() {
                kind = d[i + 2];
            }
            i += 2 + len;
        }
        let reply = match kind {
            1 => 2, // DISCOVER -> OFFER
            3 => 5, // REQUEST -> ACK
            _ => return,
        };
        let chaddr: [u8; 6] = d[28..34].try_into().unwrap();
        self.guest_mac = chaddr;
        let mut r = vec![0u8; 240];
        r[0] = 2;
        r[1] = 1;
        r[2] = 6;
        r[4..8].copy_from_slice(&d[4..8]); // xid
        r[10..12].copy_from_slice(&d[10..12]); // flags
        r[16..20].copy_from_slice(&GUEST_IP);
        r[20..24].copy_from_slice(&ROUTER_IP);
        r[28..44].copy_from_slice(&d[28..44]);
        r[236..240].copy_from_slice(&[99, 130, 83, 99]);
        let mut opt = |code: u8, v: &[u8]| {
            r.push(code);
            r.push(v.len() as u8);
            r.extend_from_slice(v);
        };
        opt(53, &[reply]);
        opt(54, &ROUTER_IP);
        opt(51, &86400u32.to_be_bytes());
        opt(1, &NETMASK);
        opt(3, &ROUTER_IP);
        opt(6, &ROUTER_IP);
        opt(28, &[192, 168, 86, 255]);
        opt(26, &1500u16.to_be_bytes());
        r.push(255);
        self.udp(ROUTER_IP, 67, [255; 4], 68, &r);
    }

    /// The address DNS gives `name`.
    fn address_of(&mut self, name: &str) -> [u8; 4] {
        if name == "localhost" {
            return [127, 0, 0, 1];
        }
        let i = match self.by_name.get(name) {
            Some(&i) => i,
            None => {
                let i = self.names.len() as u32 + 2;
                self.names.push(name.to_string());
                self.by_name.insert(name.to_string(), i);
                i
            },
        };
        [10, (i >> 16) as u8, (i >> 8) as u8, i as u8]
    }

    /// The name behind an address DNS gave out, or the address itself.
    fn host_of(&self, ip: [u8; 4]) -> String {
        if ip[0] == 10 {
            let i = (ip[1] as u32) << 16 | (ip[2] as u32) << 8 | ip[3] as u32;
            if let Some(n) = i.checked_sub(2).and_then(|k| self.names.get(k as usize)) {
                return n.clone();
            }
        }
        dotted(ip)
    }

    fn dns(&mut self, q: &[u8]) -> Option<Vec<u8>> {
        if q.len() < 12 || q[2] & 0x80 != 0 || be16(q, 4) != 1 {
            return None;
        }
        // The question
        let mut i = 12;
        let mut labels = Vec::new();
        loop {
            let n = *q.get(i)? as usize;
            i += 1;
            if n == 0 {
                break;
            }
            if n > 63 {
                return None;
            }
            labels.push(String::from_utf8_lossy(q.get(i..i + n)?).to_ascii_lowercase());
            i += n;
        }
        let qtype = be16(q.get(i..i + 2)?, 0);
        let question_end = i + 4;
        if question_end > q.len() {
            return None;
        }
        let name = labels.join(".");
        let mut r = Vec::with_capacity(question_end + 16);
        r.extend_from_slice(&q[0..2]);
        let opcode = q[2] & 0x78;
        let rd = q[2] & 1;
        r.push(0x80 | opcode | rd);
        let answer = qtype == 1 && opcode == 0 && !name.is_empty();
        r.push(if opcode == 0 { 0x80 } else { 0x84 }); // RA; NOTIMP for other opcodes
        r.extend_from_slice(&[0, 1, 0, answer as u8, 0, 0, 0, 0]);
        r.extend_from_slice(&q[12..question_end]);
        if answer {
            let ip = self.address_of(&name);
            r.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0x02, 0x58, 0, 4]);
            r.extend_from_slice(&ip);
        }
        Some(r)
    }

    fn tcp_in(&mut self, dst: [u8; 4], t: &[u8], out: &Outbox) {
        if t.len() < 20 {
            return;
        }
        let sport = be16(t, 0);
        let dport = be16(t, 2);
        let seq = be32(t, 4);
        let ack = be32(t, 8);
        let off = ((t[12] >> 4) as usize * 4).clamp(20, t.len());
        let flags = t[13];
        let window = be16(t, 14) as u32;
        let payload = &t[off..];

        let id = self
            .conns
            .values()
            .find(|c| c.guest_port == sport && c.remote_ip == dst && c.remote_port == dport)
            .map(|c| c.id);

        let Some(id) = id else {
            if flags & RST != 0 {
                return;
            }
            if flags & SYN != 0 && flags & ACK == 0 {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                let mut c = self.new_conn(id, sport, dst, dport, State::SynReceived, 0);
                c.rcv_nxt = seq.wrapping_add(1);
                c.guest_window = window;
                c.mss = parse_mss(&t[20..off]).unwrap_or(536).min(MSS);
                self.conns.insert(id, c);
                let host = self.host_of(dst);
                out.push(Event::TcpOpen {
                    id,
                    host,
                    port: dport,
                });
                return;
            }
            // Not a connection we know (e.g. from before a restore): reset it.
            let (rseq, rack, rflags) = if flags & ACK != 0 {
                (ack, 0, RST)
            } else {
                let len = payload.len() as u32 + (flags & (SYN | FIN) != 0) as u32;
                (0, seq.wrapping_add(len), RST | ACK)
            };
            self.tcp_raw(dst, dport, sport, rseq, rack, rflags, &[], &[]);
            return;
        };

        if flags & RST != 0 {
            let notify = self
                .conns
                .get(&id)
                .is_some_and(|c| c.state != State::SynReceived || c.accepted);
            self.conns.remove(&id);
            if notify {
                out.push(Event::TcpClosed { id });
            }
            return;
        }

        let c = self.conns.get_mut(&id).unwrap();
        match c.state {
            State::SynReceived => {
                if flags & SYN != 0 {
                    // The guest retransmitted its SYN.
                    if c.accepted {
                        let isn = c.snd_una;
                        self.segment(id, SYN, isn, &[], &[2, 4, (MSS >> 8) as u8, MSS as u8]);
                    }
                    return;
                }
                if !(c.accepted && flags & ACK != 0 && ack == c.snd_nxt) {
                    return;
                }
                c.state = State::Established;
                c.snd_una = ack;
                c.retries = 0;
                c.rto = RTO_MS;
            },
            State::SynSent => {
                if flags & SYN != 0 && flags & ACK != 0 && ack == c.snd_nxt {
                    c.state = State::Established;
                    c.rcv_nxt = seq.wrapping_add(1);
                    c.snd_una = ack;
                    c.guest_window = window;
                    c.mss = parse_mss(&t[20..off]).unwrap_or(536).min(MSS);
                    c.retries = 0;
                    c.rto = RTO_MS;
                    self.send_ack(id);
                    out.push(Event::TcpConnected { id });
                    self.pump(id);
                }
                return;
            },
            State::Established => {},
        }

        // ACK processing
        let c = self.conns.get_mut(&id).unwrap();
        c.guest_window = window;
        if flags & ACK != 0 {
            let acked = ack.wrapping_sub(c.snd_una);
            if acked > 0 && acked <= c.in_flight() {
                let fin_now_acked = c.fin_sent && ack == c.snd_nxt;
                let data_acked = acked as usize - fin_now_acked as usize;
                c.buf.drain(..data_acked.min(c.buf.len()));
                c.snd_una = ack;
                c.dup_acks = 0;
                c.retries = 0;
                c.rto = RTO_MS;
                c.last_progress = self.now;
            } else if acked == 0
                && payload.is_empty()
                && c.in_flight() > 0
                && flags & (SYN | FIN) == 0
            {
                c.dup_acks += 1;
                if c.dup_acks == 3 {
                    c.snd_nxt = c.snd_una;
                    c.fin_sent = false;
                }
            }
        }

        // Data and FIN
        let mut need_ack = false;
        if !payload.is_empty() || flags & FIN != 0 {
            need_ack = true;
            let c = self.conns.get_mut(&id).unwrap();
            let skip = c.rcv_nxt.wrapping_sub(seq) as usize;
            if skip <= payload.len() && !c.guest_fin {
                let fresh = &payload[skip..];
                if !fresh.is_empty() {
                    c.rcv_nxt = c.rcv_nxt.wrapping_add(fresh.len() as u32);
                    out.push(Event::TcpData {
                        id,
                        data: fresh.to_vec(),
                    });
                }
                let c = self.conns.get_mut(&id).unwrap();
                if flags & FIN != 0 && seq.wrapping_add(payload.len() as u32) == c.rcv_nxt {
                    c.rcv_nxt = c.rcv_nxt.wrapping_add(1);
                    c.guest_fin = true;
                    out.push(Event::TcpEof { id });
                }
            }
        }

        let before = self.conns.get(&id).map(|c| c.snd_nxt);
        self.pump(id);
        let sent = self.conns.get(&id).map(|c| c.snd_nxt) != before;
        if need_ack && !sent {
            self.send_ack(id);
        }
        self.finish_if_done(id, out);
    }

    pub fn save(&self, w: &mut W) {
        w.0.extend_from_slice(&self.guest_mac);
        w.u32(self.names.len() as u32);
        for n in &self.names {
            w.str(n);
        }
        w.u32(self.next_id);
        w.u32(self.isn);
        w.u16(self.ip_id);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.guest_mac = r.take(6)?.try_into().ok()?;
        self.names.clear();
        self.by_name.clear();
        for i in 0..r.u32()? {
            let n = r.str()?;
            self.by_name.insert(n.clone(), i + 2);
            self.names.push(n);
        }
        self.next_id = r.u32()?;
        self.isn = r.u32()?;
        self.ip_id = r.u16()?;
        // Connections belonged to the host that saved the state.
        self.conns.clear();
        self.frames.clear();
        Some(())
    }
}

fn parse_mss(opts: &[u8]) -> Option<u16> {
    let mut i = 0;
    while i < opts.len() {
        match opts[i] {
            0 => return None,
            1 => i += 1,
            kind => {
                let len = *opts.get(i + 1)? as usize;
                if len < 2 {
                    return None;
                }
                if kind == 2 && len == 4 {
                    return Some(be16(opts, i + 2));
                }
                i += len;
            },
        }
    }
    None
}
