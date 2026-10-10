//! WebAssembly exports and imports: the boundary to `armless.js`.
//!
//! One machine per instance (per shared memory, with several CPUs). The JS
//! side only provides time, the JIT's code loader and a wake-up for host
//! events; everything else — devices included — runs here. Host input goes
//! in through the exports below, and what the host has to act on comes out
//! of `armless_events` as a byte stream:
//!
//! ```text
//! u8 type, u32 id, u32 port, u32 length, bytes
//! 1 serial output   2 console output   3 blob wanted (name)
//! 4 tcp open (host) 5 tcp connected    6 tcp data
//! 7 tcp eof         8 tcp closed
//! ```
//!
//! With several CPUs (`armless-smp.wasm`) every vCPU worker instantiates the
//! module on the shared memory and attaches to its CPU; the main thread's
//! instance serves host calls, and pauses the workers for snapshots.

use std::sync::atomic::Ordering;

use crate::board::{BoardHost, Platform};
use crate::cpu::{Cpu, Exit, StopReason};
use crate::dev::{DeviceConfig, Event};
use crate::vm::{BootError, Config, StateLoader, Vm};

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
extern "C" {
    /// Monotonic milliseconds.
    fn microtick() -> f64;
    /// Milliseconds since the Unix epoch (`Date.now()`).
    fn wall_clock_ms() -> f64;
    /// Compile the JIT module at ptr/len into table slot `table_index`
    /// (asynchronously; JS calls armless_jit_ready). Returns 0 if unsupported.
    fn jit_compile(table_index: u32, ptr: u32, len: u32) -> u32;
    /// Events are waiting (called once until they are drained).
    fn host_notify();
}

// Native stand-ins so `cargo check --features wasm-api` type-checks this file.
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
#[cfg(not(target_arch = "wasm32"))]
unsafe fn host_notify() {}

pub struct JsPlatform;

impl Platform for JsPlatform {
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

    fn yield_now(&mut self) {
        // vCPU workers may block: sleep ~20 µs so the vCPU we wait for can run.
        #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
        unsafe {
            static NEVER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            core::arch::wasm32::memory_atomic_wait32(NEVER.as_ptr() as *mut i32, 0, 20_000);
        }
    }
}

fn js_clock() -> u64 {
    (unsafe { wall_clock_ms() } / 1000.0) as u64
}

type JsCpu = Cpu<BoardHost<JsPlatform>>;

/// The machine. Shared by every instance on the same memory.
static mut VM: Option<Vm<JsPlatform>> = None;
static mut BUF: Vec<u8> = Vec::new();
static mut ERROR: String = String::new();
static mut LOADER: Option<(StateLoader, Option<crate::zstd::Stream>)> = None;
static mut WAKE_MS: f64 = -1.0;

std::thread_local! {
    /// The CPU this instance drives (`armless_smp_attach`); CPU 0 by default.
    static CURRENT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[allow(static_mut_refs)]
fn vm() -> &'static mut Vm<JsPlatform> {
    unsafe { VM.as_mut().expect("armless_create not called") }
}

fn cpu() -> &'static mut JsCpu {
    let i = CURRENT.with(|c| c.get());
    &mut vm().cpus[i]
}

fn set_error(e: impl Into<String>) {
    unsafe {
        ERROR = e.into();
    }
}

#[allow(static_mut_refs)]
fn out(data: Vec<u8>) -> u32 {
    unsafe {
        BUF = data;
        BUF.len() as u32
    }
}

unsafe fn slice<'a>(ptr: u32, len: u32) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(ptr as *const u8, len as usize)
    }
}

unsafe fn string(ptr: u32, len: u32) -> String {
    String::from_utf8_lossy(slice(ptr, len)).into_owned()
}

/// Device flags for `armless_create`.
pub const DEV_CONSOLE: u32 = 1;
pub const DEV_FS: u32 = 2;
pub const DEV_NET: u32 = 4;

/// Create the machine: `memory_mb` MiB of RAM, `ncpus` CPUs (more than one
/// needs armless-smp.wasm), devices per DEV_* flags; `mac_ptr` points to 6
/// bytes (0 = default MAC). Returns 0, or -1 (see `armless_error`).
///
/// # Safety
/// `mac_ptr` must be 0 or point to 6 readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_create(
    memory_mb: u32,
    ncpus: u32,
    flags: u32,
    mac_ptr: u32,
) -> i32 {
    if ncpus > 1 && !cfg!(target_feature = "atomics") {
        set_error("several CPUs need armless-smp.wasm");
        return -1;
    }
    let mut devices = DeviceConfig {
        console: flags & DEV_CONSOLE != 0,
        fs_tag: (flags & DEV_FS != 0).then(|| "host9p".to_string()),
        net: flags & DEV_NET != 0,
        ..Default::default()
    };
    if mac_ptr != 0 {
        devices.mac.copy_from_slice(slice(mac_ptr, 6));
    }
    let config = Config {
        memory_size: (memory_mb as usize) << 20,
        cpus: ncpus as usize,
        devices,
    };
    match Vm::new(config, js_clock, |_| JsPlatform) {
        Ok(v) => {
            #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
            {
                let m = v.board.machine().clone();
                m.clone().set_waker(Box::new(move |cpu| unsafe {
                    // Wake the worker waiting (Atomics.wait) on this CPU's counter.
                    core::arch::wasm32::memory_atomic_notify(
                        m.cpus[cpu].wake.as_ptr() as *mut i32,
                        u32::MAX,
                    );
                }));
            }
            v.board.set_notify(Box::new(|| unsafe { host_notify() }));
            VM = Some(v);
            0
        },
        Err(e) => {
            set_error(e);
            -1
        },
    }
}

/// Last error message: pointer (length from `armless_error_len`).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_error() -> u32 {
    unsafe { ERROR.as_ptr() as u32 }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_error_len() -> u32 {
    unsafe { ERROR.len() as u32 }
}

/// Output of the last call that returned a length.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_buf() -> u32 {
    unsafe { BUF.as_ptr() as u32 }
}

// ------------------------------------------------------------------ SMP

/// Make this instance (a vCPU worker) drive CPU `index`.
#[no_mangle]
pub extern "C" fn armless_smp_attach(index: u32) -> i32 {
    if (index as usize) < vm().cpus.len() {
        CURRENT.with(|c| c.set(index as usize));
        0
    } else {
        -1
    }
}

#[no_mangle]
pub extern "C" fn armless_smp_ncpus() -> u32 {
    vm().cpus.len() as u32
}

/// Address of CPU `index`'s wake counter: a worker whose CPU is idle waits
/// on it with Atomics.wait (read it before armless_run, wait while unchanged).
#[no_mangle]
pub extern "C" fn armless_smp_wake_addr(index: u32) -> u32 {
    vm().machine().cpus[index as usize].wake.as_ptr() as u32
}

/// How many host cores the vCPU workers can use (navigator.hardwareConcurrency).
#[no_mangle]
pub extern "C" fn armless_smp_host_cores(n: u32) {
    vm().machine().host_cores.store(n, Ordering::Relaxed);
}

/// Ask every CPU to stop (power off); workers' armless_run then returns 2.
#[no_mangle]
pub extern "C" fn armless_smp_stop() {
    vm().machine().request_stop(crate::smp::STOP_POWER_OFF);
}

/// Mark this instance's CPU idle while its worker is paused (so CPUs still
/// running don't wait for it to acknowledge TLB invalidations).
#[no_mangle]
pub extern "C" fn armless_smp_parked() {
    let i = CURRENT.with(|c| c.get());
    vm().machine().cpus[i]
        .run_state
        .store(crate::smp::RUN_IDLE, Ordering::SeqCst);
}

/// SMP statistics: 0 broadcast TLBIs, 1 DSB waits, 2 IC invalidations, 3 kicks.
#[no_mangle]
pub extern "C" fn armless_smp_stat(which: u32) -> f64 {
    vm().machine()
        .stats
        .get(which as usize)
        .map_or(0.0, |s| s.load(Ordering::Relaxed) as f64)
}

// ------------------------------------------------------------- memory

/// Scratch allocation for passing data in.
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

/// Aligned allocation that is never freed (vCPU worker stacks and TLS blocks).
#[no_mangle]
pub extern "C" fn armless_alloc_aligned(len: u32, align: u32) -> u32 {
    let layout =
        std::alloc::Layout::from_size_align(len.max(1) as usize, align.max(1) as usize).unwrap();
    unsafe { std::alloc::alloc_zeroed(layout) as u32 }
}

#[no_mangle]
pub extern "C" fn armless_mem_ptr() -> u32 {
    vm().board.io.mem.as_ptr() as u32
}

#[no_mangle]
pub extern "C" fn armless_mem_size() -> u32 {
    vm().board.io.mem.len() as u32
}

// --------------------------------------------------------------- boot

/// Boot a kernel (raw Image, gzip, zstd or EFI zboot) with an optional
/// initrd. Returns 0, or -1 (see `armless_error`).
///
/// # Safety
/// Pointers must reference `len` readable bytes in wasm memory.
#[no_mangle]
pub unsafe extern "C" fn armless_boot_linux(
    k: u32,
    k_len: u32,
    rd: u32,
    rd_len: u32,
    cmd: u32,
    cmd_len: u32,
) -> i32 {
    let initrd = (rd_len > 0).then(|| slice(rd, rd_len));
    match vm().boot_linux(slice(k, k_len), initrd, &string(cmd, cmd_len)) {
        Ok(_) => 0,
        Err(e) => {
            set_error(e);
            -1
        },
    }
}

/// Boot the kernel and initrd under /boot in the 9p filesystem. Returns 0
/// when booted, 1 when files are still being fetched (blob events were
/// raised: provide them, then call again), -1 on failure.
///
/// # Safety
/// `cmd` must reference `cmd_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_boot_from_fs(cmd: u32, cmd_len: u32) -> i32 {
    match vm().boot_from_fs(&string(cmd, cmd_len)) {
        Ok(_) => 0,
        Err(BootError::Need(blobs)) => {
            for b in blobs {
                if !vm().board.has_blob(&b) {
                    vm().board.io.out.push(Event::Blob(b));
                }
            }
            1
        },
        Err(BootError::Fail(e)) => {
            set_error(e);
            -1
        },
    }
}

/// Load a raw binary at `addr` and start there (bare-metal tests).
///
/// # Safety
/// `ptr` must reference `len` readable bytes in wasm memory.
#[no_mangle]
pub unsafe extern "C" fn armless_boot_raw(ptr: u32, len: u32, addr: u32) -> i32 {
    match crate::boot_raw(&mut vm().cpus[0], slice(ptr, len), addr as u64) {
        Ok(_) => 0,
        Err(e) => {
            set_error(e);
            -1
        },
    }
}

// ---------------------------------------------------------------- run

/// Run up to `budget` instructions on this instance's CPU. Returns:
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

/// After `armless_run` returned 1: microtick() time at which to resume, or -1
/// if only an interrupt can wake the guest.
#[no_mangle]
pub extern "C" fn armless_wake_ms() -> f64 {
    unsafe { WAKE_MS }
}

#[no_mangle]
pub extern "C" fn armless_pc() -> f64 {
    cpu().pc as f64
}

#[no_mangle]
pub extern "C" fn armless_insn_count() -> f64 {
    vm().cpus.iter().map(|c| c.insn_count).sum::<u64>() as f64
}

// ------------------------------------------------------------- events

/// Serialise and drain pending events; returns the length (see `armless_buf`).
#[no_mangle]
pub extern "C" fn armless_events() -> u32 {
    let mut b = Vec::new();
    let mut put = |ty: u8, id: u32, port: u32, data: &[u8]| {
        b.push(ty);
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&port.to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(data);
    };
    for e in vm().board.events() {
        match e {
            Event::Serial(d) => put(1, 0, 0, &d),
            Event::Console(d) => put(2, 0, 0, &d),
            Event::Blob(n) => put(3, 0, 0, n.as_bytes()),
            Event::TcpOpen { id, host, port } => put(4, id, port as u32, host.as_bytes()),
            Event::TcpConnected { id } => put(5, id, 0, &[]),
            Event::TcpData { id, data } => put(6, id, 0, &data),
            Event::TcpEof { id } => put(7, id, 0, &[]),
            Event::TcpClosed { id } => put(8, id, 0, &[]),
        }
    }
    out(b)
}

// --------------------------------------------------------- host input

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_console_input(ptr: u32, len: u32) {
    vm().board.console_input(slice(ptr, len));
}

#[no_mangle]
pub extern "C" fn armless_console_pending() -> u32 {
    vm().board.console_pending() as u32
}

#[no_mangle]
pub extern "C" fn armless_console_resize(cols: u32, rows: u32) {
    vm().board.console_resize(cols as u16, rows as u16);
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_serial_input(ptr: u32, len: u32) {
    vm().board.serial_input(slice(ptr, len));
}

/// Load v86's filesystem.json. Returns 0 or -1.
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_fs_load(ptr: u32, len: u32) -> i32 {
    match vm().board.load_filesystem(slice(ptr, len)) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e);
            -1
        },
    }
}

/// Provide a blob (zstd or raw). Returns 0 or -1.
///
/// # Safety
/// Pointers must reference readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_blob(name: u32, name_len: u32, data: u32, data_len: u32) -> i32 {
    let n = string(name, name_len);
    match vm().board.provide_blob(&n, slice(data, data_len).to_vec()) {
        Ok(()) => 0,
        Err(e) => {
            vm().board.blob_failed(&n);
            set_error(e);
            -1
        },
    }
}

/// # Safety
/// `name` must reference `name_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_blob_failed(name: u32, name_len: u32) {
    vm().board.blob_failed(&string(name, name_len));
}

/// # Safety
/// `name` must reference `name_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_has_blob(name: u32, name_len: u32) -> u32 {
    vm().board.has_blob(&string(name, name_len)) as u32
}

#[no_mangle]
pub extern "C" fn armless_tcp_accept(id: u32) {
    vm().board.tcp_accept(id);
}

#[no_mangle]
pub extern "C" fn armless_tcp_refuse(id: u32) {
    vm().board.tcp_refuse(id);
}

/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn armless_tcp_send(id: u32, ptr: u32, len: u32) {
    vm().board.tcp_send(id, slice(ptr, len));
}

#[no_mangle]
pub extern "C" fn armless_tcp_buffered(id: u32) -> u32 {
    vm().board.tcp_buffered(id) as u32
}

#[no_mangle]
pub extern "C" fn armless_tcp_close(id: u32) {
    vm().board.tcp_close(id);
}

#[no_mangle]
pub extern "C" fn armless_tcp_abort(id: u32) {
    vm().board.tcp_abort(id);
}

/// Connect to the guest's TCP `port`: returns the connection id (0 = no network).
#[no_mangle]
pub extern "C" fn armless_tcp_connect(port: u32) -> u32 {
    vm().board.tcp_connect(port as u16).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn armless_tick(now_ms: f64) {
    vm().board.tick(now_ms as u64);
}

// -------------------------------------------------------------- state

/// Snapshot of the whole machine (CPUs stopped); returns its length (see
/// `armless_buf`). Uncompressed.
#[no_mangle]
pub extern "C" fn armless_save_state() -> u32 {
    let v = vm();
    let mut cpus: Vec<&mut JsCpu> = v.cpus.iter_mut().collect();
    out(crate::vm::save_state(&v.board, &mut cpus))
}

/// Start restoring a snapshot; `compressed` = the data fed is zstd.
#[no_mangle]
pub extern "C" fn armless_state_begin(compressed: u32) {
    let loader = StateLoader::new(&vm().board);
    let stream = (compressed != 0).then(crate::zstd::Stream::new);
    unsafe {
        LOADER = Some((loader, stream));
    }
}

/// Feed snapshot bytes. Returns 0 or -1 (see `armless_error`).
///
/// # Safety
/// `ptr` must reference `len` readable bytes.
#[no_mangle]
#[allow(static_mut_refs)]
pub unsafe extern "C" fn armless_state_feed(ptr: u32, len: u32) -> i32 {
    let Some((loader, stream)) = LOADER.as_mut() else {
        return -1;
    };
    let board = &vm().board;
    let data = slice(ptr, len);
    let result = match stream {
        Some(s) => {
            let mut err = Ok(());
            let ok = s.feed(data, &mut |chunk| {
                if err.is_ok() {
                    err = loader.feed(board, chunk);
                }
            });
            if !ok {
                Err("corrupt zstd snapshot".to_string())
            } else {
                err
            }
        },
        None => loader.feed(board, data),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            set_error(e);
            LOADER = None;
            -1
        },
    }
}

/// Apply the snapshot (all CPUs stopped). Returns 0 or -1.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn armless_state_finish() -> i32 {
    let Some((loader, _)) = (unsafe { LOADER.take() }) else {
        return -1;
    };
    let v = vm();
    let mut cpus: Vec<&mut JsCpu> = v.cpus.iter_mut().collect();
    match loader.finish(&v.board, &mut cpus) {
        Ok(()) => 0,
        Err(e) => {
            set_error(e);
            -1
        },
    }
}

// ------------------------------------------------------------------- JIT

/// Load helper for JIT blocks (fast-TLB miss path).
#[no_mangle]
pub extern "C" fn armless_jit_read(va: i64, size: i32) -> i64 {
    cpu().jit_helper_read(va as u64, size as u32) as i64
}

/// Store helper for JIT blocks (fast-TLB miss path).
#[no_mangle]
pub extern "C" fn armless_jit_write(va: i64, size: i32, value: i64) {
    cpu().jit_helper_write(va as u64, size as u32, value as u64)
}

/// Interpret one instruction on behalf of a JIT block.
#[no_mangle]
pub extern "C" fn armless_jit_interp(insn: i32, pc: i64) -> i32 {
    cpu().jit_helper_interp(insn as u32, pc as u64)
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

/// Testing aid: return from armless_run after each JIT compile request.
#[no_mangle]
pub extern "C" fn armless_jit_sync(on: u32) {
    cpu().jit.sync_compile = on != 0;
}

/// [blocks compiled, blocks invalidated, block executions, ...]
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
