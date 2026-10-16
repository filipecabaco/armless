//! The board's devices, all in Rust: a 16550 UART, and virtio devices on a
//! PCI (ECAM) bus — a console, a 9p filesystem and a network card with its
//! own user-mode network.
//!
//! Devices run on whichever thread touches them (a vCPU doing MMIO, or the
//! host delivering input), under one lock. What the host has to act on —
//! output, files to fetch, network connections — is queued in the
//! [`Outbox`]; the host drains it and answers through [`crate::vm::Vm`].

pub mod console;
pub mod fs;
pub mod json;
pub mod net;
pub mod netstack;
pub mod p9;
pub mod pci;
pub mod uart;
pub mod virtio;
pub mod web;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::board;
use crate::ram::Ram;
use crate::ser::{R, W};
use crate::smp::{Machine, SpinLock};

/// Something the host has to act on.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Bytes the guest wrote to the serial port (ttyS0).
    Serial(Vec<u8>),
    /// Bytes the guest wrote to the virtio console (hvc0).
    Console(Vec<u8>),
    /// The filesystem needs this blob (a file under `baseurl`); answer with
    /// `Vm::provide_blob` or `Vm::blob_failed`.
    Blob(String),
    /// The guest opened a TCP connection to `host` (a name it resolved, or
    /// the dotted address) port `port`. Answer with `tcp_accept`/`tcp_refuse`.
    TcpOpen { id: u32, host: String, port: u16 },
    /// A connection the host opened into the guest (`tcp_connect`) is up.
    TcpConnected { id: u32 },
    /// Data from the guest.
    TcpData { id: u32, data: Vec<u8> },
    /// The guest closed its sending side.
    TcpEof { id: u32 },
    /// The guest made an HTTP(S) request (ports 80 and 443 end in the machine's web relay).
    /// Answer with `http_respond`, `http_body`… and `http_end`, or `http_fail`.
    HttpRequest {
        id: u32,
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// The guest opened a WebSocket (ws:// or wss:// URL): `ws_accept` or `ws_fail`.
    WsOpen {
        id: u32,
        url: String,
        protocols: Vec<String>,
    },
    /// A message from the guest's WebSocket.
    WsMessage { id: u32, text: bool, data: Vec<u8> },
    /// The guest closed its WebSocket.
    WsClose { id: u32, code: u16 },
    /// The connection is gone (closed both ways, reset, refused, or given up on;
    /// not after the host's own `tcp_abort`). Every `TcpOpen` ends with one.
    TcpClosed { id: u32 },
}

type Notify = Box<dyn Fn() + Send + Sync>;

/// Events for the host, in order.
pub struct Outbox {
    queue: SpinLock<VecDeque<Event>>,
    notified: AtomicBool,
    notify: SpinLock<Option<Notify>>,
}

impl Default for Outbox {
    fn default() -> Self {
        Outbox {
            queue: SpinLock::new(VecDeque::new()),
            notified: AtomicBool::new(false),
            notify: SpinLock::new(None),
        }
    }
}

impl Outbox {
    /// Called (once, until the queue is drained) when an event is queued.
    pub fn set_notify(&self, f: Notify) {
        *self.notify.lock() = Some(f);
    }

    pub fn push(&self, e: Event) {
        {
            let mut q = self.queue.lock();
            // Output arrives a byte or a line at a time: merge it.
            match (q.back_mut(), &e) {
                (Some(Event::Console(a)), Event::Console(b))
                | (Some(Event::Serial(a)), Event::Serial(b)) => {
                    a.extend_from_slice(b);
                },
                (Some(Event::TcpData { id: a_id, data: a }), Event::TcpData { id, data: b })
                    if a_id == id && a.len() < 1 << 20 =>
                {
                    a.extend_from_slice(b);
                },
                _ => q.push_back(e),
            }
        }
        if !self.notified.swap(true, Ordering::SeqCst) {
            if let Some(f) = self.notify.lock().as_ref() {
                f();
            }
        }
    }

    pub fn drain(&self) -> Vec<Event> {
        self.notified.store(false, Ordering::SeqCst);
        self.queue.lock().drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }
}

/// What devices need from the machine: guest RAM (DMA), interrupt lines and
/// the outbox.
pub struct Io {
    pub machine: Arc<Machine>,
    pub mem: Ram,
    pub out: Outbox,
    /// Seconds since the Unix epoch.
    pub clock: fn() -> u64,
}

impl Io {
    pub fn new(machine: Arc<Machine>, clock: fn() -> u64) -> Io {
        Io {
            mem: machine.mem.alias(),
            machine,
            out: Outbox::default(),
            clock,
        }
    }

    pub fn now(&self) -> u64 {
        (self.clock)()
    }

    pub fn set_irq(&self, intid: u32, level: bool) {
        let mask = self.machine.gic.lock().set_line(0, intid, level);
        if mask != 0 {
            self.machine.kick_mask(mask, usize::MAX);
        }
    }

    fn in_ram(&self, addr: u64, len: usize) -> bool {
        (addr as u128) + (len as u128) <= self.mem.len() as u128
    }

    /// Copy guest memory out. False if the range is not RAM.
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        if !self.in_ram(addr, buf.len()) {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mem.as_mut_ptr().add(addr as usize),
                buf.as_mut_ptr(),
                buf.len(),
            );
        }
        true
    }

    /// Copy into guest memory (DMA), dropping JIT code of the pages written.
    pub fn write(&self, addr: u64, data: &[u8]) -> bool {
        if !self.in_ram(addr, data.len()) {
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.mem.as_mut_ptr().add(addr as usize),
                data.len(),
            );
        }
        if !data.is_empty() {
            let first = addr >> 12;
            let last = (addr + data.len() as u64 - 1) >> 12;
            for p in first..=last {
                let flag = &self.machine.code_pages[p as usize];
                if flag.load(Ordering::Relaxed) != 0 {
                    flag.store(0, Ordering::Relaxed);
                    self.machine.invalidate_code(p as u32, usize::MAX);
                }
            }
        }
        true
    }

    pub fn read_u16(&self, addr: u64) -> u16 {
        let mut b = [0; 2];
        self.read(addr, &mut b);
        u16::from_le_bytes(b)
    }
    pub fn read_u32(&self, addr: u64) -> u32 {
        let mut b = [0; 4];
        self.read(addr, &mut b);
        u32::from_le_bytes(b)
    }
    pub fn read_u64(&self, addr: u64) -> u64 {
        let mut b = [0; 8];
        self.read(addr, &mut b);
        u64::from_le_bytes(b)
    }
    /// Ring bookkeeping writes (not code).
    pub fn write_u16(&self, addr: u64, v: u16) {
        if self.in_ram(addr, 2) {
            unsafe {
                std::ptr::write_unaligned(
                    self.mem.as_mut_ptr().add(addr as usize) as *mut u16,
                    v.to_le(),
                )
            }
        }
    }
    pub fn write_u32(&self, addr: u64, v: u32) {
        if self.in_ram(addr, 4) {
            unsafe {
                std::ptr::write_unaligned(
                    self.mem.as_mut_ptr().add(addr as usize) as *mut u32,
                    v.to_le(),
                )
            }
        }
    }
}

/// Which devices a machine has.
#[derive(Clone, Debug)]
pub struct DeviceConfig {
    /// virtio console (hvc0)
    pub console: bool,
    /// 9p filesystem with this mount tag
    pub fs_tag: Option<String>,
    /// Bytes of decompressed file contents the 9p filesystem keeps on the host (the guest's
    /// page cache keeps what it uses often): a cap on what reading files costs the host
    pub fs_cache: usize,
    /// virtio-net with the user-mode network
    pub net: bool,
    pub mac: [u8; 6],
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig {
            console: true,
            fs_tag: None,
            fs_cache: fs::DEFAULT_CACHE,
            net: false,
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        }
    }
}

pub struct Devices {
    pub uart: uart::Uart,
    pub pci: pci::Bus,
    /// The VM generation ID (see `Board::seed`)
    pub vmgenid: [u8; 16],
}

/// PCI slots: fixed, so snapshots line up.
pub const SLOT_CONSOLE: u8 = 1;
pub const SLOT_FS: u8 = 2;
/// Snapshot extension: the 9p fids' owners
const EXT_P9_OWNERS: u32 = u32::from_le_bytes(*b"P9OW");
/// Snapshot extension: the VM generation ID
const EXT_VMGENID: u32 = u32::from_le_bytes(*b"VGID");
pub const SLOT_NET: u8 = 3;

impl Devices {
    pub fn new(cfg: &DeviceConfig) -> Devices {
        let mut pci = pci::Bus::default();
        if cfg.console {
            pci.add(
                SLOT_CONSOLE,
                virtio::Device::Console(console::Console::default()),
            );
        }
        if let Some(tag) = &cfg.fs_tag {
            pci.add(
                SLOT_FS,
                virtio::Device::P9(p9::P9::new(tag, {
                    let mut f = fs::Fs::empty();
                    f.set_cache_limit(cfg.fs_cache);
                    f
                })),
            );
        }
        if cfg.net {
            pci.add(SLOT_NET, virtio::Device::Net(net::Net::new(cfg.mac)));
        }
        Devices {
            uart: uart::Uart::default(),
            pci,
            vmgenid: [0; 16],
        }
    }

    pub fn mmio_read(&mut self, io: &Io, addr: u64, size: u32) -> Option<u64> {
        if (board::UART_BASE..board::UART_BASE + board::UART_SIZE).contains(&addr) {
            return Some(self.uart.read(io, (addr - board::UART_BASE) & 7) as u64);
        }
        if (board::VMGENID_BASE..board::VMGENID_BASE + board::VMGENID_SIZE).contains(&addr) {
            let off = (addr - board::VMGENID_BASE) as usize;
            let mut v = 0u64;
            for i in (0..size as usize).rev() {
                v = v << 8 | *self.vmgenid.get(off + i).unwrap_or(&0) as u64;
            }
            return Some(v);
        }
        if (board::PCI_ECAM_BASE..board::PCI_ECAM_BASE + board::PCI_ECAM_SIZE).contains(&addr) {
            return Some(self.pci.config_read(addr - board::PCI_ECAM_BASE, size) as u64);
        }
        if (board::PCI_MMIO_BASE..board::PCI_MMIO_BASE + board::PCI_MMIO_SIZE).contains(&addr) {
            if size == 8 {
                let lo = self.pci.bar_read(io, addr, 4) as u64;
                let hi = self.pci.bar_read(io, addr + 4, 4) as u64;
                return Some(lo | hi << 32);
            }
            return Some(self.pci.bar_read(io, addr, size) as u64);
        }
        if (board::PCI_PIO_BASE..board::PCI_PIO_BASE + board::PCI_PIO_SIZE).contains(&addr) {
            return Some(crate::bits::ones(size * 8));
        }
        None
    }

    pub fn mmio_write(&mut self, io: &Io, addr: u64, size: u32, value: u64) -> bool {
        if (board::VMGENID_BASE..board::VMGENID_BASE + board::VMGENID_SIZE).contains(&addr) {
            return true; // read-only
        }
        if (board::UART_BASE..board::UART_BASE + board::UART_SIZE).contains(&addr) {
            self.uart
                .write(io, (addr - board::UART_BASE) & 7, value as u8);
            return true;
        }
        if (board::PCI_ECAM_BASE..board::PCI_ECAM_BASE + board::PCI_ECAM_SIZE).contains(&addr) {
            self.pci
                .config_write(addr - board::PCI_ECAM_BASE, size, value as u32);
            return true;
        }
        if (board::PCI_MMIO_BASE..board::PCI_MMIO_BASE + board::PCI_MMIO_SIZE).contains(&addr) {
            if size == 8 {
                self.pci.bar_write(io, addr, 4, value as u32);
                self.pci.bar_write(io, addr + 4, 4, (value >> 32) as u32);
            } else {
                self.pci.bar_write(io, addr, size, value as u32);
            }
            return true;
        }
        (board::PCI_PIO_BASE..board::PCI_PIO_BASE + board::PCI_PIO_SIZE).contains(&addr)
    }

    pub fn console(&mut self) -> Option<(&mut virtio::Transport, &mut console::Console)> {
        match self.pci.slot_mut(SLOT_CONSOLE) {
            Some((t, virtio::Device::Console(c))) => Some((t, c)),
            _ => None,
        }
    }

    pub fn p9(&mut self) -> Option<(&mut virtio::Transport, &mut p9::P9)> {
        match self.pci.slot_mut(SLOT_FS) {
            Some((t, virtio::Device::P9(c))) => Some((t, c)),
            _ => None,
        }
    }

    pub fn net(&mut self) -> Option<(&mut virtio::Transport, &mut net::Net)> {
        match self.pci.slot_mut(SLOT_NET) {
            Some((t, virtio::Device::Net(c))) => Some((t, c)),
            _ => None,
        }
    }

    pub fn save(&self, w: &mut W) {
        self.uart.save(w);
        self.pci.save(w);
        // Additions to the format go last, each tagged, so older snapshots (which end here)
        // still load
        if let Some((_, virtio::Device::P9(p))) = self.pci.slot(SLOT_FS) {
            w.u32(EXT_P9_OWNERS);
            p.save_owners(w);
        }
        w.u32(EXT_VMGENID);
        w.bytes(&self.vmgenid);
    }

    pub fn load(&mut self, r: &mut R) -> Option<()> {
        self.uart.load(r)?;
        self.pci.load(r)?;
        while r.0.len() >= 4 {
            match r.u32()? {
                EXT_P9_OWNERS => self.p9()?.1.load_owners(r)?,
                EXT_VMGENID => self.vmgenid = r.bytes()?.try_into().ok()?,
                _ => return None,
            }
        }
        Some(())
    }
}
