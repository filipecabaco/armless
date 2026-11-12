//! Sockets, as far as a program needs them inside itself: `socketpair(AF_UNIX)` gives two
//! connected ends, each reading one pipe and writing the other (`Kind::Sock`), so they block,
//! poll, wait in epoll and are saved with the process as pipes are. Python's asyncio and tokio
//! (deno) make one at startup to wake their event loop, and send and receive on it with
//! sendto/recvfrom and sendmsg/recvmsg; shutdown, getsockopt, setsockopt, getsockname and
//! getpeername answer as Linux does for such a pair.
//!
//! Every type (stream, datagram, seqpacket) is a byte stream here: datagram boundaries aren't
//! kept. Not yet: `socket` and the rest of a network (bind, listen, accept, connect), passing
//! descriptors (SCM_RIGHTS), MSG_PEEK.

use crate::files::{Kind, O_CLOEXEC, O_NONBLOCK, SHUT_RD, SHUT_WR};
use crate::{Process, Ret};

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_SEQPACKET: u64 = 5;
const SOCK_TYPE_MASK: u64 = 0xf;
const O_RDWR: u32 = 2;
const MSG_DONTWAIT: u64 = 0x40;
const SOL_SOCKET: u64 = 1;
const SO_TYPE: u64 = 3;
const SO_ERROR: u64 = 4;
const SO_SNDBUF: u64 = 7;
const SO_RCVBUF: u64 = 8;
const SO_DOMAIN: u64 = 39;

const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EMFILE: i64 = 24;
const ENOTSOCK: i64 = 88;
const EPROTONOSUPPORT: i64 = 93;
const EOPNOTSUPP: i64 = 95;
const EAFNOSUPPORT: i64 = 97;
const EISCONN: i64 = 106;

/// The socket system calls user mode answers (the rest are ENOSYS).
pub(crate) fn handles(nr: u64) -> bool {
    matches!(nr, 199 | 204..=212)
}

impl Process {
    /// socketpair, getsockname, getpeername, sendto, recvfrom, setsockopt, getsockopt, shutdown,
    /// sendmsg, recvmsg (see `handles`).
    pub(crate) fn sock_call(&mut self, nr: u64, a: &[u64; 6]) -> Ret {
        let v = Ret::Val;
        if nr == 199 {
            return self.socketpair(a[0], a[1], a[2], a[3]);
        }
        let fd = a[0] as i32;
        let Some(d) = self.files.get(fd) else {
            return v(-EBADF);
        };
        let Kind::Sock { shut, .. } = d.kind else {
            return v(-ENOTSOCK);
        };
        match nr {
            // getsockname, getpeername: an unnamed AF_UNIX socket (just its family)
            204 | 205 => v(if self.sockaddr_unnamed(a[1], a[2]) { 0 } else { -EFAULT }),
            // sendto(fd, buf, len, flags, addr, addrlen): a pair is connected, so no address
            206 => {
                if a[4] != 0 {
                    return v(-EISCONN);
                }
                let Some(b) = self.bytes_at(a[1], a[2] as usize) else {
                    return v(-EFAULT);
                };
                dontwait(a[3], self.write_fd(fd, &b, None)).map_or_else(|r| r, |n| v(n as i64))
            },
            // recvfrom(fd, buf, len, flags, addr, addrlen)
            207 => match dontwait(a[3], self.read_fd(fd, a[2] as usize, None)) {
                Ok(b) => {
                    if !self.write(a[1], &b) || a[4] != 0 && !self.sockaddr_unnamed(a[4], a[5]) {
                        return v(-EFAULT);
                    }
                    v(b.len() as i64)
                },
                Err(r) => r,
            },
            // setsockopt: buffer sizes and the like are taken and change nothing here
            208 => v(0),
            // getsockopt(fd, level, name, val, len)
            209 => {
                if a[1] != SOL_SOCKET {
                    return v(-EOPNOTSUPP);
                }
                let value: u32 = match a[2] {
                    SO_TYPE => SOCK_STREAM as u32,
                    SO_ERROR => 0,
                    SO_DOMAIN => AF_UNIX as u32,
                    SO_SNDBUF | SO_RCVBUF => crate::files::PIPE_BUF_MAX as u32,
                    _ => return v(-EOPNOTSUPP),
                };
                let ok = self.u32_at(a[4]).is_some_and(|len| len >= 4)
                    && self.put_u32(a[3], value)
                    && self.put_u32(a[4], 4);
                v(if ok { 0 } else { -EFAULT })
            },
            // shutdown(fd, how): SHUT_RD 0, SHUT_WR 1, SHUT_RDWR 2
            210 => {
                let how = match a[1] {
                    0 => SHUT_RD,
                    1 => SHUT_WR,
                    2 => SHUT_RD | SHUT_WR,
                    _ => return v(-EINVAL),
                };
                if let Some(d) = self.files.desc_of(fd) {
                    self.files.shutdown(d, how & !shut);
                    self.poked = true;
                }
                v(0)
            },
            // sendmsg(fd, msghdr, flags), recvmsg(fd, msghdr, flags)
            211 | 212 => self.sock_msg(nr == 211, fd, a[1], a[2]),
            _ => v(-EINVAL),
        }
    }

    /// socketpair(domain, type, protocol, sv): two connected ends in sv[0] and sv[1].
    fn socketpair(&mut self, domain: u64, ty: u64, protocol: u64, sv: u64) -> Ret {
        let v = Ret::Val;
        if domain != AF_UNIX {
            return v(-EAFNOSUPPORT);
        }
        if !matches!(
            ty & SOCK_TYPE_MASK,
            SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET
        ) {
            return v(-EINVAL);
        }
        if protocol != 0 {
            return v(-EPROTONOSUPPORT);
        }
        if ty & !(SOCK_TYPE_MASK | O_NONBLOCK as u64 | O_CLOEXEC as u64) != 0 {
            return v(-EINVAL);
        }
        if self.files.fds.len() + 2 > 1024 {
            return v(-EMFILE);
        }
        let flags = O_RDWR | ty as u32 & O_NONBLOCK;
        let cloexec = ty & O_CLOEXEC as u64 != 0;
        let (p, q) = (self.files.new_pipe(), self.files.new_pipe());
        let a = self.files.add(
            Kind::Sock {
                rx: p,
                tx: q,
                shut: 0,
            },
            flags,
        );
        let b = self.files.add(
            Kind::Sock {
                rx: q,
                tx: p,
                shut: 0,
            },
            flags,
        );
        let fa = self.files.install(a, 0, cloexec);
        let fb = self.files.install(b, 0, cloexec);
        if !(self.put_u32(sv, fa as u32) && self.put_u32(sv + 4, fb as u32)) {
            self.files.close(fa);
            self.files.close(fb);
            return v(-EFAULT);
        }
        v(0)
    }

    /// sendmsg or recvmsg on a socket pair's end: the msghdr's iovecs, with no address and no
    /// control messages.
    fn sock_msg(&mut self, send: bool, fd: i32, msg: u64, flags: u64) -> Ret {
        let v = Ret::Val;
        // struct msghdr: name, namelen, iov, iovlen, control, controllen, flags
        let field = |o: u64| self.u64_at(msg + o);
        let (Some(name), Some(iov), Some(iovlen), Some(controllen)) =
            (field(0), field(16), field(24), field(40))
        else {
            return v(-EFAULT);
        };
        let Some(iov) = self.iovecs(iov, iovlen) else {
            return v(-EFAULT);
        };
        if send {
            if name != 0 {
                return v(-EISCONN);
            }
            if controllen != 0 {
                // Passing descriptors or credentials: not here
                return v(-EOPNOTSUPP);
            }
            let mut all = Vec::new();
            for (base, len) in iov {
                let Some(b) = self.bytes_at(base, len as usize) else {
                    return v(-EFAULT);
                };
                all.extend_from_slice(&b);
            }
            return dontwait(flags, self.write_fd(fd, &all, None))
                .map_or_else(|r| r, |n| v(n as i64));
        }
        let total: u64 = iov.iter().map(|&(_, l)| l).sum();
        match dontwait(flags, self.read_fd(fd, total as usize, None)) {
            Ok(b) => {
                let mut off = 0;
                for (base, len) in iov {
                    let n = (len as usize).min(b.len() - off);
                    if !self.write(base, &b[off..off + n]) {
                        return v(-EFAULT);
                    }
                    off += n;
                }
                // No address, no control messages, no flags
                let ok = self.put_u32(msg + 8, 0)
                    && self.put_u64(msg + 40, 0)
                    && self.put_u32(msg + 48, 0);
                v(if ok { b.len() as i64 } else { -EFAULT })
            },
            Err(r) => r,
        }
    }

    /// An unnamed AF_UNIX address at `addr` (as much as `*len` allows), and its length in `*len`.
    fn sockaddr_unnamed(&self, addr: u64, len: u64) -> bool {
        let Some(room) = self.u32_at(len) else {
            return false;
        };
        let family = (AF_UNIX as u16).to_le_bytes();
        let n = (room as usize).min(2);
        (n == 0 || self.write(addr, &family[..n])) && self.put_u32(len, 2)
    }
}

/// MSG_DONTWAIT: a call that would wait returns EAGAIN instead.
fn dontwait<T>(flags: u64, r: Result<T, Ret>) -> Result<T, Ret> {
    match r {
        Err(Ret::Retry { .. }) if flags & MSG_DONTWAIT != 0 => Err(Ret::Val(-EAGAIN)),
        r => r,
    }
}
