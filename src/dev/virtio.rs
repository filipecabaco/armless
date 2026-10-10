//! virtio 1.x: split virtqueues and the modern PCI transport's registers
//! (common configuration, ISR, device configuration, notifications).
//!
//! Interrupts are legacy INTx (no MSI-X capability): the ISR register says
//! why, and reading it lowers the line.

use std::sync::atomic::{fence, Ordering};

use super::console::Console;
use super::net::Net;
use super::p9::P9;
use super::Io;
use crate::ser::{R, W};

pub const F_VERSION_1: u64 = 1 << 32;
pub const F_RING_INDIRECT_DESC: u64 = 1 << 28;

const DESC_F_NEXT: u16 = 1;
const DESC_F_WRITE: u16 = 2;
const DESC_F_INDIRECT: u16 = 4;

const STATUS_DRIVER_OK: u8 = 4;
const STATUS_NEEDS_RESET: u8 = 0x40;

/// One descriptor chain: buffers the device reads, then buffers it writes.
#[derive(Clone, Debug, Default)]
pub struct Chain {
    pub head: u16,
    pub readable: Vec<(u64, u32)>,
    pub writable: Vec<(u64, u32)>,
}

impl Chain {
    pub fn read_len(&self) -> usize {
        self.readable.iter().map(|&(_, l)| l as usize).sum()
    }

    pub fn write_len(&self) -> usize {
        self.writable.iter().map(|&(_, l)| l as usize).sum()
    }

    /// Everything the driver gave the device to read.
    pub fn read_all(&self, io: &Io) -> Vec<u8> {
        let mut out = vec![0; self.read_len()];
        let mut at = 0;
        for &(addr, len) in &self.readable {
            io.read(addr, &mut out[at..at + len as usize]);
            at += len as usize;
        }
        out
    }

    /// Fill the writable buffers from `data`; returns the bytes written.
    pub fn write_all(&self, io: &Io, data: &[u8]) -> usize {
        let mut at = 0;
        for &(addr, len) in &self.writable {
            if at >= data.len() {
                break;
            }
            let n = (len as usize).min(data.len() - at);
            io.write(addr, &data[at..at + n]);
            at += n;
        }
        at
    }
}

#[derive(Clone, Debug)]
pub struct Queue {
    pub max: u16,
    pub size: u16,
    pub ready: bool,
    pub desc: u64,
    pub avail: u64,
    pub used: u64,
    pub last_avail: u16,
    pub used_idx: u16,
}

impl Queue {
    pub fn new(max: u16) -> Queue {
        Queue {
            max,
            size: max,
            ready: false,
            desc: 0,
            avail: 0,
            used: 0,
            last_avail: 0,
            used_idx: 0,
        }
    }

    fn reset(&mut self) {
        *self = Queue::new(self.max);
    }

    /// Whether the driver has made a buffer available.
    pub fn has_request(&self, io: &Io) -> bool {
        if !self.ready || self.size == 0 {
            return false;
        }
        fence(Ordering::SeqCst);
        io.read_u16(self.avail + 2) != self.last_avail
    }

    /// Take the next available chain.
    pub fn pop(&mut self, io: &Io) -> Option<Chain> {
        if !self.has_request(io) {
            return None;
        }
        let slot = self.last_avail % self.size;
        let head = io.read_u16(self.avail + 4 + 2 * slot as u64);
        self.last_avail = self.last_avail.wrapping_add(1);
        self.walk(io, head)
    }

    fn walk(&self, io: &Io, head: u16) -> Option<Chain> {
        let mut chain = Chain {
            head,
            ..Default::default()
        };
        let mut table = self.desc;
        let mut table_size = self.size as u32;
        let mut i = head as u32;
        let mut steps = 0;
        loop {
            if i >= table_size || steps > 4096 {
                return Some(chain); // malformed: give back what we have
            }
            steps += 1;
            let d = table + 16 * i as u64;
            let addr = io.read_u64(d);
            let len = io.read_u32(d + 8);
            let flags = io.read_u16(d + 12);
            let next = io.read_u16(d + 14);
            if flags & DESC_F_INDIRECT != 0 {
                table = addr;
                table_size = len / 16;
                i = 0;
                continue;
            }
            if flags & DESC_F_WRITE != 0 {
                chain.writable.push((addr, len));
            } else {
                chain.readable.push((addr, len));
            }
            if flags & DESC_F_NEXT == 0 {
                return Some(chain);
            }
            i = next as u32;
        }
    }

    /// Return a chain to the driver. Returns whether the driver wants an
    /// interrupt for it.
    pub fn push(&mut self, io: &Io, head: u16, len: u32) -> bool {
        let slot = self.used_idx % self.size;
        let e = self.used + 4 + 8 * slot as u64;
        io.write_u32(e, head as u32);
        io.write_u32(e + 4, len);
        fence(Ordering::SeqCst);
        self.used_idx = self.used_idx.wrapping_add(1);
        io.write_u16(self.used + 2, self.used_idx);
        fence(Ordering::SeqCst);
        // VIRTQ_AVAIL_F_NO_INTERRUPT
        io.read_u16(self.avail) & 1 == 0
    }

    fn save(&self, w: &mut W) {
        w.u16(self.size);
        w.bool(self.ready);
        w.u64(self.desc);
        w.u64(self.avail);
        w.u64(self.used);
        w.u16(self.last_avail);
        w.u16(self.used_idx);
    }

    fn load(&mut self, r: &mut R) -> Option<()> {
        self.size = r.u16()?;
        self.ready = r.bool()?;
        self.desc = r.u64()?;
        self.avail = r.u64()?;
        self.used = r.u64()?;
        self.last_avail = r.u16()?;
        self.used_idx = r.u16()?;
        Some(())
    }
}

/// The transport side of a device: feature negotiation, status, queues and
/// the interrupt line.
pub struct Transport {
    pub intid: u32,
    pub device_features: u64,
    pub driver_features: u64,
    device_feature_select: u32,
    driver_feature_select: u32,
    pub status: u8,
    config_generation: u8,
    queue_select: u16,
    pub queues: Vec<Queue>,
    isr: u8,
    line: bool,
}

impl Transport {
    pub fn new(intid: u32, device_features: u64, queues: &[u16]) -> Transport {
        Transport {
            intid,
            device_features: device_features | F_VERSION_1 | F_RING_INDIRECT_DESC,
            driver_features: 0,
            device_feature_select: 0,
            driver_feature_select: 0,
            status: 0,
            config_generation: 0,
            queue_select: 0,
            queues: queues.iter().map(|&m| Queue::new(m)).collect(),
            isr: 0,
            line: false,
        }
    }

    pub fn driver_ok(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0 && self.status & STATUS_NEEDS_RESET == 0
    }

    pub fn has_feature(&self, f: u64) -> bool {
        self.driver_features & f != 0
    }

    fn set_line(&mut self, io: &Io) {
        let level = self.isr != 0;
        if level != self.line {
            self.line = level;
            io.set_irq(self.intid, level);
        }
    }

    /// A used buffer is ready.
    pub fn interrupt(&mut self, io: &Io) {
        self.isr |= 1;
        self.set_line(io);
    }

    /// The device configuration changed.
    pub fn config_changed(&mut self, io: &Io) {
        self.config_generation = self.config_generation.wrapping_add(1);
        if self.driver_ok() {
            self.isr |= 2;
            self.set_line(io);
        }
    }

    /// Return a chain on queue `q` and interrupt if the driver wants it.
    pub fn complete(&mut self, io: &Io, q: usize, head: u16, len: u32) {
        if self.queues[q].push(io, head, len) {
            self.interrupt(io);
        }
    }

    pub fn reset(&mut self, io: &Io) {
        self.driver_features = 0;
        self.device_feature_select = 0;
        self.driver_feature_select = 0;
        self.status = 0;
        self.queue_select = 0;
        for q in &mut self.queues {
            q.reset();
        }
        self.isr = 0;
        self.set_line(io);
    }

    fn selected(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.queue_select as usize)
    }

    /// Common configuration structure.
    pub fn common_read(&mut self, off: u64) -> u32 {
        let q = self.queues.get(self.queue_select as usize).cloned();
        match off {
            0x00 => self.device_feature_select,
            0x04 => match self.device_feature_select {
                0 => self.device_features as u32,
                1 => (self.device_features >> 32) as u32,
                _ => 0,
            },
            0x08 => self.driver_feature_select,
            0x0c => match self.driver_feature_select {
                0 => self.driver_features as u32,
                1 => (self.driver_features >> 32) as u32,
                _ => 0,
            },
            0x10 => 0xffff, // config_msix_vector: no MSI-X
            0x12 => self.queues.len() as u32,
            0x14 => self.status as u32,
            0x15 => self.config_generation as u32,
            0x16 => self.queue_select as u32,
            0x18 => q.map_or(0, |q| q.size as u32),
            0x1a => 0xffff,
            0x1c => q.map_or(0, |q| q.ready as u32),
            0x1e => self.queue_select as u32, // queue_notify_off
            0x20 => q.map_or(0, |q| q.desc as u32),
            0x24 => q.map_or(0, |q| (q.desc >> 32) as u32),
            0x28 => q.map_or(0, |q| q.avail as u32),
            0x2c => q.map_or(0, |q| (q.avail >> 32) as u32),
            0x30 => q.map_or(0, |q| q.used as u32),
            0x34 => q.map_or(0, |q| (q.used >> 32) as u32),
            _ => 0,
        }
    }

    /// Returns true if the device was reset (status written as 0).
    pub fn common_write(&mut self, io: &Io, off: u64, _size: u32, v: u32) -> bool {
        let lo = |old: u64, v: u32| (old & !0xffff_ffff) | v as u64;
        let hi = |old: u64, v: u32| (old & 0xffff_ffff) | (v as u64) << 32;
        match off {
            0x00 => self.device_feature_select = v,
            0x08 => self.driver_feature_select = v,
            0x0c => match self.driver_feature_select {
                0 => self.driver_features = lo(self.driver_features, v) & self.device_features,
                1 => self.driver_features = hi(self.driver_features, v) & self.device_features,
                _ => {},
            },
            0x14 => {
                if v as u8 == 0 {
                    self.reset(io);
                    return true;
                }
                self.status = v as u8;
            },
            0x16 => self.queue_select = v as u16,
            0x18 => {
                if let Some(q) = self.selected() {
                    // Must be a power of two no larger than the maximum.
                    let n = (v as u16).min(q.max);
                    q.size = if n.is_power_of_two() { n } else { q.max };
                }
            },
            0x1c => {
                if let Some(q) = self.selected() {
                    q.ready = v & 1 != 0;
                }
            },
            0x20 => {
                if let Some(q) = self.selected() {
                    q.desc = lo(q.desc, v)
                }
            },
            0x24 => {
                if let Some(q) = self.selected() {
                    q.desc = hi(q.desc, v)
                }
            },
            0x28 => {
                if let Some(q) = self.selected() {
                    q.avail = lo(q.avail, v)
                }
            },
            0x2c => {
                if let Some(q) = self.selected() {
                    q.avail = hi(q.avail, v)
                }
            },
            0x30 => {
                if let Some(q) = self.selected() {
                    q.used = lo(q.used, v)
                }
            },
            0x34 => {
                if let Some(q) = self.selected() {
                    q.used = hi(q.used, v)
                }
            },
            _ => {},
        }
        false
    }

    /// ISR status: reading acknowledges the interrupt.
    pub fn isr_read(&mut self, io: &Io) -> u32 {
        let v = self.isr;
        self.isr = 0;
        self.set_line(io);
        v as u32
    }

    pub fn save(&self, w: &mut W) {
        w.u64(self.driver_features);
        w.u32(self.device_feature_select);
        w.u32(self.driver_feature_select);
        w.u8(self.status);
        w.u8(self.config_generation);
        w.u16(self.queue_select);
        w.u8(self.isr);
        w.bool(self.line);
        w.u32(self.queues.len() as u32);
        for q in &self.queues {
            q.save(w);
        }
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.driver_features = r.u64()?;
        self.device_feature_select = r.u32()?;
        self.driver_feature_select = r.u32()?;
        self.status = r.u8()?;
        self.config_generation = r.u8()?;
        self.queue_select = r.u16()?;
        self.isr = r.u8()?;
        self.line = r.bool()?;
        let n = r.u32()? as usize;
        if n != self.queues.len() {
            return None;
        }
        for q in &mut self.queues {
            q.load(r)?;
        }
        Some(())
    }
}

/// The device behind a transport.
pub enum Device {
    Console(Console),
    P9(P9),
    Net(Net),
}

impl Device {
    pub fn device_type(&self) -> u16 {
        match self {
            Device::Net(_) => 1,
            Device::Console(_) => 3,
            Device::P9(_) => 9,
        }
    }

    pub fn class_code(&self) -> u32 {
        match self {
            Device::Net(_) => 0x02_00_00,
            Device::Console(_) => 0x07_80_00,
            Device::P9(_) => 0x01_80_00,
        }
    }

    pub fn features(&self) -> u64 {
        match self {
            Device::Console(_) => Console::FEATURES,
            Device::P9(_) => P9::FEATURES,
            Device::Net(_) => Net::FEATURES,
        }
    }

    pub fn queue_sizes(&self) -> &'static [u16] {
        match self {
            Device::Console(_) => &[128, 128],
            Device::P9(_) => &[128],
            Device::Net(_) => &[256, 256],
        }
    }

    pub fn config_read(&self, off: u64) -> u8 {
        match self {
            Device::Console(c) => c.config_read(off),
            Device::P9(p) => p.config_read(off),
            Device::Net(n) => n.config_read(off),
        }
    }

    pub fn notify(&mut self, io: &Io, t: &mut Transport, q: usize) {
        if !t.driver_ok() {
            return;
        }
        match self {
            Device::Console(c) => c.notify(io, t, q),
            Device::P9(p) => p.notify(io, t, q),
            Device::Net(n) => n.notify(io, t, q),
        }
    }

    /// The driver set DRIVER_OK.
    pub fn activate(&mut self, io: &Io, t: &mut Transport) {
        match self {
            Device::Console(c) => c.activate(io, t),
            Device::P9(_) => {},
            Device::Net(n) => n.activate(io, t),
        }
    }

    pub fn reset(&mut self) {
        match self {
            Device::Console(c) => c.reset(),
            Device::P9(p) => p.reset(),
            Device::Net(n) => n.reset(),
        }
    }

    pub fn save(&self, w: &mut W) {
        match self {
            Device::Console(c) => c.save(w),
            Device::P9(p) => p.save(w),
            Device::Net(n) => n.save(w),
        }
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        match self {
            Device::Console(c) => c.load(r),
            Device::P9(p) => p.load(r),
            Device::Net(n) => n.load(r),
        }
    }
}
