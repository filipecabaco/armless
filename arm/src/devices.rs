//! Native host implementation for tests and the `armless-run` CLI.
//!
//! In the browser these addresses are served by v86's JavaScript devices; here
//! a small 16550 stands in for uart.js and the PCI bus is empty.

use std::collections::VecDeque;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::board::{self, Host};

/// Minimal 16550A: enough for Linux's 8250 driver and for polled output.
#[derive(Default)]
pub struct Uart16550 {
    pub output: Vec<u8>,
    pub input: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    /// THR-empty interrupt is pending until IIR is read or THR written.
    thre_pending: bool,
}

impl Uart16550 {
    fn dlab(&self) -> bool {
        self.lcr & 0x80 != 0
    }

    pub fn irq_level(&self) -> bool {
        (self.ier & 1 != 0 && !self.input.is_empty()) || (self.ier & 2 != 0 && self.thre_pending)
    }

    pub fn read(&mut self, reg: u64) -> u8 {
        match reg {
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
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60 | (!self.input.is_empty()) as u8, // THRE | TEMT | DR
            6 => 0xb0,                                 // DCD | DSR | CTS
            7 => self.scr,
            _ => 0,
        }
    }

    pub fn write(&mut self, reg: u64, v: u8) {
        match reg {
            0 if self.dlab() => self.dll = v,
            0 => {
                self.output.push(v);
                self.thre_pending = true;
            }
            1 if self.dlab() => self.dlm = v,
            1 => {
                // Enabling the THRE interrupt raises it immediately (THR is empty).
                if v & 2 != 0 && self.ier & 2 == 0 {
                    self.thre_pending = true;
                }
                self.ier = v & 0x0f;
            }
            3 => self.lcr = v,
            4 => self.mcr = v,
            7 => self.scr = v,
            _ => {}
        }
    }
}

pub struct NativeBus {
    pub uart: Uart16550,
    start: Instant,
    /// Extra time added to the clock, so an idle guest can be fast-forwarded.
    pub skew_ns: u64,
    uart_line: bool,
}

impl Default for NativeBus {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeBus {
    pub fn new() -> NativeBus {
        NativeBus {
            uart: Uart16550::default(),
            start: Instant::now(),
            skew_ns: 0,
            uart_line: false,
        }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.uart.output)
    }
}

impl Host for NativeBus {
    fn mmio_read(&mut self, addr: u64, size: u32) -> Option<u64> {
        if (board::UART_BASE..board::UART_BASE + board::UART_SIZE).contains(&addr) {
            return Some(self.uart.read(addr - board::UART_BASE) as u64);
        }
        if (board::PCI_ECAM_BASE..board::PCI_ECAM_BASE + board::PCI_ECAM_SIZE).contains(&addr) {
            // No devices: vendor ID reads as all ones.
            return Some(crate::bits::ones(size * 8));
        }
        if (board::PCI_PIO_BASE..board::PCI_PIO_BASE + board::PCI_PIO_SIZE).contains(&addr)
            || (board::PCI_MMIO_BASE..board::PCI_MMIO_BASE + board::PCI_MMIO_SIZE).contains(&addr)
        {
            return Some(crate::bits::ones(size * 8));
        }
        None
    }

    fn mmio_write(&mut self, addr: u64, _size: u32, value: u64) -> bool {
        if (board::UART_BASE..board::UART_BASE + board::UART_SIZE).contains(&addr) {
            self.uart.write(addr - board::UART_BASE, value as u8);
            return true;
        }
        (board::PCI_ECAM_BASE..board::PCI_ECAM_BASE + board::PCI_ECAM_SIZE).contains(&addr)
            || (board::PCI_PIO_BASE..board::PCI_PIO_BASE + board::PCI_PIO_SIZE).contains(&addr)
            || (board::PCI_MMIO_BASE..board::PCI_MMIO_BASE + board::PCI_MMIO_SIZE).contains(&addr)
    }

    fn now_ns(&mut self) -> u64 {
        self.start.elapsed().as_nanos() as u64 + self.skew_ns
    }

    fn wall_clock_secs(&mut self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn sync_irqs(&mut self, set_line: &mut dyn FnMut(u32, bool)) {
        let level = self.uart.irq_level();
        if level != self.uart_line {
            self.uart_line = level;
        }
        set_line(board::SPI_UART, level);
    }
}
