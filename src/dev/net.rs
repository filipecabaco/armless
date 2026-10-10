//! virtio-net, wired to the user-mode network in [`netstack`].
//!
//! No offloads: frames are at most 1514 bytes with checksums filled in, and
//! the 12-byte virtio-net header is all zeros (num_buffers = 1).

use super::netstack::Stack;
use super::virtio::Transport;
use super::Io;
use crate::ser::{R, W};

const F_MAC: u64 = 1 << 5;
const RX: usize = 0;
const TX: usize = 1;
const HDR: usize = 12;

pub struct Net {
    pub mac: [u8; 6],
    pub stack: Stack,
}

impl Net {
    pub const FEATURES: u64 = F_MAC;

    pub fn new(mac: [u8; 6]) -> Net {
        Net {
            mac,
            stack: Stack::new(mac),
        }
    }

    pub fn config_read(&self, off: u64) -> u8 {
        self.mac.get(off as usize).copied().unwrap_or(0)
    }

    pub fn reset(&mut self) {}

    pub fn activate(&mut self, io: &Io, t: &mut Transport) {
        self.deliver(io, t);
    }

    /// Move queued frames into the guest's receive buffers.
    pub fn deliver(&mut self, io: &Io, t: &mut Transport) {
        if !t.driver_ok() {
            return;
        }
        let mut done = false;
        while !self.stack.frames.is_empty() {
            let Some(chain) = t.queues[RX].pop(io) else {
                break;
            };
            let frame = self.stack.frames.pop_front().unwrap();
            let mut data = vec![0u8; HDR + frame.len()];
            data[10] = 1; // num_buffers
            data[HDR..].copy_from_slice(&frame);
            let n = chain.write_all(io, &data);
            if t.queues[RX].push(io, chain.head, n as u32) {
                done = true;
            }
        }
        if done {
            t.interrupt(io);
        }
    }

    pub fn notify(&mut self, io: &Io, t: &mut Transport, q: usize) {
        match q {
            RX => self.deliver(io, t),
            TX => {
                let mut irq = false;
                while let Some(chain) = t.queues[TX].pop(io) {
                    let data = chain.read_all(io);
                    if data.len() > HDR {
                        self.stack.receive(&data[HDR..], &io.out);
                    }
                    if t.queues[TX].push(io, chain.head, 0) {
                        irq = true;
                    }
                }
                if irq {
                    t.interrupt(io);
                }
                self.deliver(io, t);
            },
            _ => {},
        }
    }

    pub fn save(&self, w: &mut W) {
        w.0.extend_from_slice(&self.mac);
        self.stack.save(w);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.mac = r.take(6)?.try_into().ok()?;
        self.stack.load(r)
    }
}
