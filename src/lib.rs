//! armless: an AArch64 virtual machine that runs Linux in the browser
//! (WebAssembly, with a JIT to wasm and one Web Worker per CPU) and natively.
//!
//! The crate owns the whole machine: CPUs, RAM (at physical address 0), the
//! GICv2, the generic timer, a PL031 RTC, a 16550 UART and virtio devices on
//! PCI — a console, a 9p filesystem fed with content-addressed blobs, and a
//! network card with a user-mode network that hands TCP streams to the host.
//! [`vm`] is the entry point; hosts drive it and answer its [`dev::Event`]s.
//!
//! See `docs/armless.md` at the repository root for the overall design.

#![cfg_attr(
    all(target_arch = "wasm32", target_feature = "atomics"),
    feature(stdarch_wasm_atomic_wait)
)]
// `unsafe` lives where nothing else will do, and says why it is sound: guest RAM (`ram`), the
// spinlock (`smp`), the decoded-page cache (`predecode`), a CPU's own pointers (`cpu`), running
// compiled code and its memory (`jit`), and the boundaries with C and JavaScript (`zstd`, `wasm`).
// Anywhere else it does not compile; each allowance below is where it may.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub(crate) mod a64;
pub(crate) mod advsimd;
pub(crate) mod bits;
pub mod board;
pub mod cpu;
pub(crate) mod crypto;
pub mod dev;
pub(crate) mod dtb;
pub(crate) mod fp;
pub(crate) mod gic;
#[cfg(target_arch = "wasm32")]
#[allow(unsafe_code)]
pub(crate) mod heap;
pub mod jit;
pub(crate) mod kernel;
pub(crate) mod mmu;
#[cfg(not(target_arch = "wasm32"))]
pub mod native;
#[cfg(not(target_arch = "wasm32"))]
pub mod platform;
pub(crate) mod predecode;
#[allow(unsafe_code)]
pub(crate) mod ram;
pub(crate) mod rtc;
#[cfg(not(target_arch = "wasm32"))]
pub mod runner;
pub(crate) mod ser;
pub(crate) mod simd;
pub(crate) mod simd_ls;
pub(crate) mod smp;
pub(crate) mod state;
pub(crate) mod sysreg;
pub mod vm;
#[allow(unsafe_code)]
pub mod zstd;

// The exports JavaScript calls: raw pointers into the wasm memory and the one VM, throughout
#[cfg(any(target_arch = "wasm32", feature = "wasm-api"))]
#[allow(unsafe_code, unsafe_op_in_unsafe_fn)]
pub mod wasm;

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static HEAP: heap::Heap = heap::Heap;

pub use board::{Host, Platform};
pub use cpu::{Cpu, Exit, StopReason};
pub use dev::Event;
pub use vm::{Board, Config, Vm};

/// Where the boot loader placed things, for logging and tests.
#[derive(Debug, Clone)]
pub struct BootLayout {
    pub kernel_addr: u64,
    pub kernel_end: u64,
    pub dtb_addr: u64,
    pub initrd: Option<(u64, u64)>,
}

const IMAGE_MAGIC: u32 = 0x644d_5241; // "ARM\x64"

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

fn align_down(v: u64, a: u64) -> u64 {
    v & !(a - 1)
}

/// Load an arm64 Linux `Image` following Documentation/arch/arm64/booting.rst:
/// kernel at a 2 MiB-aligned base + text_offset, DTB and initrd at the top of
/// RAM, x0 = DTB address, MMU off, all interrupts masked, EL1h.
pub fn boot_linux<H: Host>(
    cpu: &mut Cpu<H>,
    image: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
) -> Result<BootLayout, String> {
    boot_linux_seeded(cpu, image, initrd, cmdline, &[])
}

/// [`boot_linux`] with entropy for the guest: the device tree's `/chosen/rng-seed`, which Linux mixes in
/// (and credits, with `random.trust_bootloader`) before anything draws from its random generator.
pub fn boot_linux_seeded<H: Host>(
    cpu: &mut Cpu<H>,
    image: &[u8],
    initrd: Option<&[u8]>,
    cmdline: &str,
    rng_seed: &[u8],
) -> Result<BootLayout, String> {
    if image.len() >= 2 && image[0] == 0x1f && image[1] == 0x8b {
        return Err("kernel is gzip-compressed; decompress it to a raw Image first".into());
    }
    if image.len() < 64 {
        return Err("kernel image too small".into());
    }
    let word = |o: usize| u32::from_le_bytes(image[o..o + 4].try_into().unwrap());
    let dword = |o: usize| u64::from_le_bytes(image[o..o + 8].try_into().unwrap());
    if &image[4..8] == b"zimg" {
        return Err("EFI zboot images are not supported; extract the inner Image".into());
    }
    if word(56) != IMAGE_MAGIC {
        return Err("not an arm64 Image (missing ARM\\x64 magic at 0x38)".into());
    }
    let text_offset = dword(8);
    let mut image_size = dword(16);
    if image_size == 0 {
        // Pre-3.17 kernels: unknown size, assume the file plus generous bss.
        image_size = image.len() as u64 + (16 << 20);
    }
    let mem_size = cpu.mem.len() as u64;
    let kernel_addr = board::RAM_BASE + board::KERNEL_OFFSET + text_offset;
    let kernel_end = kernel_addr + image_size.max(image.len() as u64);
    if kernel_end > mem_size {
        return Err(format!("kernel needs {} MiB of RAM", kernel_end >> 20));
    }

    // DTB in the top 2 MiB, initrd just below it.
    let dtb_addr = align_down(mem_size - (2 << 20), 2 << 20);
    let initrd_range = match initrd {
        Some(rd) => {
            let start = align_down(dtb_addr - rd.len() as u64, 4096);
            if start < align_up(kernel_end, 2 << 20) {
                return Err("not enough RAM for kernel + initrd".into());
            }
            Some((start, start + rd.len() as u64))
        },
        None => None,
    };
    let dtb = dtb::board_dtb(&dtb::BoardConfig {
        memory_size: mem_size,
        cmdline,
        initrd: initrd_range,
        ncpus: cpu.machine.ncpus,
        rng_seed,
    });
    if dtb.len() > 2 << 20 {
        return Err("device tree too large".into());
    }

    cpu.reset();
    load(cpu, kernel_addr, image)?;
    load(cpu, dtb_addr, &dtb)?;
    if let (Some(rd), Some((s, _))) = (initrd, initrd_range) {
        load(cpu, s, rd)?;
    }
    cpu.x[0] = dtb_addr;
    cpu.pc = kernel_addr;
    Ok(BootLayout {
        kernel_addr,
        kernel_end,
        dtb_addr,
        initrd: initrd_range,
    })
}

/// Copy `data` into RAM at `addr`.
fn load<H: Host>(cpu: &Cpu<H>, addr: u64, data: &[u8]) -> Result<(), String> {
    match usize::try_from(addr) {
        Ok(a) if cpu.mem.write(a, data) => Ok(()),
        _ => Err(format!(
            "{} bytes at {addr:#x} do not fit in RAM",
            data.len()
        )),
    }
}

/// Load a raw bare-metal binary at `addr` and start executing it there.
/// x0 points at a device tree, like for Linux, so tests can find devices.
pub fn boot_raw<H: Host>(cpu: &mut Cpu<H>, bin: &[u8], addr: u64) -> Result<BootLayout, String> {
    let end = addr + bin.len() as u64;
    let mem_size = cpu.mem.len() as u64;
    if end > mem_size {
        return Err("binary does not fit in RAM".into());
    }
    let dtb_addr = align_down(mem_size - (2 << 20), 2 << 20);
    let dtb = dtb::board_dtb(&dtb::BoardConfig {
        memory_size: mem_size,
        cmdline: "",
        initrd: None,
        ncpus: cpu.machine.ncpus,
        rng_seed: &[],
    });
    cpu.reset();
    load(cpu, addr, bin)?;
    load(cpu, dtb_addr, &dtb)?;
    cpu.x[0] = dtb_addr;
    cpu.pc = addr;
    Ok(BootLayout {
        kernel_addr: addr,
        kernel_end: end,
        dtb_addr,
        initrd: None,
    })
}
