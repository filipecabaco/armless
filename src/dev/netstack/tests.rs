//! The guest's network, driven as a guest's driver drives it: Ethernet frames in, frames and host
//! events out. Every frame the stack sends is checked for valid IPv4 and TCP/UDP/ICMP checksums.

use super::*;

pub(crate) const GUEST_MAC: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];

/// TCP flags, for tests elsewhere that play the guest.
pub(crate) mod flags {
    pub(crate) const SYN: u8 = super::SYN;
    pub(crate) const ACK: u8 = super::ACK;
    pub(crate) const FIN: u8 = super::FIN;
    pub(crate) const RST: u8 = super::RST;
}

/// A TCP segment the stack sent the guest.
#[derive(Debug, Clone)]
pub(crate) struct Seg {
    pub(crate) src: [u8; 4],
    pub(crate) sport: u16,
    pub(crate) dport: u16,
    pub(crate) seq: u32,
    pub(crate) ack: u32,
    pub(crate) flags: u8,
    pub(crate) options: Vec<u8>,
    pub(crate) payload: Vec<u8>,
}

impl Seg {
    pub(crate) fn has(&self, f: u8) -> bool {
        self.flags & f == f
    }
}

/// The stack, seen from the guest: its own, or a board's (whose events a host takes).
pub(crate) struct Guest {
    stack: Stack,
    out: Outbox,
    board: Option<std::sync::Arc<crate::vm::Board>>,
    /// The guest's next sequence number, per local port.
    seq: HashMap<u16, u32>,
}

fn sum(parts: &[&[u8]]) -> u16 {
    checksum(parts)
}

impl Guest {
    fn new() -> Guest {
        Guest {
            stack: Stack::new(GUEST_MAC),
            out: Outbox::default(),
            board: None,
            seq: HashMap::new(),
        }
    }

    /// The network of `board` (which has one, and no driver taking its frames).
    pub(crate) fn on_board(board: std::sync::Arc<crate::vm::Board>) -> Guest {
        Guest {
            board: Some(board),
            ..Guest::new()
        }
    }

    fn with<T>(&mut self, f: impl FnOnce(&mut Stack, &Outbox) -> T) -> T {
        match &self.board {
            Some(b) => {
                let mut d = b.devices.lock();
                let (_, n) = d.net().expect("a network");
                f(&mut n.stack, &b.io.out)
            },
            None => f(&mut self.stack, &self.out),
        }
    }

    fn send_frame(&mut self, ty: u16, payload: &[u8]) {
        let mut f = ROUTER_MAC.to_vec();
        f.extend_from_slice(&GUEST_MAC);
        f.extend_from_slice(&ty.to_be_bytes());
        f.extend_from_slice(payload);
        self.with(|s, out| s.receive(&f, out));
    }

    fn send_ip(&mut self, dst: [u8; 4], proto: u8, payload: &[u8]) {
        let mut h = [0u8; 20];
        h[0] = 0x45;
        h[2..4].copy_from_slice(&((20 + payload.len()) as u16).to_be_bytes());
        h[8] = 64;
        h[9] = proto;
        h[12..16].copy_from_slice(&GUEST_IP);
        h[16..20].copy_from_slice(&dst);
        let c = sum(&[&h]);
        h[10..12].copy_from_slice(&c.to_be_bytes());
        let mut p = h.to_vec();
        p.extend_from_slice(payload);
        self.send_frame(ETH_IPV4, &p);
    }

    fn send_udp(&mut self, dst: [u8; 4], sport: u16, dport: u16, payload: &[u8]) {
        let mut u = sport.to_be_bytes().to_vec();
        u.extend_from_slice(&dport.to_be_bytes());
        u.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
        u.extend_from_slice(&[0, 0]);
        u.extend_from_slice(payload);
        self.send_ip(dst, PROTO_UDP, &u);
    }

    #[allow(clippy::too_many_arguments)]
    fn send_tcp(
        &mut self,
        dst: [u8; 4],
        sport: u16,
        dport: u16,
        seq: u32,
        ack: u32,
        flags: u8,
        window: u16,
        options: &[u8],
        payload: &[u8],
    ) {
        let hl = 20 + options.len();
        let mut t = sport.to_be_bytes().to_vec();
        t.extend_from_slice(&dport.to_be_bytes());
        t.extend_from_slice(&seq.to_be_bytes());
        t.extend_from_slice(&ack.to_be_bytes());
        t.push(((hl / 4) as u8) << 4);
        t.push(flags);
        t.extend_from_slice(&window.to_be_bytes());
        t.extend_from_slice(&[0, 0, 0, 0]);
        t.extend_from_slice(options);
        t.extend_from_slice(payload);
        let len = (t.len() as u16).to_be_bytes();
        let c = sum(&[&GUEST_IP, &dst, &[0, PROTO_TCP], &len, &t]);
        t[16..18].copy_from_slice(&c.to_be_bytes());
        self.send_ip(dst, PROTO_TCP, &t);
    }

    /// Frames for the guest since the last call: (destination MAC, EtherType, payload), each
    /// with valid IPv4 header and transport checksums.
    fn frames(&mut self) -> Vec<([u8; 6], u16, Vec<u8>)> {
        self.with(|s, _| s.frames.drain(..).collect::<Vec<_>>())
            .into_iter()
            .map(|f| {
                let dst: [u8; 6] = f[0..6].try_into().unwrap();
                assert_eq!(f[6..12], ROUTER_MAC, "from the router");
                let ty = be16(&f, 12);
                let p = f[14..].to_vec();
                if ty == ETH_IPV4 {
                    assert_eq!(sum(&[&p[..20]]), 0, "IPv4 header checksum");
                    let total = be16(&p, 2) as usize;
                    assert_eq!(total, p.len(), "IPv4 length");
                    let (src, dst_ip, proto) = (&p[12..16], &p[16..20], p[9]);
                    let body = &p[20..];
                    let len = (body.len() as u16).to_be_bytes();
                    match proto {
                        PROTO_TCP | PROTO_UDP => {
                            assert_eq!(sum(&[src, dst_ip, &[0, proto], &len, body]), 0, "checksum")
                        },
                        PROTO_ICMP => assert_eq!(sum(&[body]), 0, "ICMP checksum"),
                        _ => {},
                    }
                }
                (dst, ty, p)
            })
            .collect()
    }

    /// The TCP segments sent to the guest since the last call.
    pub(crate) fn segs(&mut self) -> Vec<Seg> {
        self.frames()
            .into_iter()
            .filter(|(_, ty, p)| *ty == ETH_IPV4 && p[9] == PROTO_TCP)
            .map(|(dst, _, p)| {
                assert_eq!(dst, GUEST_MAC);
                assert_eq!(p[16..20], GUEST_IP);
                let t = &p[20..];
                let off = (t[12] >> 4) as usize * 4;
                Seg {
                    src: p[12..16].try_into().unwrap(),
                    sport: be16(t, 0),
                    dport: be16(t, 2),
                    seq: be32(t, 4),
                    ack: be32(t, 8),
                    flags: t[13],
                    options: t[20..off].to_vec(),
                    payload: t[off..].to_vec(),
                }
            })
            .collect()
    }

    fn events(&mut self) -> Vec<Event> {
        self.out.drain()
    }

    /// DNS: the address the router gives `name`.
    pub(crate) fn resolve(&mut self, name: &str, qtype: u16) -> Option<[u8; 4]> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&[0, 1]);
        self.send_udp(ROUTER_IP, 5353, 53, &q);
        let frames = self.frames();
        assert_eq!(frames.len(), 1, "one answer");
        let (_, _, p) = &frames[0];
        let u = &p[20..];
        assert_eq!((be16(u, 0), be16(u, 2)), (53, 5353));
        let r = &u[8..];
        assert_eq!(r[0..2], [0x12, 0x34], "the query's id");
        assert!(r[2] & 0x80 != 0, "a response");
        assert_eq!(r[12..12 + q.len() - 12], q[12..], "the question, echoed");
        match be16(r, 6) {
            0 => None,
            1 => Some(r[r.len() - 4..].try_into().unwrap()),
            n => panic!("{n} answers"),
        }
    }

    /// A connection from the guest's `sport` to `dst:dport`, its SYN announcing `mss`: the
    /// TcpOpen it raises.
    fn open(&mut self, dst: [u8; 4], sport: u16, dport: u16, mss: u16) -> (u32, String) {
        let isn = 0x5000_0000 + sport as u32 * 0x100;
        let opts = [2, 4, (mss >> 8) as u8, mss as u8];
        self.send_tcp(dst, sport, dport, isn, 0, SYN, 64000, &opts, &[]);
        self.seq.insert(sport, isn.wrapping_add(1));
        match &self.events()[..] {
            [Event::TcpOpen { id, host, port }] => {
                assert_eq!(*port, dport);
                (*id, host.clone())
            },
            e => panic!("{e:?}"),
        }
    }

    /// Accept `id` on the host's side, and complete the handshake: the router's first sequence
    /// number after its SYN.
    fn establish(&mut self, id: u32, dst: [u8; 4], sport: u16, dport: u16) -> u32 {
        self.stack.accept(id);
        let segs = self.segs();
        assert_eq!(segs.len(), 1, "{segs:?}");
        let sa = &segs[0];
        assert!(sa.has(SYN | ACK), "{sa:?}");
        assert_eq!((sa.src, sa.sport, sa.dport), (dst, dport, sport));
        assert_eq!(sa.ack, self.seq[&sport], "acknowledges the guest's SYN");
        assert_eq!(parse_mss(&sa.options), Some(MSS), "announces its MSS");
        let theirs = sa.seq.wrapping_add(1);
        self.send_tcp(
            dst,
            sport,
            dport,
            self.seq[&sport],
            theirs,
            ACK,
            64000,
            &[],
            &[],
        );
        assert!(self.segs().is_empty(), "nothing to say yet");
        theirs
    }

    /// Data from the guest on `sport` (its next sequence number advances).
    pub(crate) fn data(&mut self, dst: [u8; 4], sport: u16, dport: u16, ack: u32, payload: &[u8]) {
        let seq = self.seq[&sport];
        self.send_tcp(dst, sport, dport, seq, ack, ACK | PSH, 64000, &[], payload);
        self.seq
            .insert(sport, seq.wrapping_add(payload.len() as u32));
    }

    /// A connection from the guest's `sport` (with no MSS option).
    pub(crate) fn syn(&mut self, dst: [u8; 4], sport: u16, dport: u16) {
        let isn = 0x5000_0000 + sport as u32 * 0x100;
        self.send_tcp(dst, sport, dport, isn, 0, SYN, 64000, &[], &[]);
        self.seq.insert(sport, isn.wrapping_add(1));
    }

    pub(crate) fn ack(&mut self, dst: [u8; 4], sport: u16, dport: u16, ack: u32) {
        self.send_tcp(
            dst,
            sport,
            dport,
            self.seq[&sport],
            ack,
            ACK,
            64000,
            &[],
            &[],
        );
    }

    pub(crate) fn fin(&mut self, dst: [u8; 4], sport: u16, dport: u16, ack: u32) {
        let seq = self.seq[&sport];
        self.send_tcp(dst, sport, dport, seq, ack, FIN | ACK, 64000, &[], &[]);
        self.seq.insert(sport, seq.wrapping_add(1));
    }

    pub(crate) fn rst(&mut self, dst: [u8; 4], sport: u16, dport: u16, ack: u32) {
        let seq = self.seq[&sport];
        self.send_tcp(dst, sport, dport, seq, ack, RST | ACK, 0, &[], &[]);
    }

    /// The guest accepts a connection the host opened into it (`syn`, to its `syn.dport`).
    pub(crate) fn accept(&mut self, syn: &Seg, isn: u32) {
        let ack = syn.seq.wrapping_add(1);
        self.send_tcp(
            syn.src,
            syn.dport,
            syn.sport,
            isn,
            ack,
            SYN | ACK,
            64000,
            &[],
            &[],
        );
        self.seq.insert(syn.dport, isn.wrapping_add(1));
        self.ack(syn.src, syn.dport, syn.sport, ack);
    }

    /// Data from the guest on a connection the host opened into it.
    pub(crate) fn reply(&mut self, syn: &Seg, ack: u32, payload: &[u8]) {
        self.data(syn.src, syn.dport, syn.sport, ack, payload);
    }
}

#[test]
fn arp_for_the_router_is_answered_and_probes_are_not() {
    let mut g = Guest::new();
    let who_has = |target: [u8; 4], sender: [u8; 4]| {
        let mut a = vec![0, 1, 8, 0, 6, 4, 0, 1];
        a.extend_from_slice(&GUEST_MAC);
        a.extend_from_slice(&sender);
        a.extend_from_slice(&[0; 6]);
        a.extend_from_slice(&target);
        a
    };
    g.send_frame(ETH_ARP, &who_has(ROUTER_IP, GUEST_IP));
    let f = g.frames();
    assert_eq!(f.len(), 1);
    let (dst, ty, r) = &f[0];
    assert_eq!((*dst, *ty), (GUEST_MAC, ETH_ARP));
    assert_eq!(be16(r, 6), 2, "a reply");
    assert_eq!(r[8..14], ROUTER_MAC);
    assert_eq!(r[14..18], ROUTER_IP);
    assert_eq!(r[18..24], GUEST_MAC);
    assert_eq!(r[24..28], GUEST_IP);
    // Gratuitous ARP and duplicate-address probes get no answer
    g.send_frame(ETH_ARP, &who_has(GUEST_IP, GUEST_IP));
    g.send_frame(ETH_ARP, &who_has(GUEST_IP, [0; 4]));
    // Nor a frame too short to be ARP
    g.send_frame(ETH_ARP, &[0, 1, 8, 0]);
    assert!(g.frames().is_empty());
}

#[test]
fn dhcp_offers_and_acknowledges_the_guest_its_address() {
    let mut g = Guest::new();
    let request = |kind: u8| {
        let mut d = vec![0u8; 240];
        d[0] = 1;
        d[1] = 1;
        d[2] = 6;
        d[4..8].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        d[10] = 0x80; // broadcast
        d[28..34].copy_from_slice(&GUEST_MAC);
        d[236..240].copy_from_slice(&[99, 130, 83, 99]);
        d.extend_from_slice(&[0, 53, 1, kind, 255]); // a pad, then the message type
        d
    };
    for (kind, reply) in [(1, 2), (3, 5)] {
        g.send_udp([255; 4], 68, 67, &request(kind));
        let f = g.frames();
        assert_eq!(f.len(), 1, "DHCP {kind}");
        let (dst, _, p) = &f[0];
        assert_eq!(*dst, [0xff; 6], "broadcast");
        assert_eq!(p[16..20], [255; 4]);
        let u = &p[20..];
        assert_eq!((be16(u, 0), be16(u, 2)), (67, 68));
        let d = &u[8..];
        assert_eq!((d[0], d[4..8].to_vec()), (2, vec![0xde, 0xad, 0xbe, 0xef]));
        assert_eq!(d[16..20], GUEST_IP, "yiaddr");
        assert_eq!(d[28..34], GUEST_MAC, "chaddr");
        // Options: the reply type, server, lease, mask, router and DNS all present
        let mut opts = HashMap::new();
        let mut i = 240;
        while d[i] != 255 {
            opts.insert(d[i], d[i + 2..i + 2 + d[i + 1] as usize].to_vec());
            i += 2 + d[i + 1] as usize;
        }
        assert_eq!(opts[&53], [reply]);
        assert_eq!(opts[&1], NETMASK);
        assert_eq!(opts[&3], ROUTER_IP);
        assert_eq!(opts[&6], ROUTER_IP);
        assert_eq!(opts[&54], ROUTER_IP);
    }
    // Anything else (a RELEASE, a truncated or foreign packet) is ignored
    g.send_udp([255; 4], 68, 67, &request(7));
    g.send_udp([255; 4], 68, 67, &[1; 100]);
    assert!(g.frames().is_empty());
}

#[test]
fn dns_names_get_stable_addresses_that_survive_a_restore() {
    let mut g = Guest::new();
    let a = g.resolve("db.example.com", 1).unwrap();
    let b = g.resolve("api.example.com", 1).unwrap();
    assert_eq!(a[0], 10);
    assert_ne!(a, b);
    assert_eq!(
        g.resolve("DB.Example.COM", 1),
        Some(a),
        "names are case-insensitive"
    );
    assert_eq!(g.resolve("localhost", 1), Some([127, 0, 0, 1]));
    assert_eq!(
        g.resolve("db.example.com", 28),
        None,
        "AAAA: none, so the guest uses IPv4"
    );
    // A restored machine keeps the names its guest already resolved
    let mut w = W::new();
    g.stack.save(&mut w);
    let mut back = Guest::new();
    back.stack.load(&mut R(&w.0)).unwrap();
    assert_eq!(back.resolve("api.example.com", 1), Some(b));
    assert_eq!(back.resolve("db.example.com", 1), Some(a));
    // ... and connections tell the host the name, not the address
    let (_, host) = back.open(b, 40000, 5432, 1460);
    assert_eq!(host, "api.example.com");
    // An address no name was given is passed on as one
    let (_, host) = back.open([10, 9, 9, 9], 40001, 5432, 1460);
    assert_eq!(host, "10.9.9.9");
}

#[test]
fn the_router_answers_ping() {
    let mut g = Guest::new();
    let mut echo = vec![8, 0, 0, 0, 0x12, 0x34, 0, 7];
    echo.extend_from_slice(b"ping payload");
    let c = sum(&[&echo]);
    echo[2..4].copy_from_slice(&c.to_be_bytes());
    g.send_ip(ROUTER_IP, PROTO_ICMP, &echo);
    let f = g.frames();
    assert_eq!(f.len(), 1);
    let (_, _, p) = &f[0];
    assert_eq!(
        (p[12..16].to_vec(), p[16..20].to_vec()),
        (ROUTER_IP.to_vec(), GUEST_IP.to_vec())
    );
    let r = &p[20..];
    assert_eq!(r[0], 0, "an echo reply");
    assert_eq!(r[4..], echo[4..], "same id, sequence and payload");
    // Echoes to other addresses aren't the router's to answer
    g.send_ip([10, 0, 0, 9], PROTO_ICMP, &echo);
    assert!(g.frames().is_empty());
}

#[test]
fn a_tcp_connection_from_the_guest_carries_data_both_ways_and_closes() {
    let mut g = Guest::new();
    let dst = g.resolve("svc.test", 1).unwrap();
    let (sport, dport) = (40100, 5432);
    // The guest's MSS (1000) is smaller than ours: our segments respect it
    let (id, host) = g.open(dst, sport, dport, 1000);
    assert_eq!(host, "svc.test");
    let mut theirs = g.establish(id, dst, sport, dport);

    // Guest to host
    g.data(dst, sport, dport, theirs, b"hello");
    assert_eq!(
        g.events(),
        [Event::TcpData {
            id,
            data: b"hello".to_vec()
        }]
    );
    let acks = g.segs();
    assert_eq!(acks.len(), 1);
    assert!(acks[0].has(ACK) && acks[0].payload.is_empty());
    assert_eq!(acks[0].ack, g.seq[&sport], "acknowledges the 5 bytes");

    // Host to guest: 3000 bytes, in segments of at most the guest's MSS, in order
    let sent: Vec<u8> = (0..3000).map(|i| (i % 251) as u8).collect();
    g.stack.send(id, &sent);
    let segs = g.segs();
    assert!(segs.iter().all(|s| s.payload.len() <= 1000), "{segs:?}");
    assert!(segs.last().unwrap().has(PSH), "the last pushes");
    let mut got = Vec::new();
    for s in &segs {
        assert_eq!(s.seq, theirs.wrapping_add(got.len() as u32), "in order");
        got.extend_from_slice(&s.payload);
    }
    assert_eq!(got, sent);
    assert_eq!(g.stack.buffered(id), 3000, "kept until acknowledged");
    theirs = theirs.wrapping_add(3000);
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport],
        theirs,
        ACK,
        64000,
        &[],
        &[],
    );
    assert_eq!(g.stack.buffered(id), 0);

    // The guest closes its side: the host hears EOF, the guest's FIN is acknowledged
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport],
        theirs,
        FIN | ACK,
        64000,
        &[],
        &[],
    );
    assert_eq!(g.events(), [Event::TcpEof { id }]);
    let s = g.segs();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].ack, g.seq[&sport] + 1, "the FIN is acknowledged");
    // Then the host: our FIN, and once the guest acknowledges it, closed
    g.stack.close(id, &g.out);
    let s = g.segs();
    assert_eq!(s.len(), 1);
    assert!(s[0].has(FIN), "{s:?}");
    assert!(
        g.events().is_empty(),
        "not closed until the FIN is acknowledged"
    );
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport] + 1,
        theirs + 1,
        ACK,
        64000,
        &[],
        &[],
    );
    assert_eq!(g.events(), [Event::TcpClosed { id }]);
}

#[test]
fn refused_and_reset_connections_end_with_tcp_closed() {
    let mut g = Guest::new();
    let dst = g.resolve("closed.test", 1).unwrap();
    let (id, _) = g.open(dst, 40200, 6379, 1460);
    g.stack.refuse(id, &g.out);
    assert_eq!(g.events(), [Event::TcpClosed { id }]);
    let s = g.segs();
    assert_eq!(s.len(), 1);
    assert!(s[0].has(RST | ACK));
    assert_eq!(s[0].ack, g.seq[&40200], "acknowledges the SYN it refuses");

    // The guest resets an open connection
    let (id, _) = g.open(dst, 40201, 6379, 1460);
    let theirs = g.establish(id, dst, 40201, 6379);
    g.send_tcp(dst, 40201, 6379, g.seq[&40201], theirs, RST, 0, &[], &[]);
    assert_eq!(g.events(), [Event::TcpClosed { id }]);

    // The host aborts one: a RST for the guest, no event (the host knows)
    let (id, _) = g.open(dst, 40202, 6379, 1460);
    g.establish(id, dst, 40202, 6379);
    g.stack.abort(id);
    let s = g.segs();
    assert!(s.len() == 1 && s[0].has(RST), "{s:?}");
    assert!(g.events().is_empty());
}

#[test]
fn segments_for_no_connection_are_reset() {
    let mut g = Guest::new();
    let dst = [10, 0, 0, 2];
    // With ACK: RST from the acknowledged sequence number
    g.send_tcp(dst, 40300, 80, 1000, 0xabcd, ACK, 64000, &[], b"stale");
    let s = g.segs();
    assert_eq!(s.len(), 1);
    assert!(s[0].has(RST) && !s[0].has(ACK));
    assert_eq!(s[0].seq, 0xabcd);
    // Without: RST|ACK acknowledging what it carried (data, and a FIN counts one)
    g.send_tcp(dst, 40301, 80, 1000, 0, FIN, 64000, &[], b"abc");
    let s = g.segs();
    assert!(s[0].has(RST | ACK));
    assert_eq!(s[0].ack, 1004);
    // A RST for no connection gets nothing back (no reset storms)
    g.send_tcp(dst, 40302, 80, 1000, 0, RST, 0, &[], &[]);
    assert!(g.segs().is_empty());
    assert!(g.events().is_empty());
}

#[test]
fn data_the_guest_sends_again_is_delivered_once() {
    let mut g = Guest::new();
    let dst = g.resolve("dup.test", 1).unwrap();
    let (sport, dport) = (40400, 9000);
    let (id, _) = g.open(dst, sport, dport, 1460);
    let theirs = g.establish(id, dst, sport, dport);
    let start = g.seq[&sport];
    g.data(dst, sport, dport, theirs, b"abc");
    // The same segment again (our ACK was lost), then one overlapping it
    g.send_tcp(dst, sport, dport, start, theirs, ACK, 64000, &[], b"abc");
    g.send_tcp(dst, sport, dport, start, theirs, ACK, 64000, &[], b"abcdef");
    // ... and one entirely in the past
    g.send_tcp(
        dst,
        sport,
        dport,
        start.wrapping_sub(10),
        theirs,
        ACK,
        64000,
        &[],
        b"zz",
    );
    let data: Vec<u8> = g
        .events()
        .into_iter()
        .flat_map(|e| match e {
            Event::TcpData { data, .. } => data,
            e => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(data, b"abcdef");
    // Every one of them is acknowledged up to what arrived
    let acks = g.segs();
    assert!(acks.iter().all(|s| s.has(ACK)));
    assert_eq!(acks.last().unwrap().ack, start + 6);
}

#[test]
fn the_guests_window_limits_what_is_in_flight_and_a_closed_window_is_probed() {
    let mut g = Guest::new();
    let dst = g.resolve("window.test", 1).unwrap();
    let (sport, dport) = (40500, 9001);
    let (id, _) = g.open(dst, sport, dport, 1460);
    let mut theirs = g.establish(id, dst, sport, dport);
    // A window of 2000 bytes
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport],
        theirs,
        ACK,
        2000,
        &[],
        &[],
    );
    g.stack.send(id, &[7u8; 10_000]);
    let in_flight: usize = g.segs().iter().map(|s| s.payload.len()).sum();
    assert_eq!(in_flight, 2000);
    // Acknowledged, with room for 2000 more
    theirs += 2000;
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport],
        theirs,
        ACK,
        2000,
        &[],
        &[],
    );
    let segs = g.segs();
    assert_eq!(segs.iter().map(|s| s.payload.len()).sum::<usize>(), 2000);
    assert_eq!(segs[0].seq, theirs);
    // All acknowledged, but the window closes: nothing is sent...
    theirs += 2000;
    g.send_tcp(dst, sport, dport, g.seq[&sport], theirs, ACK, 0, &[], &[]);
    assert!(g.segs().is_empty());
    assert_eq!(g.stack.buffered(id), 6000);
    // ... until the retransmit timer probes it with one byte
    g.stack.tick(500, &g.out);
    assert!(g.segs().is_empty(), "not before the timeout");
    g.stack.tick(1000, &g.out);
    let probe = g.segs();
    assert_eq!(probe.len(), 1);
    assert_eq!((probe[0].seq, probe[0].payload.len()), (theirs, 1));
    // The window opens: sending resumes
    g.send_tcp(
        dst,
        sport,
        dport,
        g.seq[&sport],
        theirs,
        ACK,
        64000,
        &[],
        &[],
    );
    assert_eq!(
        g.segs().iter().map(|s| s.payload.len()).sum::<usize>(),
        6000
    );
}

#[test]
fn unacknowledged_data_is_sent_again_with_a_backoff_and_the_connection_given_up() {
    let mut g = Guest::new();
    let dst = g.resolve("lossy.test", 1).unwrap();
    let (sport, dport) = (40600, 9002);
    let (id, _) = g.open(dst, sport, dport, 1460);
    let theirs = g.establish(id, dst, sport, dport);
    g.stack.send(id, b"lost");
    assert_eq!(g.segs().len(), 1);
    // Sent again after 1 s, then 2, 4, 8, 16 s (capped): each from the oldest byte
    let mut now = 0;
    let mut gaps = Vec::new();
    for _ in 0..12 {
        let before = now;
        loop {
            now += 250;
            g.stack.tick(now, &g.out);
            let s = g.segs();
            if !s.is_empty() {
                assert_eq!((s[0].seq, s[0].payload.clone()), (theirs, b"lost".to_vec()));
                break;
            }
        }
        gaps.push(now - before);
    }
    assert_eq!(gaps[..5], [1000, 2000, 4000, 8000, 16000]);
    assert!(gaps[5..].iter().all(|&d| d == 16000), "{gaps:?}");
    assert!(g.events().is_empty());
    // The 13th time it gives up: a RST for the guest, TcpClosed for the host
    g.stack.tick(now + 16_000, &g.out);
    assert!(g.segs().iter().any(|s| s.has(RST)));
    assert_eq!(g.events(), [Event::TcpClosed { id }]);
}

#[test]
fn three_duplicate_acks_send_the_lost_segment_again_at_once() {
    let mut g = Guest::new();
    let dst = g.resolve("fast.test", 1).unwrap();
    let (sport, dport) = (40700, 9003);
    let (id, _) = g.open(dst, sport, dport, 1000);
    let theirs = g.establish(id, dst, sport, dport);
    g.stack.send(id, &[1u8; 3000]);
    assert_eq!(g.segs().len(), 3);
    // The first segment was lost: the guest acknowledges its start three times
    for _ in 0..3 {
        g.send_tcp(
            dst,
            sport,
            dport,
            g.seq[&sport],
            theirs,
            ACK,
            64000,
            &[],
            &[],
        );
    }
    let again = g.segs();
    assert!(
        !again.is_empty(),
        "sent again without waiting for the timer"
    );
    assert_eq!(again[0].seq, theirs);
}

#[test]
fn the_host_opens_connections_into_the_guest() {
    let mut g = Guest::new();
    let id = g.stack.connect(8080);
    let s = g.segs();
    assert_eq!(s.len(), 1);
    let syn = &s[0];
    assert!(syn.has(SYN) && !syn.has(ACK));
    assert_eq!((syn.src, syn.dport), (ROUTER_IP, 8080));
    assert_eq!(parse_mss(&syn.options), Some(MSS));
    // The guest answers SYN|ACK
    let isn = 0x7000_0000;
    let mss = [2, 4, 0x03, 0xe8];
    g.send_tcp(
        ROUTER_IP,
        8080,
        syn.sport,
        isn,
        syn.seq + 1,
        SYN | ACK,
        64000,
        &mss,
        &[],
    );
    assert_eq!(g.events(), [Event::TcpConnected { id }]);
    let ack = g.segs();
    assert_eq!(ack.len(), 1);
    assert_eq!(ack[0].ack, isn + 1);
    // Data both ways
    g.stack.send(id, b"GET /");
    let s = g.segs();
    assert_eq!(s[0].payload, b"GET /");
    g.send_tcp(
        ROUTER_IP,
        8080,
        syn.sport,
        isn + 1,
        syn.seq + 1 + 5,
        ACK,
        64000,
        &[],
        b"200",
    );
    assert_eq!(
        g.events(),
        [Event::TcpData {
            id,
            data: b"200".to_vec()
        }]
    );
    assert!(
        g.segs().iter().all(|s| s.ack == isn + 4),
        "the 3 bytes acknowledged"
    );
    // A guest that never answers: the SYN is sent again (the same one), and in the end given up
    let id2 = g.stack.connect(8081);
    let first = g.segs();
    assert!(first.len() == 1 && first[0].has(SYN));
    g.stack.tick(1000, &g.out);
    let again = g.segs();
    assert!(
        again.len() == 1 && again[0].has(SYN) && again[0].seq == first[0].seq,
        "{again:?}"
    );
    let mut now = 1000;
    while !g.events().contains(&Event::TcpClosed { id: id2 }) {
        now += 1000;
        assert!(now < 300_000, "never given up");
        g.stack.tick(now, &g.out);
    }
}

#[test]
fn segments_with_options_and_frames_of_other_kinds_are_handled() {
    let mut g = Guest::new();
    // Not IPv4 (IPv6, an unknown EtherType), or too short to be anything: ignored
    g.send_frame(0x86dd, &[0x60; 40]);
    g.send_frame(0x1234, &[0; 20]);
    g.stack.receive(&[0; 10], &g.out);
    // IPv4 with a bad version or header length: ignored
    let mut bad = vec![0x65; 40];
    bad[9] = PROTO_TCP;
    g.send_frame(ETH_IPV4, &bad);
    assert!(g.frames().is_empty());
    assert!(g.events().is_empty());
    // A SYN with timestamps and window scaling before its MSS still has its MSS read
    let dst = g.resolve("opts.test", 1).unwrap();
    let opts = [
        1, 1, 8, 10, 0, 0, 0, 1, 0, 0, 0, 0, 1, 3, 3, 7, 2, 4, 0x02, 0x00,
    ];
    g.send_tcp(dst, 40800, 9004, 1, 0, SYN, 64000, &opts, &[]);
    let id = match &g.events()[..] {
        [Event::TcpOpen { id, .. }] => *id,
        e => panic!("{e:?}"),
    };
    g.seq.insert(40800, 2);
    g.establish(id, dst, 40800, 9004);
    g.stack.send(id, &[0u8; 2000]);
    assert!(
        g.segs().iter().all(|s| s.payload.len() <= 512),
        "the guest's MSS of 512"
    );
}
