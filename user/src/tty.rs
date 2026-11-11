//! The terminal: what Linux's tty and its line discipline (n_tty) do for a program whose stdin,
//! stdout and stderr are a terminal. termios as the program sets it, the window size the host
//! gives, and the input typed into it: in canonical mode, edited a line at a time and echoed;
//! in raw mode, passed through. Ctrl-C and the like become signals.

use std::collections::VecDeque;

use crate::ser::{R, W};

// c_iflag, c_oflag, c_lflag bits (asm-generic)
const ICRNL: u32 = 0o400;
const IXON: u32 = 0o2000;
const IUTF8: u32 = 0o40000;
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;
// c_cc indices
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSUSP: usize = 10;
const NCCS: usize = 19;

pub(crate) const SIGINT: u64 = 2;
pub(crate) const SIGQUIT: u64 = 3;
pub(crate) const SIGTSTP: u64 = 20;

/// The kernel's struct termios (TCGETS and TCSETS read and write these 36 bytes).
#[derive(Clone)]
pub(crate) struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; NCCS],
}

impl Default for Termios {
    /// What a fresh terminal has (a Linux console, cooked, UTF-8)
    fn default() -> Termios {
        let mut cc = [0u8; NCCS];
        for (i, v) in [
            (VINTR, 3),
            (VQUIT, 0x1c),
            (VERASE, 0x7f),
            (VKILL, 0x15),
            (VEOF, 4),
            (VMIN, 1),
            (8, 0x11),
            (9, 0x13),
            (VSUSP, 0x1a),
            (12, 0x12),
            (14, 0x17),
            (15, 0x16),
        ] {
            cc[i] = v;
        }
        Termios {
            iflag: ICRNL | IXON | IUTF8,
            oflag: OPOST | ONLCR,
            cflag: 0o17 | 0o60 | 0o200 | 0o2000, // B38400 CS8 CREAD HUPCL
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc,
        }
    }
}

impl Termios {
    pub fn to_bytes(&self) -> [u8; 36] {
        let mut b = [0u8; 36];
        b[0..4].copy_from_slice(&self.iflag.to_le_bytes());
        b[4..8].copy_from_slice(&self.oflag.to_le_bytes());
        b[8..12].copy_from_slice(&self.cflag.to_le_bytes());
        b[12..16].copy_from_slice(&self.lflag.to_le_bytes());
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b
    }

    pub fn from_bytes(b: &[u8; 36]) -> Termios {
        let u = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        Termios {
            iflag: u(0),
            oflag: u(4),
            cflag: u(8),
            lflag: u(12),
            line: b[16],
            cc: b[17..36].try_into().unwrap(),
        }
    }
}

/// The terminal's state.
pub(crate) struct Tty {
    pub termios: Termios,
    pub cols: u16,
    pub rows: u16,
    /// Input the program can read now (lines, in canonical mode)
    queue: VecDeque<u8>,
    /// The line being edited (canonical mode)
    line: Vec<u8>,
    /// Reads that see the end of input (Ctrl-D on an empty line), then input again
    eofs: usize,
    /// The host closed the terminal's input: every read after the queue is 0
    pub closed: bool,
    /// The terminal's foreground process group (TIOCGPGRP, TIOCSPGRP)
    pub pgrp: i64,
}

impl Tty {
    pub fn new(pgrp: i64) -> Tty {
        Tty {
            termios: Termios::default(),
            cols: 80,
            rows: 24,
            queue: VecDeque::new(),
            line: Vec::new(),
            eofs: 0,
            closed: false,
            pgrp,
        }
    }

    fn canonical(&self) -> bool {
        self.termios.lflag & ICANON != 0
    }

    /// New termios (TCSETS). Leaving canonical mode makes the line being edited readable, as
    /// n_tty does; `flush` (TCSETSF) drops the input not read yet first.
    pub fn set(&mut self, t: Termios, flush: bool) {
        if flush {
            self.queue.clear();
            self.line.clear();
        }
        let leaving_canonical = self.canonical() && t.lflag & ICANON == 0;
        self.termios = t;
        if leaving_canonical {
            self.queue.extend(self.line.drain(..));
        }
    }

    /// Bytes typed in. Returns what to echo back to the screen, and the signals the keys raise.
    pub fn input(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u64>) {
        let t = self.termios.clone();
        let echo = t.lflag & ECHO != 0;
        let mut out = Vec::new();
        let mut signals = Vec::new();
        for &b0 in bytes {
            let b = if b0 == b'\r' && t.iflag & ICRNL != 0 { b'\n' } else { b0 };
            if t.lflag & ISIG != 0 {
                let sig = [(VINTR, SIGINT), (VQUIT, SIGQUIT), (VSUSP, SIGTSTP)]
                    .into_iter()
                    .find(|&(i, _)| t.cc[i] != 0 && b == t.cc[i]);
                if let Some((_, sig)) = sig {
                    self.line.clear();
                    if echo {
                        out.extend_from_slice(&[b'^', b ^ 0x40, b'\r', b'\n']);
                    }
                    signals.push(sig);
                    continue;
                }
            }
            if !self.canonical() {
                self.queue.push_back(b);
                if echo {
                    self.echo(b, &mut out);
                }
                continue;
            }
            if b == t.cc[VERASE] || b == 8 {
                if self.line.pop().is_some() && echo && t.lflag & ECHOE != 0 {
                    out.extend_from_slice(b"\x08 \x08");
                }
            } else if b == t.cc[VKILL] {
                if echo {
                    for _ in 0..self.line.len() {
                        out.extend_from_slice(b"\x08 \x08");
                    }
                }
                self.line.clear();
            } else if b == t.cc[VEOF] {
                if self.line.is_empty() {
                    self.eofs += 1;
                } else {
                    self.queue.extend(self.line.drain(..));
                }
            } else {
                self.line.push(b);
                if echo {
                    self.echo(b, &mut out);
                }
                if b == b'\n' {
                    self.queue.extend(self.line.drain(..));
                }
            }
        }
        (out, signals)
    }

    fn echo(&self, b: u8, out: &mut Vec<u8>) {
        if b == b'\n' {
            out.extend_from_slice(b"\r\n");
        } else if b < 0x20 && b != b'\t' && self.termios.lflag & ECHOCTL != 0 {
            out.extend_from_slice(&[b'^', b + 0x40]);
        } else {
            out.push(b);
        }
    }

    /// What a write to the terminal shows (OPOST, ONLCR: a newline starts a line too).
    pub fn output(&self, data: &[u8]) -> Vec<u8> {
        if self.termios.oflag & (OPOST | ONLCR) != OPOST | ONLCR {
            return data.to_vec();
        }
        let mut out = Vec::with_capacity(data.len() + 8);
        for &b in data {
            if b == b'\n' {
                out.push(b'\r');
            }
            out.push(b);
        }
        out
    }

    /// Whether a read would return now (data, an end of input, or a closed terminal).
    pub fn readable(&self) -> bool {
        !self.queue.is_empty()
            || self.eofs > 0
            || self.closed
            || (!self.canonical() && self.termios.cc[VMIN] == 0)
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    /// A read of up to `max` bytes, or None when it must wait. In canonical mode at most a line;
    /// in raw mode what is there (VMIN 0: even nothing).
    pub fn read(&mut self, max: usize) -> Option<Vec<u8>> {
        if self.queue.is_empty() {
            if self.eofs > 0 {
                self.eofs -= 1;
                return Some(Vec::new());
            }
            if self.closed || (!self.canonical() && self.termios.cc[VMIN] == 0) {
                return Some(Vec::new());
            }
            return None;
        }
        let mut n = self.queue.len().min(max);
        if self.canonical() {
            if let Some(nl) = self.queue.iter().position(|&b| b == b'\n') {
                n = n.min(nl + 1);
            }
        }
        Some(self.queue.drain(..n).collect())
    }

    /// How long a raw read with VMIN 0 and VTIME set may wait (VTIME is tenths of a second).
    pub fn read_timeout_ms(&self) -> Option<u64> {
        (!self.canonical() && self.termios.cc[VMIN] == 0 && self.termios.cc[VTIME] > 0)
            .then(|| self.termios.cc[VTIME] as u64 * 100)
    }

    pub fn save(&self, w: &mut W) {
        w.b(&self.termios.to_bytes());
        w.u(self.cols as u64);
        w.u(self.rows as u64);
        w.b(&self.queue.iter().copied().collect::<Vec<u8>>());
        w.b(&self.line);
        w.u(self.eofs as u64);
        w.u(self.closed as u64);
        w.u(self.pgrp as u64);
    }

    pub fn load(r: &mut R) -> Result<Tty, String> {
        let termios = Termios::from_bytes(r.b()?.try_into().map_err(|_| "bad termios")?);
        Ok(Tty {
            termios,
            cols: r.u()? as u16,
            rows: r.u()? as u16,
            queue: r.b()?.iter().copied().collect(),
            line: r.b()?.to_vec(),
            eofs: r.u()? as usize,
            closed: r.u()? != 0,
            pgrp: r.u()? as i64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_edited_then_readable_whole() {
        let mut t = Tty::new(1);
        let (echo, sigs) = t.input(b"helo\x7f\x7fllo");
        assert_eq!(echo, b"helo\x08 \x08\x08 \x08llo");
        assert!(sigs.is_empty());
        assert!(!t.readable(), "no line until its end");
        t.input(b"\r");
        assert_eq!(
            t.read(100),
            Some(b"hello\n".to_vec()),
            "ICRNL: the return ends the line"
        );
        assert_eq!(t.read(100), None, "then it waits");
    }

    #[test]
    fn a_read_gives_at_most_a_line() {
        let mut t = Tty::new(1);
        t.input(b"one\ntwo\n");
        assert_eq!(t.read(100), Some(b"one\n".to_vec()));
        assert_eq!(t.read(2), Some(b"tw".to_vec()));
        assert_eq!(t.read(100), Some(b"o\n".to_vec()));
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_the_end_of_input_once() {
        let mut t = Tty::new(1);
        t.input(b"\x04");
        assert_eq!(t.read(10), Some(Vec::new()));
        assert_eq!(t.read(10), None);
    }

    #[test]
    fn ctrl_c_is_sigint_and_drops_the_line() {
        let mut t = Tty::new(1);
        let (echo, sigs) = t.input(b"abc\x03");
        assert_eq!(sigs, vec![SIGINT]);
        assert!(echo.ends_with(b"^C\r\n"));
        t.input(b"\n");
        assert_eq!(t.read(10), Some(b"\n".to_vec()));
    }

    #[test]
    fn raw_mode_passes_bytes_through_and_leaving_canonical_keeps_the_line() {
        let mut t = Tty::new(1);
        t.input(b"q");
        let mut raw = t.termios.clone();
        raw.lflag &= !(ICANON | ECHO);
        t.set(raw, false);
        assert_eq!(
            t.read(10),
            Some(b"q".to_vec()),
            "what was typed before raw mode is readable"
        );
        let (echo, _) = t.input(b"\x1b[A\x7f");
        assert!(echo.is_empty(), "no echo");
        assert_eq!(t.read(10), Some(b"\x1b[A\x7f".to_vec()));
    }

    #[test]
    fn output_newlines_start_a_line_unless_opost_is_off() {
        let mut t = Tty::new(1);
        assert_eq!(t.output(b"a\nb"), b"a\r\nb");
        let mut raw = t.termios.clone();
        raw.oflag &= !OPOST;
        t.set(raw, false);
        assert_eq!(t.output(b"a\nb"), b"a\nb");
    }

    #[test]
    fn termios_round_trips_as_the_kernel_lays_it_out() {
        let t = Termios::default();
        let b = t.to_bytes();
        assert_eq!(&b[12..16], &t.lflag.to_le_bytes());
        assert_eq!(b[17 + VEOF], 4);
        assert_eq!(Termios::from_bytes(&b).to_bytes(), b);
    }
}
