//! PCI bus 0 behind the ECAM window, holding virtio-pci (modern) devices.
//!
//! Each device has one 32-bit memory BAR, assigned here at a fixed address
//! (the device tree says `linux,pci-probe-only`):
//!
//! ```text
//! BAR0 + 0x0000  common configuration
//! BAR0 + 0x1000  ISR status
//! BAR0 + 0x2000  device configuration
//! BAR0 + 0x3000  notifications (queue n at + 4 * n)
//! ```

use super::virtio::{Device, Transport};
use super::Io;
use crate::board;
use crate::ser::{R, W};

const BAR_SIZE: u32 = 0x4000;
const COMMON: u64 = 0x0000;
const ISR: u64 = 0x1000;
const DEVICE: u64 = 0x2000;
const NOTIFY: u64 = 0x3000;
const NOTIFY_MULTIPLIER: u32 = 4;

pub struct Function {
    pub slot: u8,
    config: [u8; 256],
    bar0: u32,
    pub transport: Transport,
    pub device: Device,
}

fn bar_base(slot: u8) -> u32 {
    (board::PCI_MMIO_BASE + slot as u64 * 0x10000) as u32
}

impl Function {
    fn new(slot: u8, device: Device) -> Function {
        let mut c = [0u8; 256];
        let put16 =
            |c: &mut [u8; 256], o: usize, v: u16| c[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put32 =
            |c: &mut [u8; 256], o: usize, v: u32| c[o..o + 4].copy_from_slice(&v.to_le_bytes());
        let ty = device.device_type();
        put16(&mut c, 0x00, 0x1af4); // vendor: Red Hat (virtio)
        put16(&mut c, 0x02, 0x1040 + ty); // modern virtio device
        put16(&mut c, 0x04, 0x0006); // memory space + bus master
        put16(&mut c, 0x06, 0x0010); // status: capabilities list
        put32(&mut c, 0x08, device.class_code() << 8 | 1); // class, revision 1
        put16(&mut c, 0x2c, 0x1af4);
        put16(&mut c, 0x2e, 0x40 + ty);
        c[0x34] = 0x40; // capabilities
        c[0x3c] = 0xff;
        c[0x3d] = 1; // INTA
                     // virtio capabilities: (cfg_type, offset, length)
        let caps = [
            (1u8, COMMON, 0x38u32),
            (3, ISR, 1),
            (4, DEVICE, 0x100),
            (2, NOTIFY, 0x1000),
        ];
        let mut at = 0x40;
        for (i, &(ty, off, len)) in caps.iter().enumerate() {
            let cap_len = if ty == 2 { 20 } else { 16 };
            let next = if i + 1 < caps.len() { at + cap_len } else { 0 };
            c[at] = 0x09; // vendor-specific
            c[at + 1] = next as u8;
            c[at + 2] = cap_len as u8;
            c[at + 3] = ty;
            c[at + 4] = 0; // BAR0
            put32(&mut c, at + 8, off as u32);
            put32(&mut c, at + 12, len);
            if ty == 2 {
                put32(&mut c, at + 16, NOTIFY_MULTIPLIER);
            }
            at += cap_len;
        }
        let intid = crate::dtb::pci_intx_to_intid(slot as u32, 1);
        let transport = Transport::new(intid, device.features(), device.queue_sizes());
        Function {
            slot,
            config: c,
            bar0: bar_base(slot),
            transport,
            device,
        }
    }

    fn config_read(&self, reg: usize, size: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..size as usize {
            let b = match reg + i {
                0x10..=0x13 => self.bar0.to_le_bytes()[reg + i - 0x10],
                r if r < 256 => self.config[r],
                _ => 0,
            };
            v |= (b as u32) << (8 * i);
        }
        v
    }

    fn config_write(&mut self, reg: usize, size: u32, value: u32) {
        for i in 0..size as usize {
            let b = (value >> (8 * i)) as u8;
            match reg + i {
                0x04 | 0x05 | 0x3c => self.config[reg + i] = b,
                r @ 0x10..=0x13 => {
                    let mut bytes = self.bar0.to_le_bytes();
                    bytes[r - 0x10] = b;
                    // Size probing: only address bits above the size stick.
                    self.bar0 = u32::from_le_bytes(bytes) & !(BAR_SIZE - 1);
                },
                _ => {},
            }
        }
    }

    fn bar_contains(&self, addr: u64) -> bool {
        let base = self.bar0 as u64;
        self.config[4] & 2 != 0 && addr >= base && addr < base + BAR_SIZE as u64
    }

    fn bar_read(&mut self, io: &Io, off: u64, size: u32) -> u32 {
        let mask = if size >= 4 { u32::MAX } else { (1 << (8 * size)) - 1 };
        match off {
            COMMON..=0x37 => self.transport.common_read(off) & mask,
            ISR => self.transport.isr_read(io),
            o if (DEVICE..DEVICE + 0x100).contains(&o) => {
                let mut v = 0u32;
                for i in 0..size.min(4) as u64 {
                    v |= (self.device.config_read(o - DEVICE + i) as u32) << (8 * i);
                }
                v
            },
            _ => 0,
        }
    }

    fn bar_write(&mut self, io: &Io, off: u64, size: u32, value: u32) {
        match off {
            COMMON..=0x37 => {
                let was_ok = self.transport.driver_ok();
                if self.transport.common_write(io, off, size, value) {
                    self.device.reset();
                } else if !was_ok && self.transport.driver_ok() {
                    self.device.activate(io, &mut self.transport);
                }
            },
            o if (NOTIFY..NOTIFY + 0x1000).contains(&o) => {
                let q = ((o - NOTIFY) / NOTIFY_MULTIPLIER as u64) as usize;
                if q < self.transport.queues.len() {
                    self.device.notify(io, &mut self.transport, q);
                }
            },
            _ => {},
        }
    }
}

#[derive(Default)]
pub struct Bus {
    pub functions: Vec<Function>,
}

impl Bus {
    pub fn add(&mut self, slot: u8, device: Device) {
        self.functions.push(Function::new(slot, device));
    }

    pub fn slot(&self, slot: u8) -> Option<(&Transport, &Device)> {
        self.functions
            .iter()
            .find(|f| f.slot == slot)
            .map(|f| (&f.transport, &f.device))
    }

    pub fn slot_mut(&mut self, slot: u8) -> Option<(&mut Transport, &mut Device)> {
        self.functions
            .iter_mut()
            .find(|f| f.slot == slot)
            .map(|f| (&mut f.transport, &mut f.device))
    }

    /// ECAM: offset = device << 15 | function << 12 | register.
    pub fn config_read(&self, off: u64, size: u32) -> u32 {
        let (dev, func, reg) = ((off >> 15) & 31, (off >> 12) & 7, (off & 0xfff) as usize);
        match self.functions.iter().find(|f| f.slot as u64 == dev) {
            Some(f) if func == 0 => f.config_read(reg, size),
            _ => crate::bits::ones(size * 8) as u32,
        }
    }

    pub fn config_write(&mut self, off: u64, size: u32, value: u32) {
        let (dev, func, reg) = ((off >> 15) & 31, (off >> 12) & 7, (off & 0xfff) as usize);
        if func != 0 {
            return;
        }
        if let Some(f) = self.functions.iter_mut().find(|f| f.slot as u64 == dev) {
            f.config_write(reg, size, value);
        }
    }

    pub fn bar_read(&mut self, io: &Io, addr: u64, size: u32) -> u32 {
        match self.functions.iter_mut().find(|f| f.bar_contains(addr)) {
            Some(f) => {
                let off = addr - f.bar0 as u64;
                f.bar_read(io, off, size)
            },
            None => crate::bits::ones(size * 8) as u32,
        }
    }

    pub fn bar_write(&mut self, io: &Io, addr: u64, size: u32, value: u32) {
        if let Some(f) = self.functions.iter_mut().find(|f| f.bar_contains(addr)) {
            let off = addr - f.bar0 as u64;
            f.bar_write(io, off, size, value);
        }
    }

    pub fn save(&self, w: &mut W) {
        w.u32(self.functions.len() as u32);
        for f in &self.functions {
            w.u8(f.slot);
            w.u16(f.device.device_type());
            w.bytes(&f.config);
            w.u32(f.bar0);
            f.transport.save(w);
            f.device.save(w);
        }
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        let n = r.u32()? as usize;
        if n != self.functions.len() {
            return None;
        }
        for f in &mut self.functions {
            if r.u8()? != f.slot || r.u16()? != f.device.device_type() {
                return None;
            }
            f.config.copy_from_slice(r.bytes()?.get(..256)?);
            f.bar0 = r.u32()?;
            f.transport.load(r)?;
            f.device.load(r)?;
        }
        Some(())
    }
}
