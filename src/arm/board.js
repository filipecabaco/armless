// armless "virt" board memory map. Must match arm/src/board.rs.

export const RAM_MAX = 0xE0000000;

export const PCI_MMIO_BASE = 0xE0000000;
export const PCI_MMIO_SIZE = 0x10000000;
export const UART_BASE = 0xF0100000;
export const UART_SIZE = 0x1000;
export const UART_PORT = 0x3F8;
export const PCI_PIO_BASE = 0xF0200000;
export const PCI_PIO_SIZE = 0x10000;
export const PCI_ECAM_BASE = 0xF1000000;
export const PCI_ECAM_SIZE = 0x100000;

export const SPI_UART = 32 + 1;

// Function table for armless.wasm: Rust's own entries stay below
// JIT_SLOT_BASE, JIT-compiled blocks use the JIT_SLOTS entries above it.
// Must match arm/src/jit/mod.rs.
export const JIT_SLOT_BASE = 1024;
export const JIT_SLOTS = 4096;
