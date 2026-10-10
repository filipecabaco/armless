//! The armless "virt" board: physical memory map and the boundary to the host.
//!
//! RAM starts at physical address 0, exactly like v86's `mem8`, so every
//! JavaScript device that does DMA through `cpu.mem8` / `cpu.read32s` keeps
//! working with unchanged addresses. Everything else lives in a 256 MiB MMIO
//! hole at the top of the 32-bit space.
//!
//! ```text
//! 0x0000_0000 .. memory_size   RAM (kernel Image at 0x0020_0000)
//! 0xE000_0000 .. 0xEFFF_FFFF   PCI 32-bit MMIO window (BARs)
//! 0xF000_0000 .. +0x1_0000     GICv2 distributor
//! 0xF001_0000 .. +0x1_0000     GICv2 CPU interface
//! 0xF010_0000 .. +0x1000       16550 UART  (= x86 port 0x3F8, v86's uart.js)
//! 0xF011_0000 .. +0x1000       PL031 RTC
//! 0xF020_0000 .. +0x1_0000     PCI I/O window: offset N = x86 port N
//! 0xF100_0000 .. +0x10_0000    PCI ECAM, bus 0 (v86's pci.js config space)
//! ```

pub const RAM_BASE: u64 = 0;
pub const KERNEL_OFFSET: u64 = 0x0020_0000;
/// RAM must end below the PCI MMIO window.
pub const RAM_MAX: u64 = 0xE000_0000;

pub const PCI_MMIO_BASE: u64 = 0xE000_0000;
pub const PCI_MMIO_SIZE: u64 = 0x1000_0000;
pub const GICD_BASE: u64 = 0xF000_0000;
pub const GICD_SIZE: u64 = 0x1_0000;
pub const GICC_BASE: u64 = 0xF001_0000;
pub const GICC_SIZE: u64 = 0x1_0000;
pub const UART_BASE: u64 = 0xF010_0000;
pub const UART_SIZE: u64 = 0x1000;
/// The x86 port the UART window forwards to (COM1, v86's serial0).
pub const UART_PORT: u32 = 0x3F8;
pub const RTC_BASE: u64 = 0xF011_0000;
pub const RTC_SIZE: u64 = 0x1000;
pub const PCI_PIO_BASE: u64 = 0xF020_0000;
pub const PCI_PIO_SIZE: u64 = 0x1_0000;
pub const PCI_ECAM_BASE: u64 = 0xF100_0000;
pub const PCI_ECAM_SIZE: u64 = 0x10_0000;

/// Interrupt numbers (GIC INTIDs).
pub const PPI_HYP_TIMER: u32 = 26;
pub const PPI_VIRT_TIMER: u32 = 27;
pub const PPI_PHYS_TIMER: u32 = 30;
pub const SPI_UART: u32 = 32 + 1;
pub const SPI_RTC: u32 = 32 + 2;
/// PCI INTA..INTD of slot 0 map to these four SPIs; other slots are swizzled.
pub const SPI_PCI_BASE: u32 = 32 + 3;

/// Generic timer frequency, same as QEMU's virt board.
pub const TIMER_FREQ_HZ: u64 = 62_500_000;

/// Everything outside RAM, the GIC and the timer is owned by the host.
///
/// In the browser build this is implemented by JavaScript imports, so the
/// existing v86 devices (uart.js, pci.js, virtio*.js) handle the accesses.
/// In the native build it is implemented by [`crate::devices::NativeBus`].
pub trait Host {
    /// Read `size` bytes (1, 2, 4 or 8) from a physical MMIO address.
    /// `None` means nothing is mapped there (a bus error for the guest).
    fn mmio_read(&mut self, addr: u64, size: u32) -> Option<u64>;
    /// Write `size` bytes to a physical MMIO address. `false` means unmapped.
    fn mmio_write(&mut self, addr: u64, size: u32, value: u64) -> bool;
    /// Monotonic host time in nanoseconds; drives the generic timer.
    fn now_ns(&mut self) -> u64;
    /// Wall clock in seconds since the Unix epoch, for the RTC.
    fn wall_clock_secs(&mut self) -> u64;
    /// Report interrupt lines the host's devices changed since the last call,
    /// as (GIC INTID, level). Device code must not call into the CPU while it
    /// is running, so lines are latched by the host and collected here.
    fn sync_irqs(&mut self, _set_line: &mut dyn FnMut(u32, bool)) {}
    /// Compile a JIT block (a wasm module) into function table slot
    /// `table_index`, asynchronously; call `Jit::ready` when done. Returns
    /// false if the host cannot run generated code.
    fn jit_compile(&mut self, _table_index: u32, _wasm: &[u8]) -> bool {
        false
    }
    /// Give the host CPU to other vCPU threads for a moment (guest WFE, or
    /// a wait for other CPUs). Only called on machines with several CPUs.
    fn yield_now(&mut self) {}
}
