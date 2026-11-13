//! ioctl: the terminal's, FIONREAD, and the network interfaces' (SIOCGIF*).

use crate::errno::{EBADF, EFAULT, ENOTTY};
use crate::files::{Kind, O_ASYNC, O_NONBLOCK};
use crate::{Process, Ret};


impl Process {
    /// A SIOCGIF* ioctl: struct ifreq at `arg` (SIOCGIFCONF: struct ifconf, a length and where)
    pub(super) fn ifreq_ioctl(&mut self, req: u32, arg: u64) -> i64 {
        let network = self.board.as_ref().is_some_and(|b| b.has_network());
        let guest = crate::net::GUEST_IP;
        if req == 0x8912 {
            // struct ifconf: ifc_len (int), pad, ifc_buf (pointer); a NULL buffer asks the length
            let (Some(room), Some(buf)) = (self.u32_at(arg), self.u64_at(arg + 8)) else {
                return -EFAULT;
            };
            let all = crate::netlink::ifconf(if buf == 0 { usize::MAX } else { room as usize }, network, guest);
            if buf != 0 && !self.write(buf, &all) {
                return -EFAULT;
            }
            return if self.put_u32(arg, all.len() as u32) { 0 } else { -EFAULT };
        }
        let Some(b) = self.bytes_at(arg, 40) else {
            return -EFAULT;
        };
        let mut ifr: [u8; 40] = b.try_into().unwrap();
        let r = crate::netlink::ifreq(req, &mut ifr, network, guest);
        if r == 0 && !self.write(arg, &ifr) {
            return -EFAULT;
        }
        r
    }

    pub(super) fn ioctl(&mut self, fd: i32, req: u32, arg: u64) -> Ret {
        let v = Ret::Val;
        let Some(d) = self.files.desc_of(fd) else {
            return v(-EBADF);
        };
        match req {
            // FIONBIO
            0x5421 => {
                let on = self.u32_at(arg).unwrap_or(0) != 0;
                let desc = self.files.descs.get_mut(&d).unwrap();
                desc.flags = if on { desc.flags | O_NONBLOCK } else { desc.flags & !O_NONBLOCK };
                return v(0);
            },
            // FIOASYNC: the flag is kept, but SIGIO isn't sent: a program that asks for it
            // (nginx does, for the channel between its processes) reads with its event loop
            0x5452 => {
                let on = self.u32_at(arg).unwrap_or(0) != 0;
                let desc = self.files.descs.get_mut(&d).unwrap();
                desc.flags = if on { desc.flags | O_ASYNC } else { desc.flags & !O_ASYNC };
                return v(0);
            },
            // FIOCLEX, FIONCLEX
            0x5451 | 0x5450 => {
                self.files.fds.get_mut(&fd).unwrap().1 = req == 0x5451;
                return v(0);
            },
            _ => {},
        }
        // An interface's name, index, flags, addresses, MTU or queue, asked of any socket
        // (SIOCGIF*: netlink.rs's table)
        if (0x8910..=0x8950).contains(&req) && matches!(self.files.descs[&d].kind, Kind::Sock(_)) {
            return v(self.ifreq_ioctl(req, arg));
        }
        let kind = self.files.descs[&d].kind.clone();
        match (kind, req) {
            // TCGETS
            (Kind::Tty, 0x5401) => {
                v(if self.write(arg, &self.tty.termios.to_bytes()) { 0 } else { -EFAULT })
            },
            // TCSETS, TCSETSW, TCSETSF
            (Kind::Tty, 0x5402..=0x5404) => match self.bytes_at(arg, 36) {
                Some(b) => {
                    self.tty.set(
                        crate::tty::Termios::from_bytes(&b.try_into().unwrap()),
                        req == 0x5404,
                    );
                    self.poke();
                    v(0)
                },
                None => v(-EFAULT),
            },
            // TIOCGWINSZ
            (Kind::Tty, 0x5413) => {
                let mut b = [0u8; 8];
                b[0..2].copy_from_slice(&self.tty.rows.to_le_bytes());
                b[2..4].copy_from_slice(&self.tty.cols.to_le_bytes());
                v(if self.write(arg, &b) { 0 } else { -EFAULT })
            },
            // TIOCSWINSZ
            (Kind::Tty, 0x5414) => match self.bytes_at(arg, 4) {
                Some(b) => {
                    self.resize(
                        u16::from_le_bytes([b[2], b[3]]),
                        u16::from_le_bytes([b[0], b[1]]),
                    );
                    v(0)
                },
                None => v(-EFAULT),
            },
            // TIOCGPGRP, TIOCGSID
            (Kind::Tty, 0x540f | 0x5429) => {
                v(if self.put_u32(arg, self.tty.pgrp as u32) { 0 } else { -EFAULT })
            },
            // TIOCSPGRP
            (Kind::Tty, 0x5410) => {
                if let Some(p) = self.u32_at(arg) {
                    self.tty.pgrp = p as i64;
                }
                v(0)
            },
            // FIONREAD
            (Kind::Tty, 0x541b) => {
                v(if self.put_u32(arg, self.tty.pending() as u32) { 0 } else { -EFAULT })
            },
            (Kind::PipeRead(p), 0x541b) => {
                let n = self.files.pipes.get(&p).map_or(0, |p| p.buf.len());
                v(if self.put_u32(arg, n as u32) { 0 } else { -EFAULT })
            },
            (Kind::Sock(id), 0x541b) => {
                let n = self.sock_pending(id);
                v(if self.put_u32(arg, n as u32) { 0 } else { -EFAULT })
            },
            // TIOCSCTTY, TCFLSH, TCXONC, TIOCNOTTY, TCSBRK
            (Kind::Tty, 0x540e | 0x540b | 0x540a | 0x5422 | 0x5409) => v(0),
            (Kind::File { .. } | Kind::Dir { .. }, _) => v(-ENOTTY),
            _ => v(-ENOTTY),
        }
    }
}
