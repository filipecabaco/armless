//! armless: an AArch64 core and "virt" board for the armless fork of v86.
//!
//! The crate is the ARM replacement for v86's `src/rust` x86 core. It owns the
//! CPU, RAM (at physical address 0, like v86's `mem8`), the GICv2, the generic
//! timer and a PL031 RTC. Every other device stays in v86's JavaScript and is
//! reached through the [`board::Host`] trait (in the browser: wasm imports).
//!
//! See `docs/armless.md` at the repository root for the overall design.

pub mod a64;
pub mod bits;
pub mod board;
pub mod cpu;
pub mod devices;
pub mod dtb;
pub mod gic;
pub mod mmu;
pub mod rtc;
pub mod state;
pub mod sysreg;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use board::Host;
pub use cpu::{Cpu, Exit, StopReason};

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
        }
        None => None,
    };
    let dtb = dtb::board_dtb(&dtb::BoardConfig {
        memory_size: mem_size,
        cmdline,
        initrd: initrd_range,
    });
    if dtb.len() > 2 << 20 {
        return Err("device tree too large".into());
    }

    cpu.reset();
    cpu.mem[kernel_addr as usize..kernel_addr as usize + image.len()].copy_from_slice(image);
    cpu.mem[dtb_addr as usize..dtb_addr as usize + dtb.len()].copy_from_slice(&dtb);
    if let (Some(rd), Some((s, _))) = (initrd, initrd_range) {
        cpu.mem[s as usize..s as usize + rd.len()].copy_from_slice(rd);
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
    });
    cpu.reset();
    cpu.mem[addr as usize..end as usize].copy_from_slice(bin);
    cpu.mem[dtb_addr as usize..dtb_addr as usize + dtb.len()].copy_from_slice(&dtb);
    cpu.x[0] = dtb_addr;
    cpu.pc = addr;
    Ok(BootLayout {
        kernel_addr: addr,
        kernel_end: end,
        dtb_addr,
        initrd: None,
    })
}
