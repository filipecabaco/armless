//! Flattened device tree builder and the armless board description.
//!
//! The DTB replaces SeaBIOS + ACPI + the x86 boot protocol: Linux learns RAM,
//! the GIC, the timer, PSCI, the UART, the RTC and the PCI host bridge from it.

use crate::board;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 9;

#[derive(Default)]
pub struct Fdt {
    structs: Vec<u8>,
    strings: Vec<u8>,
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

impl Fdt {
    pub fn new() -> Fdt {
        Fdt::default()
    }

    fn token(&mut self, t: u32) {
        self.structs.extend_from_slice(&t.to_be_bytes());
    }

    pub fn begin(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        pad4(&mut self.structs);
    }

    pub fn end(&mut self) {
        self.token(FDT_END_NODE);
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        // Reuse an existing identical string.
        let needle = name.as_bytes();
        let mut i = 0;
        while i < self.strings.len() {
            let end = i + self.strings[i..].iter().position(|&b| b == 0).unwrap();
            if &self.strings[i..end] == needle {
                return i as u32;
            }
            i = end + 1;
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(needle);
        self.strings.push(0);
        off
    }

    pub fn prop(&mut self, name: &str, data: &[u8]) {
        let off = self.string_offset(name);
        self.token(FDT_PROP);
        self.structs
            .extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.structs.extend_from_slice(&off.to_be_bytes());
        self.structs.extend_from_slice(data);
        pad4(&mut self.structs);
    }

    pub fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_cells(&mut self, name: &str, cells: &[u32]) {
        let mut d = Vec::with_capacity(cells.len() * 4);
        for c in cells {
            d.extend_from_slice(&c.to_be_bytes());
        }
        self.prop(name, &d);
    }

    pub fn prop_u64(&mut self, name: &str, v: u64) {
        self.prop(name, &v.to_be_bytes());
    }

    pub fn prop_str(&mut self, name: &str, s: &str) {
        let mut d = s.as_bytes().to_vec();
        d.push(0);
        self.prop(name, &d);
    }

    pub fn prop_strs(&mut self, name: &str, list: &[&str]) {
        let mut d = Vec::new();
        for s in list {
            d.extend_from_slice(s.as_bytes());
            d.push(0);
        }
        self.prop(name, &d);
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.token(FDT_END);
        let header_len = 40u32;
        let rsvmap_off = header_len;
        let rsvmap_len = 16u32;
        let struct_off = rsvmap_off + rsvmap_len;
        let strings_off = struct_off + self.structs.len() as u32;
        let total = strings_off + self.strings.len() as u32;
        let mut out = Vec::with_capacity(total as usize);
        for v in [
            0xd00d_feedu32,
            total,
            struct_off,
            strings_off,
            rsvmap_off,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structs.len() as u32,
        ] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&self.structs);
        out.extend_from_slice(&self.strings);
        out
    }
}

fn hi(v: u64) -> u32 {
    (v >> 32) as u32
}

fn lo(v: u64) -> u32 {
    v as u32
}

pub struct BoardConfig<'a> {
    pub memory_size: u64,
    pub cmdline: &'a str,
    pub initrd: Option<(u64, u64)>,
}

const PHANDLE_GIC: u32 = 1;
const PHANDLE_APB_CLK: u32 = 2;

/// GIC interrupt specifier cells: (type, number, flags).
fn spi(intid: u32) -> [u32; 3] {
    [0, intid - 32, 4] // SPI, level-high
}

/// Build the device tree for the armless virt board.
pub fn board_dtb(cfg: &BoardConfig) -> Vec<u8> {
    let mut f = Fdt::new();
    f.begin("");
    f.prop_str("compatible", "linux,dummy-virt");
    f.prop_str("model", "armless virt");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("interrupt-parent", PHANDLE_GIC);

    let uart_path = format!("/uart@{:x}", board::UART_BASE);

    f.begin("chosen");
    f.prop_str("bootargs", cfg.cmdline);
    f.prop_str("stdout-path", &uart_path);
    // Keep the BAR assignments v86's pci.js made: its virtio devices assume them.
    f.prop_u32("linux,pci-probe-only", 1);
    if let Some((start, end)) = cfg.initrd {
        f.prop_u64("linux,initrd-start", start);
        f.prop_u64("linux,initrd-end", end);
    }
    f.end();

    f.begin("aliases");
    f.prop_str("serial0", &uart_path);
    f.end();

    f.begin(&format!("memory@{:x}", board::RAM_BASE));
    f.prop_str("device_type", "memory");
    f.prop_cells(
        "reg",
        &[
            hi(board::RAM_BASE),
            lo(board::RAM_BASE),
            hi(cfg.memory_size),
            lo(cfg.memory_size),
        ],
    );
    f.end();

    f.begin("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.begin("cpu@0");
    f.prop_str("device_type", "cpu");
    f.prop_str("compatible", "arm,cortex-a53");
    f.prop_u32("reg", 0);
    f.prop_str("enable-method", "psci");
    f.end();
    f.end();

    f.begin("psci");
    f.prop_strs("compatible", &["arm,psci-1.0", "arm,psci-0.2"]);
    f.prop_str("method", "hvc");
    f.end();

    f.begin("timer");
    f.prop_str("compatible", "arm,armv8-timer");
    // secure phys, non-secure phys, virtual, hypervisor (PPI = INTID - 16)
    f.prop_cells(
        "interrupts",
        &[
            1,
            13,
            0xf04,
            1,
            board::PPI_PHYS_TIMER - 16,
            0xf04,
            1,
            board::PPI_VIRT_TIMER - 16,
            0xf04,
            1,
            board::PPI_HYP_TIMER - 16,
            0xf04,
        ],
    );
    f.prop_empty("always-on");
    f.end();

    f.begin(&format!("intc@{:x}", board::GICD_BASE));
    f.prop_str("compatible", "arm,cortex-a15-gic");
    f.prop_u32("#interrupt-cells", 3);
    f.prop_u32("#address-cells", 0);
    f.prop_empty("interrupt-controller");
    f.prop_cells(
        "reg",
        &[
            hi(board::GICD_BASE),
            lo(board::GICD_BASE),
            0,
            lo(board::GICD_SIZE),
            hi(board::GICC_BASE),
            lo(board::GICC_BASE),
            0,
            lo(board::GICC_SIZE),
        ],
    );
    f.prop_u32("phandle", PHANDLE_GIC);
    f.end();

    f.begin("apb-pclk");
    f.prop_str("compatible", "fixed-clock");
    f.prop_u32("#clock-cells", 0);
    f.prop_u32("clock-frequency", 24_000_000);
    f.prop_str("clock-output-names", "clk24mhz");
    f.prop_u32("phandle", PHANDLE_APB_CLK);
    f.end();

    f.begin(&format!("uart@{:x}", board::UART_BASE));
    f.prop_str("compatible", "ns16550a");
    f.prop_cells(
        "reg",
        &[
            hi(board::UART_BASE),
            lo(board::UART_BASE),
            0,
            lo(board::UART_SIZE),
        ],
    );
    f.prop_cells("interrupts", &spi(board::SPI_UART));
    f.prop_u32("clock-frequency", 1_843_200);
    f.prop_u32("reg-shift", 0);
    f.prop_u32("reg-io-width", 1);
    f.end();

    f.begin(&format!("pl031@{:x}", board::RTC_BASE));
    f.prop_strs("compatible", &["arm,pl031", "arm,primecell"]);
    f.prop_cells(
        "reg",
        &[
            hi(board::RTC_BASE),
            lo(board::RTC_BASE),
            0,
            lo(board::RTC_SIZE),
        ],
    );
    f.prop_cells("interrupts", &spi(board::SPI_RTC));
    f.prop_u32("clocks", PHANDLE_APB_CLK);
    f.prop_str("clock-names", "apb_pclk");
    f.end();

    f.begin(&format!("pcie@{:x}", board::PCI_ECAM_BASE));
    f.prop_str("compatible", "pci-host-ecam-generic");
    f.prop_str("device_type", "pci");
    f.prop_u32("#address-cells", 3);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("#interrupt-cells", 1);
    f.prop_u32("linux,pci-domain", 0);
    f.prop_cells("bus-range", &[0, 0]);
    f.prop_empty("dma-coherent");
    f.prop_cells(
        "reg",
        &[
            hi(board::PCI_ECAM_BASE),
            lo(board::PCI_ECAM_BASE),
            0,
            lo(board::PCI_ECAM_SIZE),
        ],
    );
    f.prop_cells(
        "ranges",
        &[
            // I/O space: PCI port 0 at the PIO window
            0x0100_0000,
            0,
            0,
            hi(board::PCI_PIO_BASE),
            lo(board::PCI_PIO_BASE),
            0,
            lo(board::PCI_PIO_SIZE),
            // 32-bit memory space, identity-mapped
            0x0200_0000,
            0,
            lo(board::PCI_MMIO_BASE),
            hi(board::PCI_MMIO_BASE),
            lo(board::PCI_MMIO_BASE),
            0,
            lo(board::PCI_MMIO_SIZE),
        ],
    );
    // INTx swizzle: slot s, pin p (1..4) -> SPI_PCI_BASE + (s + p - 1) % 4
    let mut map = Vec::new();
    for slot in 0..32u32 {
        for pin in 1..=4u32 {
            let intid = board::SPI_PCI_BASE + (slot + pin - 1) % 4;
            map.extend_from_slice(&[slot << 11, 0, 0, pin, PHANDLE_GIC]);
            map.extend_from_slice(&spi(intid));
        }
    }
    f.prop_cells("interrupt-map", &map);
    f.prop_cells("interrupt-map-mask", &[0xf800, 0, 0, 7]);
    f.end();

    f.end();
    f.finish()
}

/// The GIC INTID a PCI device's INTx pin is wired to (matches `interrupt-map`).
/// `pin` is the PCI interrupt pin register value, 1 = INTA.
pub fn pci_intx_to_intid(slot: u32, pin: u32) -> u32 {
    board::SPI_PCI_BASE + (slot + pin.max(1) - 1) % 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_well_formed() {
        let dtb = board_dtb(&BoardConfig {
            memory_size: 512 << 20,
            cmdline: "console=ttyS0",
            initrd: Some((0x1000_0000, 0x1100_0000)),
        });
        assert_eq!(&dtb[0..4], &0xd00d_feedu32.to_be_bytes());
        let total = u32::from_be_bytes(dtb[4..8].try_into().unwrap()) as usize;
        assert_eq!(total, dtb.len());
        let struct_off = u32::from_be_bytes(dtb[8..12].try_into().unwrap()) as usize;
        assert_eq!(
            u32::from_be_bytes(dtb[struct_off..struct_off + 4].try_into().unwrap()),
            1
        );
        // Must end with FDT_END in the structure block.
        let strings_off = u32::from_be_bytes(dtb[12..16].try_into().unwrap()) as usize;
        assert_eq!(
            u32::from_be_bytes(dtb[strings_off - 4..strings_off].try_into().unwrap()),
            FDT_END
        );
    }
}
