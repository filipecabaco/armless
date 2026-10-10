//! virtio console (hvc0), single port, with the terminal size in its config
//! (VIRTIO_CONSOLE_F_SIZE): a resize is a config-change interrupt.
//!
//! Input is queued here until the guest has a receive buffer, so nothing
//! typed or pasted is lost.

use std::collections::VecDeque;

use super::virtio::Transport;
use super::{Event, Io};
use crate::ser::{R, W};

const F_SIZE: u64 = 1 << 0;
const RX: usize = 0;
const TX: usize = 1;
/// Input kept for the guest at most (a paste can be large; a flood is not).
const MAX_PENDING: usize = 16 << 20;

pub struct Console {
    pub cols: u16,
    pub rows: u16,
    pending: VecDeque<u8>,
    /// The size still has to be announced (once port 0 exists in the guest).
    announce: bool,
}

impl Default for Console {
    fn default() -> Self {
        Console {
            cols: 80,
            rows: 25,
            pending: VecDeque::new(),
            announce: false,
        }
    }
}

impl Console {
    pub const FEATURES: u64 = F_SIZE;

    pub fn config_read(&self, off: u64) -> u8 {
        let mut c = [0u8; 12];
        c[0..2].copy_from_slice(&self.cols.to_le_bytes());
        c[2..4].copy_from_slice(&self.rows.to_le_bytes());
        c[4..8].copy_from_slice(&1u32.to_le_bytes()); // max_nr_ports
        c.get(off as usize).copied().unwrap_or(0)
    }

    pub fn reset(&mut self) {
        self.announce = false;
    }

    pub fn activate(&mut self, _io: &Io, _t: &mut Transport) {
        // Linux only applies a size to port 0 once it exists, which is when
        // it first fills the receive queue.
        self.announce = true;
    }

    pub fn resize(&mut self, io: &Io, t: &mut Transport, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        t.config_changed(io);
    }

    pub fn input(&mut self, io: &Io, t: &mut Transport, bytes: &[u8]) {
        if self.pending.len() + bytes.len() <= MAX_PENDING {
            self.pending.extend(bytes);
        }
        self.deliver(io, t);
    }

    /// Bytes waiting for guest receive buffers.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    fn deliver(&mut self, io: &Io, t: &mut Transport) {
        if !t.driver_ok() {
            return;
        }
        while !self.pending.is_empty() {
            let Some(chain) = t.queues[RX].pop(io) else {
                break;
            };
            let n = chain.write_len().min(self.pending.len());
            let data: Vec<u8> = self.pending.drain(..n).collect();
            let written = chain.write_all(io, &data);
            t.complete(io, RX, chain.head, written as u32);
        }
    }

    pub fn notify(&mut self, io: &Io, t: &mut Transport, q: usize) {
        match q {
            RX => {
                if std::mem::take(&mut self.announce) {
                    t.config_changed(io);
                }
                self.deliver(io, t)
            },
            TX => {
                let mut out = Vec::new();
                while let Some(chain) = t.queues[TX].pop(io) {
                    out.extend(chain.read_all(io));
                    t.complete(io, TX, chain.head, 0);
                }
                if !out.is_empty() {
                    io.out.push(Event::Console(out));
                }
            },
            _ => {},
        }
    }

    pub fn save(&self, w: &mut W) {
        w.u16(self.cols);
        w.u16(self.rows);
        w.bytes(&self.pending.iter().copied().collect::<Vec<_>>());
        w.bool(self.announce);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.cols = r.u16()?;
        self.rows = r.u16()?;
        self.pending = r.bytes()?.iter().copied().collect();
        self.announce = r.bool()?;
        Some(())
    }
}
