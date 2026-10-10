//! Minimal 16550A (ttyS0): enough for Linux's 8250 driver, with interrupts.

use std::collections::VecDeque;

use super::{Event, Io};
use crate::board;
use crate::ser::{R, W};

#[derive(Default)]
pub struct Uart {
    pub input: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    /// THR-empty interrupt is pending until IIR is read or THR written.
    thre_pending: bool,
    line: bool,
}

impl Uart {
    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    fn irq_level(&self) -> bool {
        (self.ier & 1 != 0 && !self.input.is_empty()) || (self.ier & 2 != 0 && self.thre_pending)
    }

    fn update(&mut self, io: &Io) {
        let level = self.irq_level();
        if level != self.line {
            self.line = level;
            io.set_irq(board::SPI_UART, level);
        }
    }

    /// Bytes typed into the serial port.
    pub fn input(&mut self, io: &Io, bytes: &[u8]) {
        self.input.extend(bytes);
        self.update(io);
    }

    pub fn read(&mut self, io: &Io, reg: u64) -> u8 {
        let v = match reg {
            0 if self.dlab() => self.dll,
            0 => self.input.pop_front().unwrap_or(0),
            1 if self.dlab() => self.dlm,
            1 => self.ier,
            2 => {
                // IIR: FIFOs enabled (0xC0)
                let id = if self.ier & 1 != 0 && !self.input.is_empty() {
                    0x04
                } else if self.ier & 2 != 0 && self.thre_pending {
                    self.thre_pending = false;
                    0x02
                } else {
                    0x01
                };
                0xc0 | id
            },
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60 | (!self.input.is_empty()) as u8, // THRE | TEMT | DR
            6 => 0xb0,                                  // DCD | DSR | CTS
            7 => self.scr,
            _ => 0,
        };
        self.update(io);
        v
    }

    pub fn write(&mut self, io: &Io, reg: u64, v: u8) {
        match reg {
            0 if self.dlab() => self.dll = v,
            0 => {
                io.out.push(Event::Serial(vec![v]));
                self.thre_pending = true;
            },
            1 if self.dlab() => self.dlm = v,
            1 => {
                // Enabling the THRE interrupt raises it immediately (THR is empty).
                if v & 2 != 0 && self.ier & 2 == 0 {
                    self.thre_pending = true;
                }
                self.ier = v & 0x0f;
            },
            3 => self.lcr = v,
            4 => self.mcr = v,
            7 => self.scr = v,
            _ => {},
        }
        self.update(io);
    }

    pub fn save(&self, w: &mut W) {
        w.bytes(&self.input.iter().copied().collect::<Vec<_>>());
        for v in [self.ier, self.lcr, self.mcr, self.scr, self.dll, self.dlm] {
            w.u8(v);
        }
        w.bool(self.thre_pending);
        w.bool(self.line);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.input = r.bytes()?.iter().copied().collect();
        self.ier = r.u8()?;
        self.lcr = r.u8()?;
        self.mcr = r.u8()?;
        self.scr = r.u8()?;
        self.dll = r.u8()?;
        self.dlm = r.u8()?;
        self.thre_pending = r.bool()?;
        self.line = r.bool()?;
        Some(())
    }
}
