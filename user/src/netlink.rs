//! A netlink route socket (AF_NETLINK, NETLINK_ROUTE), as far as asking what the machine's
//! interfaces, addresses and routes are: the RTM_GETLINK, RTM_GETADDR and RTM_GETROUTE dumps.
//! `ip addr` and `ip route` use it, and so do glibc's and musl's getifaddrs, Node's
//! os.networkInterfaces(), Go's net.Interfaces() and Erlang's inet:getifaddrs().
//!
//! The machine has the loopback, `lo` (127.0.0.1/8, ::1/128), and with a network `eth0`, at
//! 192.168.86.100/24 with the router 192.168.86.1 (net.rs's GUEST_IP and ROUTER_IP). A request is
//! answered at once: its replies are one datagram in the socket's queue, NLMSG_DONE last.
//! Changing anything (RTM_NEWADDR and the like) is EPERM, as for a process without
//! CAP_NET_ADMIN.

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 1;
const NLM_F_MULTI: u16 = 2;
#[cfg(test)]
const NLM_F_ACK: u16 = 4;
const NLM_F_DUMP: u16 = 0x300;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETROUTE: u16 = 26;

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;
const EPERM: i32 = 1;
const EOPNOTSUPP: i32 = 95;

/// An interface: its index, name, hardware type, flags, MTU, hardware address, and addresses
/// (family, address, prefix length, scope)
struct Link {
    index: u32,
    name: &'static [u8],
    kind: u16,
    flags: u32,
    mtu: u32,
    mac: [u8; 6],
    addrs: Vec<(u8, Vec<u8>, u8, u8)>,
}

const IFF_UP: u32 = 0x1;
const IFF_BROADCAST: u32 = 0x2;
const IFF_LOOPBACK: u32 = 0x8;
const IFF_RUNNING: u32 = 0x40;
const IFF_MULTICAST: u32 = 0x1000;
const IFF_LOWER_UP: u32 = 0x10000;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_HOST: u8 = 254;

fn links(network: bool, guest: [u8; 4]) -> Vec<Link> {
    let mut v = vec![Link {
        index: 1,
        name: b"lo",
        kind: 772, // ARPHRD_LOOPBACK
        flags: IFF_UP | IFF_LOOPBACK | IFF_RUNNING | IFF_LOWER_UP,
        mtu: 65536,
        mac: [0; 6],
        addrs: vec![
            (AF_INET, vec![127, 0, 0, 1], 8, RT_SCOPE_HOST),
            (AF_INET6, [0u8; 15].iter().copied().chain([1]).collect(), 128, RT_SCOPE_HOST),
        ],
    }];
    if network {
        v.push(Link {
            index: 2,
            name: b"eth0",
            kind: 1, // ARPHRD_ETHER
            flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST | IFF_LOWER_UP,
            mtu: 1500,
            mac: [0x02, 0x00, guest[0], guest[1], guest[2], guest[3]],
            addrs: vec![(AF_INET, guest.to_vec(), 24, RT_SCOPE_UNIVERSE)],
        });
    }
    v
}

/// A netlink message: header (length, type, flags, sequence, port) and payload, padded to 4
fn message(ty: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let len = 16 + payload.len();
    let mut m = Vec::with_capacity(align(len));
    m.extend_from_slice(&(len as u32).to_le_bytes());
    m.extend_from_slice(&ty.to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&pid.to_le_bytes());
    m.extend_from_slice(payload);
    m.resize(align(len), 0);
    m
}

/// A route attribute (rtattr): length, type, data, padded to 4
fn attr(out: &mut Vec<u8>, ty: u16, data: &[u8]) {
    let len = 4 + data.len();
    out.extend_from_slice(&(len as u16).to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(data);
    out.resize(align(out.len()), 0);
}

fn align(n: usize) -> usize {
    (n + 3) & !3
}

/// What the kernel answers to the requests in `req` (one or more netlink messages) from port
/// `pid`: the bytes of one datagram.
pub(crate) fn answer(req: &[u8], pid: u32, network: bool, guest: [u8; 4], router: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 16 <= req.len() {
        let u32_at = |o: usize| u32::from_le_bytes(req[o..o + 4].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes(req[o..o + 2].try_into().unwrap());
        let len = u32_at(at) as usize;
        if len < 16 || at + len > req.len() {
            break;
        }
        let (ty, flags, seq) = (u16_at(at + 4), u16_at(at + 6), u32_at(at + 8));
        let body = &req[at + 16..at + len];
        let family = body.first().copied().unwrap_or(AF_UNSPEC);
        let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
        let multi = |out: &mut Vec<u8>, ty: u16, payload: &[u8]| {
            out.extend(message(ty, NLM_F_MULTI, seq, pid, payload));
        };
        match ty {
            RTM_GETLINK if dump => {
                for l in links(network, guest) {
                    // struct ifinfomsg: family, pad, type, index, flags, change
                    let mut p = vec![AF_UNSPEC, 0];
                    p.extend_from_slice(&l.kind.to_le_bytes());
                    p.extend_from_slice(&(l.index as i32).to_le_bytes());
                    p.extend_from_slice(&l.flags.to_le_bytes());
                    p.extend_from_slice(&u32::MAX.to_le_bytes());
                    let mut name = l.name.to_vec();
                    name.push(0);
                    attr(&mut p, 3, &name); // IFLA_IFNAME
                    attr(&mut p, 4, &l.mtu.to_le_bytes()); // IFLA_MTU
                    attr(&mut p, 1, &l.mac); // IFLA_ADDRESS
                    let bcast = if l.flags & IFF_BROADCAST != 0 { [0xff; 6] } else { [0; 6] };
                    attr(&mut p, 2, &bcast); // IFLA_BROADCAST
                    attr(&mut p, 16, &[6]); // IFLA_OPERSTATE: IF_OPER_UP
                    attr(&mut p, 13, &1000u32.to_le_bytes()); // IFLA_TXQLEN
                    multi(&mut out, RTM_NEWLINK, &p);
                }
                done(&mut out, seq, pid);
            },
            RTM_GETADDR if dump => {
                for l in links(network, guest) {
                    for (f, addr, prefix, scope) in &l.addrs {
                        if family != AF_UNSPEC && family != *f {
                            continue;
                        }
                        // struct ifaddrmsg: family, prefix length, flags (IFA_F_PERMANENT), scope, index
                        let mut p = vec![*f, *prefix, 0x80, *scope];
                        p.extend_from_slice(&l.index.to_le_bytes());
                        attr(&mut p, 1, addr); // IFA_ADDRESS
                        if *f == AF_INET {
                            attr(&mut p, 2, addr); // IFA_LOCAL
                            if *prefix < 32 && l.flags & IFF_BROADCAST != 0 {
                                let mask = u32::MAX >> prefix;
                                let ip = u32::from_be_bytes(addr[..4].try_into().unwrap()) | mask;
                                attr(&mut p, 4, &ip.to_be_bytes()); // IFA_BROADCAST
                            }
                            let mut label = l.name.to_vec();
                            label.push(0);
                            attr(&mut p, 3, &label); // IFA_LABEL
                        }
                        multi(&mut out, RTM_NEWADDR, &p);
                    }
                }
                done(&mut out, seq, pid);
            },
            RTM_GETROUTE if dump => {
                // The default route, through the router on eth0 (with a network)
                if network && matches!(family, AF_UNSPEC | AF_INET) {
                    // struct rtmsg: family, dst_len, src_len, tos, table (main), protocol (boot),
                    // scope (universe), type (unicast), flags
                    let mut p = vec![AF_INET, 0, 0, 0, 254, 3, RT_SCOPE_UNIVERSE, 1];
                    p.extend_from_slice(&0u32.to_le_bytes());
                    attr(&mut p, 15, &254u32.to_le_bytes()); // RTA_TABLE
                    attr(&mut p, 5, &router); // RTA_GATEWAY
                    attr(&mut p, 4, &2u32.to_le_bytes()); // RTA_OIF: eth0
                    multi(&mut out, RTM_NEWROUTE, &p);
                }
                done(&mut out, seq, pid);
            },
            // Anything else asked of the kernel: changing an address or a route needs
            // CAP_NET_ADMIN; the rest isn't here
            _ if flags & NLM_F_REQUEST != 0 => {
                let errno = if matches!(ty, 16 | 17 | 19 | 20 | 21 | 24 | 25) { EPERM } else { EOPNOTSUPP };
                error(&mut out, -errno, &req[at..at + 16], seq, pid);
            },
            _ => {},
        }
        at += align(len);
    }
    out
}

fn done(out: &mut Vec<u8>, seq: u32, pid: u32) {
    out.extend(message(NLMSG_DONE, NLM_F_MULTI, seq, pid, &0i32.to_le_bytes()));
}

/// NLMSG_ERROR: the error (negative errno, 0 for an acknowledgement) and the request's header
fn error(out: &mut Vec<u8>, errno: i32, header: &[u8], seq: u32, pid: u32) {
    let mut p = errno.to_le_bytes().to_vec();
    p.extend_from_slice(header);
    out.extend(message(NLMSG_ERROR, 0, seq, pid, &p));
}


// ---------------------------------------------------------------- the SIOCGIF* ioctls

const ENODEV: i64 = 19;
const EADDRNOTAVAIL: i64 = 99;

/// What an interface ioctl on a socket answers in `ifr` (struct ifreq: a name of 16 bytes, then
/// what is asked: an address, flags, an index, the MTU or the queue length), from the same table
/// as the netlink dumps: 0, or a negative errno. Older programs (and busybox's `ip`, for the
/// queue length) ask this way.
pub(crate) fn ifreq(req: u32, ifr: &mut [u8; 40], network: bool, guest: [u8; 4]) -> i64 {
    let all = links(network, guest);
    let name = ifr[..16].split(|&c| c == 0).next().unwrap_or_default().to_vec();
    // SIOCGIFNAME: by index
    if req == 0x8910 {
        let index = u32::from_le_bytes(ifr[16..20].try_into().unwrap());
        let Some(l) = all.iter().find(|l| l.index == index) else {
            return -ENODEV;
        };
        ifr[..16].fill(0);
        ifr[..l.name.len()].copy_from_slice(l.name);
        return 0;
    }
    let Some(l) = all.iter().find(|l| l.name == &name[..]) else {
        return -ENODEV;
    };
    let v4 = l.addrs.iter().find(|a| a.0 == AF_INET);
    let sockaddr_in = |ifr: &mut [u8; 40], ip: [u8; 4]| {
        ifr[16..32].fill(0);
        ifr[16] = AF_INET;
        ifr[20..24].copy_from_slice(&ip);
    };
    let put32 = |ifr: &mut [u8; 40], v: u32| ifr[16..20].copy_from_slice(&v.to_le_bytes());
    match req {
        // SIOCGIFFLAGS
        0x8913 => ifr[16..18].copy_from_slice(&(l.flags as u16).to_le_bytes()),
        // SIOCGIFADDR, SIOCGIFBRDADDR, SIOCGIFNETMASK
        0x8915 | 0x8919 | 0x891b => {
            let Some((_, ip, prefix, _)) = v4 else {
                return -EADDRNOTAVAIL;
            };
            let ip = u32::from_be_bytes(ip[..4].try_into().unwrap());
            let mask = if *prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
            let v = match req {
                0x8915 => ip,
                0x8919 => ip | !mask,
                _ => mask,
            };
            sockaddr_in(ifr, v.to_be_bytes());
        },
        // SIOCGIFMTU
        0x8921 => put32(ifr, l.mtu),
        // SIOCGIFHWADDR: sa_family is the hardware type
        0x8927 => {
            ifr[16..32].fill(0);
            ifr[16..18].copy_from_slice(&l.kind.to_le_bytes());
            ifr[18..24].copy_from_slice(&l.mac);
        },
        // SIOCGIFINDEX, SIOCGIFTXQLEN
        0x8933 => put32(ifr, l.index),
        0x8942 => put32(ifr, 1000),
        // Setting anything needs CAP_NET_ADMIN
        0x8914 | 0x8916 | 0x8918 | 0x891a | 0x891c | 0x8922 | 0x8924 | 0x8943 => return -(EPERM as i64),
        _ => return -(EOPNOTSUPP as i64),
    }
    0
}

/// SIOCGIFCONF: a struct ifreq (name, IPv4 address) for each interface with one, as many as
/// `room` bytes hold
pub(crate) fn ifconf(room: usize, network: bool, guest: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::new();
    for l in links(network, guest) {
        let Some((_, ip, ..)) = l.addrs.iter().find(|a| a.0 == AF_INET) else { continue };
        if out.len() + 40 > room {
            break;
        }
        let mut r = [0u8; 40];
        r[..l.name.len()].copy_from_slice(l.name);
        r[16] = AF_INET;
        r[20..24].copy_from_slice(ip);
        out.extend_from_slice(&r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(ty: u16, family: u8) -> Vec<u8> {
        message(ty, NLM_F_REQUEST | NLM_F_DUMP, 7, 0, &[family, 0, 0, 0, 0, 0, 0, 0])
    }

    /// The messages of a reply: (type, payload)
    fn messages(b: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut v = Vec::new();
        let mut at = 0;
        while at + 16 <= b.len() {
            let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
            v.push((u16::from_le_bytes([b[at + 4], b[at + 5]]), b[at + 16..at + len].to_vec()));
            at += align(len);
        }
        v
    }

    #[test]
    fn the_links_addresses_and_routes_are_the_loopback_and_eth0() {
        let g = [192, 168, 86, 100];
        let r = [192, 168, 86, 1];
        let links = messages(&answer(&request(RTM_GETLINK, 0), 5, true, g, r));
        assert_eq!(links.iter().map(|m| m.0).collect::<Vec<_>>(), [RTM_NEWLINK, RTM_NEWLINK, NLMSG_DONE]);
        let v4 = messages(&answer(&request(RTM_GETADDR, AF_INET), 5, true, g, r));
        assert_eq!(v4.len(), 3);
        // eth0's address: family, /24, index 2, and IFA_ADDRESS 192.168.86.100 after the header
        assert_eq!(&v4[1].1[..2], &[AF_INET, 24]);
        assert_eq!(&v4[1].1[12..16], &g);
        // With no network, only the loopback
        let alone = messages(&answer(&request(RTM_GETADDR, 0), 5, false, g, r));
        assert_eq!(alone.len(), 3, "127.0.0.1, ::1 and done");
        let routes = messages(&answer(&request(RTM_GETROUTE, 0), 5, true, g, r));
        assert_eq!(routes.iter().map(|m| m.0).collect::<Vec<_>>(), [RTM_NEWROUTE, NLMSG_DONE]);
        // A change is refused
        let set = messages(&answer(&message(RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK, 1, 0, &[0; 8]), 5, true, g, r));
        assert_eq!(set[0].0, NLMSG_ERROR);
        assert_eq!(i32::from_le_bytes(set[0].1[..4].try_into().unwrap()), -EPERM);
    }
}
