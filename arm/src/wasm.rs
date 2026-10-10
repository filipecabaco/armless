//! WebAssembly exports and imports: the boundary to v86's JavaScript.
//!
//! This mirrors how v86's x86 core is driven from `src/cpu.js`: one global CPU
//! inside the wasm instance, RAM in wasm linear memory (JS views it as `mem8`),
//! and MMIO accesses outside RAM/GIC/RTC forwarded to JS, which dispatches them
//! to the unchanged v86 device models:
//!
//! * UART window  -> `io.port_read8/port_write8(0x3F8 + offset)`  (uart.js, serial0)
//! * PIO window   -> `io.port_read{8,16,32}(offset)`              (virtio BARs)
//! * ECAM window  -> `pci.js` config space for (device, function, register)
//! * PCI MMIO     -> `cpu.mmap_read*/mmap_write*` handlers
//!
//! Interrupts flow back through `armless_set_irq`, which only touches a static
//! bitmap; the CPU picks changes up before the next instruction. JS device code
//! therefore never re-enters the CPU while an MMIO callback is running.
//!
//! JS must re-create its `mem8` view if `memory.buffer` changes (wasm memory can
//! grow when Rust allocates, e.g. while saving state).

use std::sync::atomic::{AtomicU32, Ordering};

use crate::board::{self, Host};
use crate::cpu::{Cpu, Exit, StopReason};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    fn mmio_read(addr: u32, size: u32) -> u32;
    fn mmio_write(addr: u32, size: u32, value: u32);
    /// Monotonic milliseconds (v86's `microtick`).
    fn microtick() -> f64;
    /// Milliseconds since the Unix epoch (`Date.now()`).
    fn wall_clock_ms() -> f64;
    /// Compile the JIT module at ptr/len into table slot `table_index`
    /// (asynchronously; JS calls armless_jit_ready). Returns 0 if unsupported.
    fn jit_compile(table_index: u32, ptr: u32, len: u32) -> u32;
}

// Native stand-ins so `cargo check --features wasm-api` type-checks this file.
#[cfg(not(target_arch = "wasm32"))]
unsafe fn mmio_read(_addr: u32, _size: u32) -> u32 {
    u32::MAX
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn mmio_write(_addr: u32, _size: u32, _value: u32) {}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn microtick() -> f64 {
    0.0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn wall_clock_ms() -> f64 {
    0.0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn jit_compile(_table_index: u32, _ptr: u32, _len: u32) -> u32 {
    0
}

const IRQ_WORDS: usize = crate::gic::NUM_IRQS / 32;
static IRQ_LEVELS: [AtomicU32; IRQ_WORDS] = [const { AtomicU32::new(0) }; IRQ_WORDS];
static IRQ_CHANGED: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
pub struct JsHost {
    /// Levels last reported to the GIC, so only changes are forwarded and
    /// lines owned by Rust devices (RTC) are never overridden.
    reported: [u32; IRQ_WORDS],
}

fn forwarded(addr: u64) -> bool {
    (board::UART_BASE..board::UART_BASE + board::UART_SIZE).contains(&addr)
        || (board::PCI_PIO_BASE..board::PCI_PIO_BASE + board::PCI_PIO_SIZE).contains(&addr)
        || (board::PCI_ECAM_BASE..board::PCI_ECAM_BASE + board::PCI_ECAM_SIZE).contains(&addr)
        || (board::PCI_MMIO_BASE..board::PCI_MMIO_BASE + board::PCI_MMIO_SIZE).contains(&addr)
}

impl Host for JsHost {
    fn mmio_read(&mut self, addr: u64, size: u32) -> Option<u64> {
        if !forwarded(addr) {
            return None;
        }
        unsafe {
            if size == 8 {
                let lo = mmio_read(addr as u32, 4) as u64;
                let hi = mmio_read(addr as u32 + 4, 4) as u64;
                Some(lo | hi << 32)
            } else {
                Some(mmio_read(addr as u32, size) as u64)
            }
        }
    }

    fn mmio_write(&mut self, addr: u64, size: u32, value: u64) -> bool {
        if !forwarded(addr) {
            return false;
        }
        unsafe {
            if size == 8 {
                mmio_write(addr as u32, 4, value as u32);
                mmio_write(addr as u32 + 4, 4, (value >> 32) as u32);
            } else {
                mmio_write(addr as u32, size, value as u32);
            }
        }
        true
    }

    fn now_ns(&mut self) -> u64 {
        (unsafe { microtick() } * 1e6) as u64
    }

    fn wall_clock_secs(&mut self) -> u64 {
        (unsafe { wall_clock_ms() } / 1000.0) as u64
    }

    fn jit_compile(&mut self, table_index: u32, wasm: &[u8]) -> bool {
        unsafe {
            jit_compile(
                table_index,
                wasm.as_ptr() as usize as u32,
                wasm.len() as u32,
            ) != 0
        }
    }

    fn sync_irqs(&mut self, set_line: &mut dyn FnMut(u32, bool)) {
        if IRQ_CHANGED.swap(0, Ordering::Relaxed) == 0 {
            return;
        }
        for (w, word) in IRQ_LEVELS.iter().enumerate().skip(1) {
            // Only SPIs (INTID >= 32) are driven from JS.
            let bits = word.load(Ordering::Relaxed);
            let changed = bits ^ self.reported[w];
            self.reported[w] = bits;
            for b in 0..32 {
                if changed & (1 << b) != 0 {
                    set_line((w * 32 + b) as u32, bits & (1 << b) != 0);
                }
            }
        }
    }
}

static mut CPU: Option<Cpu<JsHost>> = None;
static mut STATE: Vec<u8> = Vec::new();

#[allow(static_mut_refs)]
fn cpu() -> &'static mut Cpu<JsHost> {
    unsafe { CPU.as_mut().expect("armless_init not called") }
}

/// Create the machine with `memory_size` bytes of RAM. Returns 0 on success.
#[no_mangle]
pub extern "C" fn armless_init(memory_size: u32) -> i32 {
    if memory_size as u64 > board::RAM_MAX {
        return -1;
    }
    unsafe {
        CPU = Some(Cpu::new(memory_size as usize, JsHost::default()));
    }
    0
}

/// Address of guest RAM inside wasm memory (v86: `mem8 = new Uint8Array(buffer, ptr, size)`).
#[no_mangle]
pub extern "C" fn armless_mem_ptr() -> u32 {
    cpu().mem.as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn armless_mem_size() -> u32 {
    cpu().mem.len() as u32
}

/// Scratch allocation for passing kernel/initrd/cmdline/state blobs in.
#[no_mangle]
pub extern "C" fn armless_alloc(len: u32) -> u32 {
    let mut v = vec![0u8; len as usize].into_boxed_slice();
    let p = v.as_mut_ptr();
    std::mem::forget(v);
    p as u32
}

/// # Safety
/// `ptr`/`len` must come from `armless_alloc`.
#[no_mangle]
pub unsafe extern "C" fn armless_free(ptr: u32, len: u32) {
    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
        ptr as *mut u8,
        len as usize,
    )));
}

unsafe fn slice<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr as *const u8, len as usize)
    }
}

/// Load an arm64 Image (+ optional initrd) and set up the boot registers.
/// Returns 0 on success, -1 on failure.
///
/// # Safety
/// Pointers must reference `len` readable bytes in wasm memory.
#[no_mangle]
pub unsafe extern "C" fn armless_boot_linux(
    image: u32,
    image_len: u32,
    initrd: u32,
    initrd_len: u32,
    cmdline: u32,
    cmdline_len: u32,
) -> i32 {
    let image = slice(image, image_len);
    let rd = if initrd_len > 0 { Some(slice(initrd, initrd_len)) } else { None };
    let cmd = String::from_utf8_lossy(slice(cmdline, cmdline_len)).into_owned();
    match crate::boot_linux(cpu(), image, rd, &cmd) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Load a raw binary at `addr` and start there (bare-metal tests).
/// Returns 0 on success.
///
/// # Safety
/// `ptr` must reference `len` readable bytes in wasm memory.
#[no_mangle]
pub unsafe extern "C" fn armless_boot_raw(ptr: u32, len: u32, addr: u32) -> i32 {
    match crate::boot_raw(cpu(), slice(ptr, len), addr as u64) {
        Ok(_) => 0,
        Err(_) => -1,
    }
}

/// Run up to `budget` instructions. Returns:
/// 0 = budget used, 1 = idle (see `armless_wake_ms`), 2 = power off,
/// 3 = reset, 4 = halt instruction, 5 = other stop (see `armless_pc`).
#[no_mangle]
pub extern "C" fn armless_run(budget: u32) -> i32 {
    match cpu().run(budget as u64) {
        Exit::Budget => 0,
        Exit::Idle { wake_ns } => {
            unsafe { WAKE_MS = wake_ns.map_or(-1.0, |n| n as f64 / 1e6) };
            1
        },
        Exit::Stop(StopReason::PowerOff) => 2,
        Exit::Stop(StopReason::Reset) => 3,
        Exit::Stop(StopReason::Halt(_)) => 4,
        Exit::Stop(_) => 5,
    }
}

static mut WAKE_MS: f64 = -1.0;

/// After `armless_run` returned 1: microtick() time at which to resume, or -1
/// if only a device interrupt can wake the guest.
#[no_mangle]
pub extern "C" fn armless_wake_ms() -> f64 {
    unsafe { WAKE_MS }
}

/// Set a GIC interrupt input line (INTID >= 32). Safe to call from device
/// callbacks while the CPU is inside `armless_run`.
#[no_mangle]
pub extern "C" fn armless_set_irq(intid: u32, level: u32) {
    let (w, b) = ((intid / 32) as usize, intid % 32);
    if w >= IRQ_WORDS || w == 0 {
        return;
    }
    if level != 0 {
        IRQ_LEVELS[w].fetch_or(1 << b, Ordering::Relaxed);
    } else {
        IRQ_LEVELS[w].fetch_and(!(1 << b), Ordering::Relaxed);
    }
    IRQ_CHANGED.store(1, Ordering::Relaxed);
}

/// The GIC INTID wired to a PCI device's INTx pin (pin 1 = INTA).
#[no_mangle]
pub extern "C" fn armless_pci_intx_intid(slot: u32, pin: u32) -> u32 {
    crate::dtb::pci_intx_to_intid(slot, pin)
}

/// Serialise CPU state; returns its length. Read it at `armless_state_ptr()`.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_save_state() -> u32 {
    let s = cpu().save_cpu_state();
    unsafe {
        STATE = s;
        STATE.len() as u32
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_state_ptr() -> u32 {
    unsafe { STATE.as_ptr() as u32 }
}

/// Restore CPU state saved by `armless_save_state`. Returns 0 on success.
///
/// # Safety
/// `ptr` must reference `len` readable bytes in wasm memory.
#[no_mangle]
pub unsafe extern "C" fn armless_load_state(ptr: u32, len: u32) -> i32 {
    let c = cpu();
    match c.load_cpu_state(slice(ptr, len)) {
        Ok(()) => {
            // Re-base the JS-driven line bookkeeping on the restored GIC lines.
            let gic = c.machine.gic.lock();
            for w in 1..IRQ_WORDS {
                let mut bits = 0u32;
                for b in 0..32 {
                    if gic.line_level((w * 32 + b) as u32) {
                        bits |= 1 << b;
                    }
                }
                IRQ_LEVELS[w].store(bits, Ordering::Relaxed);
                c.host.reported[w] = bits;
            }
            IRQ_CHANGED.store(0, Ordering::Relaxed);
            0
        },
        Err(_) => -1,
    }
}

#[no_mangle]
pub extern "C" fn armless_pc() -> f64 {
    cpu().pc as f64
}

#[no_mangle]
pub extern "C" fn armless_insn_count() -> f64 {
    cpu().insn_count as f64
}

#[no_mangle]
pub extern "C" fn armless_reset() {
    cpu().reset();
}

// ------------------------------------------------------------------- JIT

#[allow(static_mut_refs)]
fn jit_cpu() -> &'static mut Cpu<JsHost> {
    // Set by the run loop right before it calls a block (see jit::JIT_CPU).
    unsafe { &mut *(crate::jit::JIT_CPU as *mut Cpu<JsHost>) }
}

/// Load helper for JIT blocks (fast-TLB miss path).
#[no_mangle]
pub extern "C" fn armless_jit_read(va: i64, size: i32) -> i64 {
    jit_cpu().jit_helper_read(va as u64, size as u32) as i64
}

/// Store helper for JIT blocks (fast-TLB miss path).
#[no_mangle]
pub extern "C" fn armless_jit_write(va: i64, size: i32, value: i64) {
    jit_cpu().jit_helper_write(va as u64, size as u32, value as u64)
}

/// Interpret one instruction on behalf of a JIT block.
#[no_mangle]
pub extern "C" fn armless_jit_interp(insn: i32, pc: i64) -> i32 {
    jit_cpu().jit_helper_interp(insn as u32, pc as u64)
}

/// JS finished compiling the module for `table_index`.
#[no_mangle]
pub extern "C" fn armless_jit_ready(table_index: u32) {
    cpu().jit.ready(table_index)
}

/// Turn the JIT on or off; `threshold` = executions before a block compiles.
#[no_mangle]
pub extern "C" fn armless_jit_config(enabled: u32, threshold: u32) {
    let c = cpu();
    c.jit.enabled = enabled != 0;
    if threshold > 0 {
        c.jit.threshold = threshold;
    }
    c.jit.flush_all();
}

/// Testing aid: return from armless_run after each JIT compile request, so a
/// block is compiled before its first execution.
#[no_mangle]
pub extern "C" fn armless_jit_sync(on: u32) {
    cpu().jit.sync_compile = on != 0;
}

/// Host code wrote guest RAM (DMA): drop translations of those pages.
#[no_mangle]
pub extern "C" fn armless_jit_dirty(addr: u32, len: u32) {
    cpu().jit.invalidate_range(addr as u64, len as u64)
}

/// One byte per 4 KiB page of RAM, non-zero where translated code lives
/// (lets JS skip armless_jit_dirty for pages without code).
#[no_mangle]
pub extern "C" fn armless_jit_code_pages() -> u32 {
    cpu().jit.code_page.as_ptr() as u32
}

/// [blocks compiled, blocks invalidated, block executions]
#[no_mangle]
pub extern "C" fn armless_jit_stat(which: u32) -> f64 {
    let j = &cpu().jit;
    (match which {
        0 => j.compiled,
        1 => j.invalidated,
        3 => j.flushes,
        4 => j.page_invalidations,
        5 => j.block_insns,
        6 => j.helper_interps,
        _ => j.block_runs,
    }) as f64
}
