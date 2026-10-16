//! A whole machine: RAM, CPUs, GIC, timers and the devices, with the API a
//! host drives it through (native programs and the wasm exports alike).
//!
//! * [`Board`] is what is shared and thread-safe: the devices, host input
//!   (console, serial, files, network) and the event outbox.
//! * [`Vm`] owns the CPUs too, for single-threaded hosts; multi-threaded
//!   hosts take the CPUs out and use [`save_state`] / [`StateLoader`]
//!   with all of them stopped.
//!
//! Snapshot format (version 3):
//!
//! ```text
//! "ARMLESS\x03"  u64 RAM size  u32 CPUs  u64 meta length
//! meta: CPU and machine state (state.rs), then the devices
//! RAM: (u32 page number, 4096 bytes) for each non-zero page, then u32 0xffffffff
//! ```
//!
//! RAM comes last so a loader can stream it straight into guest memory.

use std::sync::Arc;

use crate::board::{BoardHost, Host, Platform};
use crate::cpu::Cpu;
use crate::dev::{DeviceConfig, Devices, Event, Io};
use crate::ser::{R, W};
use crate::smp::{Machine, SpinLock, MAX_CPUS};

pub use crate::dev::Event as VmEvent;

const MAGIC: &[u8; 8] = b"ARMLESS\x03";
const PAGE: usize = 4096;
const END: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub struct Config {
    pub memory_size: usize,
    pub cpus: usize,
    pub devices: DeviceConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            memory_size: 512 << 20,
            cpus: 1,
            devices: DeviceConfig::default(),
        }
    }
}

/// The devices and everything a host does with them.
pub struct Board {
    pub io: Io,
    pub devices: SpinLock<Devices>,
    pub config: Config,
}

impl Board {
    pub fn new(machine: Arc<Machine>, config: Config, clock: fn() -> u64) -> Board {
        Board {
            io: Io::new(machine, clock),
            devices: SpinLock::new(Devices::new(&config.devices)),
            config,
        }
    }

    pub fn machine(&self) -> &Arc<Machine> {
        &self.io.machine
    }

    /// Everything that happened since the last call.
    pub fn events(&self) -> Vec<Event> {
        self.io.out.drain()
    }

    /// Called (from any thread) when events become available after a drain.
    pub fn set_notify(&self, f: Box<dyn Fn() + Send + Sync>) {
        self.io.out.set_notify(f);
    }

    pub fn serial_input(&self, bytes: &[u8]) {
        self.devices.lock().uart.input(&self.io, bytes);
    }

    pub fn console_input(&self, bytes: &[u8]) {
        if let Some((t, c)) = self.devices.lock().console() {
            c.input(&self.io, t, bytes);
        }
    }

    /// Input bytes still waiting for the guest to read them.
    pub fn console_pending(&self) -> usize {
        self.devices
            .lock()
            .console()
            .map_or(0, |(_, c)| c.pending())
    }

    pub fn console_resize(&self, cols: u16, rows: u16) {
        if let Some((t, c)) = self.devices.lock().console() {
            c.resize(&self.io, t, cols, rows);
        }
    }

    /// Replace the filesystem with the tree in v86's `filesystem.json`.
    pub fn load_filesystem(&self, json: &[u8]) -> Result<(), String> {
        let fs = crate::dev::fs::Fs::from_json(json)?;
        match self.devices.lock().p9() {
            Some((_, p)) => {
                p.fs = fs;
                Ok(())
            },
            None => Err("this machine has no 9p filesystem".into()),
        }
    }

    /// Contents for a blob the filesystem asked for (zstd or raw), or one to
    /// keep for later (preloading).
    pub fn provide_blob(&self, name: &str, data: Vec<u8>) -> Result<(), String> {
        let mut d = self.devices.lock();
        let Some((t, p)) = d.p9() else { return Ok(()) };
        p.fs.provide_blob(name, data)?;
        p.blob_arrived(&self.io, t, name);
        Ok(())
    }

    /// Whether a blob is already loaded.
    pub fn has_blob(&self, name: &str) -> bool {
        self.devices
            .lock()
            .p9()
            .is_some_and(|(_, p)| p.fs.has_blob(name))
    }

    pub fn blob_failed(&self, name: &str) {
        let mut d = self.devices.lock();
        if let Some((t, p)) = d.p9() {
            p.fs.blob_failed(name);
            p.blob_arrived(&self.io, t, name);
        }
    }

    /// Drop every loaded blob: what the guest reads next is fetched again.
    /// Change the 9p filesystem's budget for decompressed file contents (see
    /// `DeviceConfig::fs_cache`); what is over it is dropped now.
    pub fn set_fs_cache(&self, bytes: usize) {
        if let Some((_, p)) = self.devices.lock().p9() {
            p.fs.set_cache_limit(bytes);
        }
    }

    pub fn forget_blobs(&self) {
        if let Some((_, p)) = self.devices.lock().p9() {
            p.fs.forget_blobs();
        }
    }

    fn with_net<T>(&self, f: impl FnOnce(&mut crate::dev::netstack::Stack, &Io) -> T) -> Option<T> {
        let mut d = self.devices.lock();
        let (t, n) = d.net()?;
        let r = f(&mut n.stack, &self.io);
        n.deliver(&self.io, t);
        Some(r)
    }

    pub fn tcp_accept(&self, id: u32) {
        self.with_net(|s, _| s.accept(id));
    }
    pub fn tcp_refuse(&self, id: u32) {
        self.with_net(|s, io| s.refuse(id, &io.out));
    }
    pub fn tcp_send(&self, id: u32, data: &[u8]) {
        self.with_net(|s, _| s.send(id, data));
    }
    /// Bytes given to `tcp_send` the guest has not acknowledged yet.
    pub fn tcp_buffered(&self, id: u32) -> usize {
        self.with_net(|s, _| s.buffered(id)).unwrap_or(0)
    }
    /// Close our sending side (after what is buffered).
    pub fn tcp_close(&self, id: u32) {
        self.with_net(|s, io| s.close(id, &io.out));
    }
    pub fn tcp_abort(&self, id: u32) {
        self.with_net(|s, _| s.abort(id));
    }
    /// Connect to the guest's TCP `port`; `TcpConnected` or `TcpClosed` follows.
    pub fn tcp_connect(&self, port: u16) -> Option<u32> {
        self.with_net(|s, _| s.connect(port))
    }
    /// Timers of the network (retransmissions): call every ~100-500 ms.
    pub fn tick(&self, now_ms: u64) {
        self.with_net(|s, io| s.tick(now_ms, &io.out));
    }

    // -------------------------------------------------------- web relay

    /// The site's CA (`tls.json`): from now on the guest's HTTPS (port 443) is terminated in
    /// the machine and reaches the host as `HttpRequest`/`WsOpen` events.
    pub fn set_tls(&self, json: &[u8]) -> Result<(), String> {
        let keys = crate::dev::web::cert::TlsKeys::from_json(json)?;
        self.with_net(|s, _| s.tls = Some(keys))
            .ok_or("this machine has no network")?;
        Ok(())
    }

    /// Fresh randomness from the host: call once at start-up, and after every restore. It seeds
    /// the machine's TLS, and gives the guest a new VM generation ID, which makes Linux reseed its
    /// random generator at once: without it, every restore of a snapshot would draw the same
    /// "random" numbers (UUIDs, keys, nonces) as the machine that was saved.
    pub fn seed(&self, entropy: &[u8]) {
        self.with_net(|s, _| s.rng.seed(entropy));
        let mut id = [0u8; 16];
        for (i, b) in entropy.iter().enumerate() {
            id[i % 16] ^= b.rotate_left((i / 16) as u32);
        }
        self.devices.lock().vmgenid = id;
        // The guest configured the line as edge-triggered: a rising edge latches it
        self.io.set_irq(crate::board::SPI_VMGENID, true);
        self.io.set_irq(crate::board::SPI_VMGENID, false);
    }

    fn with_web(&self, id: u32, f: impl FnOnce(&mut crate::dev::web::Web) -> crate::dev::web::Out) {
        self.with_net(|s, io| s.with_web(id, &io.out, |w, _, _| f(w)));
    }

    /// The response to an `HttpRequest`: status and headers (the body follows with
    /// `http_body`, then `http_end`).
    pub fn http_respond(&self, id: u32, status: u16, reason: &str, headers: &[(String, String)]) {
        self.with_web(id, |w| w.respond(status, reason, headers));
    }
    pub fn http_body(&self, id: u32, data: &[u8]) {
        self.with_web(id, |w| w.body(data));
    }
    pub fn http_end(&self, id: u32) {
        self.with_web(id, |w| w.end());
    }
    /// The request (or WebSocket) could not be made: the guest gets a 502, or the connection
    /// closes if the response had started.
    pub fn http_fail(&self, id: u32, message: &str) {
        self.with_web(id, |w| w.fail(message));
    }
    /// The WebSocket for a `WsOpen` is open (with this subprotocol, or "").
    pub fn ws_accept(&self, id: u32, protocol: &str) {
        self.with_web(id, |w| w.ws_accept(protocol));
    }
    pub fn ws_send(&self, id: u32, text: bool, data: &[u8]) {
        self.with_web(id, |w| w.ws_send(text, data));
    }
    pub fn ws_close(&self, id: u32, code: u16) {
        self.with_web(id, |w| w.ws_close(code));
    }

    /// A file's contents by absolute path (or the blobs it waits on).
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, BootError> {
        let mut d = self.devices.lock();
        let Some((_, p)) = d.p9() else {
            return Err(BootError::Fail("no filesystem".into()));
        };
        let ino =
            p.fs.lookup_path(path)
                .ok_or_else(|| BootError::Fail(format!("{path}: not found")))?;
        match p.fs.read_all(ino) {
            Ok(v) => Ok(v),
            Err(crate::dev::fs::Fail::Need(b)) => Err(BootError::Need(vec![b])),
            Err(crate::dev::fs::Fail::Errno(e)) => {
                Err(BootError::Fail(format!("{path}: error {e}")))
            },
        }
    }

    /// The kernel and initrd under /boot (v86's `bzimage_initrd_from_filesystem`):
    /// the first `vmlinuz*`/`Image*` and `initramfs*`/`initrd*` by name.
    pub fn find_boot_files(&self) -> Option<(String, Option<String>)> {
        let mut d = self.devices.lock();
        let (_, p) = d.p9()?;
        let boot = p.fs.lookup_path("/boot")?;
        let names: Vec<String> =
            p.fs.list(boot)
                .ok()?
                .into_iter()
                .map(|(n, _)| String::from_utf8_lossy(&n).into_owned())
                .collect();
        let pick = |prefixes: &[&str]| {
            names
                .iter()
                .find(|n| prefixes.iter().any(|p| n.starts_with(p)) && !n.ends_with(".old"))
                .map(|n| format!("/boot/{n}"))
        };
        Some((
            pick(&["vmlinuz", "Image", "bzImage"])?,
            pick(&["initramfs", "initrd"]),
        ))
    }
}

#[derive(Debug, PartialEq)]
pub enum BootError {
    /// Fetch these blobs (`Board::provide_blob`), then try again.
    Need(Vec<String>),
    Fail(String),
}

/// A machine with its CPUs, for hosts that run them on one thread (or hand
/// them out to threads themselves).
pub struct Vm<P: Platform> {
    pub board: Arc<Board>,
    pub cpus: Vec<Cpu<BoardHost<P>>>,
}

impl<P: Platform> Vm<P> {
    /// `platform(i)` makes CPU `i`'s platform; `clock` is the wall clock in
    /// seconds (for file timestamps).
    pub fn new(
        config: Config,
        clock: fn() -> u64,
        mut platform: impl FnMut(usize) -> P,
    ) -> Result<Vm<P>, String> {
        if config.memory_size as u64 > crate::board::RAM_MAX || config.memory_size < 16 << 20 {
            return Err("memory size must be between 16 MiB and 3.5 GiB".into());
        }
        if !(1..=MAX_CPUS).contains(&config.cpus) {
            return Err(format!("1 to {MAX_CPUS} CPUs"));
        }
        let machine = Arc::new(Machine::new(config.memory_size, config.cpus));
        let board = Arc::new(Board::new(machine.clone(), config.clone(), clock));
        let cpus = (0..config.cpus)
            .map(|i| {
                Cpu::new_smp(
                    machine.clone(),
                    i,
                    BoardHost {
                        board: board.clone(),
                        platform: platform(i),
                    },
                )
            })
            .collect();
        Ok(Vm { board, cpus })
    }

    pub fn machine(&self) -> &Arc<Machine> {
        self.board.machine()
    }

    /// Boot a kernel (raw, gzip, zstd or EFI zboot) with an optional initrd.
    pub fn boot_linux(
        &mut self,
        kernel: &[u8],
        initrd: Option<&[u8]>,
        cmdline: &str,
    ) -> Result<crate::BootLayout, String> {
        let image = crate::kernel::prepare(kernel)?;
        crate::boot_linux(&mut self.cpus[0], &image, initrd, cmdline)
    }

    /// Boot the kernel and initrd found under /boot in the 9p filesystem.
    pub fn boot_from_fs(&mut self, cmdline: &str) -> Result<crate::BootLayout, BootError> {
        let (k, rd) = self.board.find_boot_files().ok_or_else(|| {
            BootError::Fail("no kernel (/boot/vmlinuz*) in the filesystem".into())
        })?;
        let kernel = self.board.read_file(&k);
        let initrd = rd.as_ref().map(|p| self.board.read_file(p));
        let mut need = Vec::new();
        for r in [Some(&kernel), initrd.as_ref()].into_iter().flatten() {
            match r {
                Err(BootError::Need(b)) => need.extend(b.iter().cloned()),
                Err(BootError::Fail(e)) => return Err(BootError::Fail(e.clone())),
                Ok(_) => {},
            }
        }
        if !need.is_empty() {
            return Err(BootError::Need(need));
        }
        let kernel = kernel.unwrap();
        let initrd = initrd.map(|r| r.unwrap());
        self.boot_linux(&kernel, initrd.as_deref(), cmdline)
            .map_err(BootError::Fail)
    }

    pub fn save_state(&mut self) -> Vec<u8> {
        let mut refs: Vec<&mut Cpu<BoardHost<P>>> = self.cpus.iter_mut().collect();
        save_state(&self.board, &mut refs)
    }

    /// Snapshot for a machine of the first `n` CPUs; see [`save_state_cpus`].
    pub fn save_state_cpus(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let mut refs: Vec<&mut Cpu<BoardHost<P>>> = self.cpus.iter_mut().collect();
        save_state_cpus(&self.board, &mut refs, n)
    }

    /// Restore a snapshot (raw or zstd-compressed).
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), String> {
        let mut refs: Vec<&mut Cpu<BoardHost<P>>> = self.cpus.iter_mut().collect();
        load_state(&self.board, &mut refs, data)
    }
}

/// Snapshot of the machine; every CPU must be stopped and in `cpus`, in order.
pub fn save_state<H: Host>(board: &Board, cpus: &mut [&mut Cpu<H>]) -> Vec<u8> {
    let n = cpus.len();
    save_state_cpus(board, cpus, n).expect("every CPU is kept")
}

/// Snapshot for a machine of the first `n` CPUs: the rest must be powered off, taken offline
/// by the guest (`echo 0 > /sys/devices/system/cpu/cpuN/online`). A machine can boot and warm
/// up on more CPUs than it ships with.
pub fn save_state_cpus<H: Host>(
    board: &Board,
    cpus: &mut [&mut Cpu<H>],
    n: usize,
) -> Result<Vec<u8>, String> {
    let mut meta = W::new();
    meta.bytes(&Cpu::save_machine_state_cpus(cpus, n)?);
    board.devices.lock().save(&mut meta);

    let mem = &board.io.mem;
    let mut out = Vec::with_capacity(meta.0.len() + mem.len() / 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(mem.len() as u64).to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out.extend_from_slice(&(meta.0.len() as u64).to_le_bytes());
    out.extend_from_slice(&meta.0);
    for (i, page) in mem.chunks(PAGE).enumerate() {
        if page.iter().any(|&b| b != 0) {
            out.extend_from_slice(&(i as u32).to_le_bytes());
            out.extend_from_slice(page);
            if page.len() < PAGE {
                out.resize(out.len() + PAGE - page.len(), 0);
            }
        }
    }
    out.extend_from_slice(&END.to_le_bytes());
    Ok(out)
}

/// Zero the RAM pages the snapshot didn't write. Reading an untouched page doesn't commit it (the
/// host maps its zero page), so only pages the guest had dirtied get written.
fn zero_unwritten(mem: &crate::ram::Ram, written: &[u64]) {
    for (page, chunk) in mem.chunks(PAGE).enumerate() {
        if written[page / 64] & (1 << (page % 64)) != 0 {
            continue;
        }
        // Word at a time: RAM is page-aligned, so the middle is all of it
        let (head, words, tail) = unsafe { chunk.align_to::<u64>() };
        if head.iter().any(|&b| b != 0) || words.iter().any(|&w| w != 0) || tail.iter().any(|&b| b != 0) {
            unsafe { std::ptr::write_bytes(mem.as_mut_ptr().add(page * PAGE), 0, chunk.len()) };
        }
    }
}

/// Restore a snapshot (raw or zstd); every CPU stopped and in `cpus`.
pub fn load_state<H: Host>(
    board: &Board,
    cpus: &mut [&mut Cpu<H>],
    data: &[u8],
) -> Result<(), String> {
    let mut loader = StateLoader::new(board);
    if crate::zstd::is_zstd(data) {
        let raw = crate::zstd::decompress(data).ok_or("corrupt zstd snapshot")?;
        loader.feed(board, &raw)?;
    } else {
        loader.feed(board, data)?;
    }
    loader.finish(board, cpus)
}

enum Stage {
    Header,
    Meta(usize),
    PageNumber,
    PageData { page: usize, at: usize },
    Done,
}

/// Loads a snapshot from a stream of (decompressed) chunks: RAM is written
/// straight into guest memory as it arrives.
pub struct StateLoader {
    stage: Stage,
    buf: Vec<u8>,
    meta: Vec<u8>,
    /// One bit per RAM page the snapshot wrote; the others are made zero in `finish`
    written: Vec<u64>,
}

impl StateLoader {
    pub fn new(_board: &Board) -> StateLoader {
        StateLoader {
            stage: Stage::Header,
            buf: Vec::new(),
            meta: Vec::new(),
            written: Vec::new(),
        }
    }

    pub fn feed(&mut self, board: &Board, mut data: &[u8]) -> Result<(), String> {
        let mem = &board.io.mem;
        while !data.is_empty() {
            match self.stage {
                Stage::Header | Stage::PageNumber | Stage::Meta(_) => {
                    let want = match self.stage {
                        Stage::Header => 28,
                        Stage::PageNumber => 4,
                        Stage::Meta(n) => n,
                        _ => unreachable!(),
                    };
                    let n = (want - self.buf.len()).min(data.len());
                    self.buf.extend_from_slice(&data[..n]);
                    data = &data[n..];
                    if self.buf.len() < want {
                        continue;
                    }
                    let b = std::mem::take(&mut self.buf);
                    self.stage = match self.stage {
                        Stage::Header => {
                            if &b[..8] != MAGIC {
                                return Err(
                                    "not an armless snapshot (or one from an older version)".into(),
                                );
                            }
                            let ram = u64::from_le_bytes(b[8..16].try_into().unwrap());
                            let ncpus = u32::from_le_bytes(b[16..20].try_into().unwrap()) as usize;
                            let meta = u64::from_le_bytes(b[20..28].try_into().unwrap()) as usize;
                            if ram != mem.len() as u64 {
                                return Err(format!(
                                    "the snapshot was saved with {} MiB of RAM, this machine has {} MiB",
                                    ram >> 20,
                                    mem.len() >> 20
                                ));
                            }
                            if ncpus != board.config.cpus {
                                return Err(format!(
                                    "the snapshot was saved with {ncpus} CPUs, this machine has {}",
                                    board.config.cpus
                                ));
                            }
                            // Pages not in the snapshot are zero, which `finish` sees to without
                            // touching the ones that already are: a fresh machine's RAM is never
                            // committed beyond what the snapshot holds
                            self.written = vec![0; mem.len().div_ceil(PAGE).div_ceil(64)];
                            Stage::Meta(meta)
                        },
                        Stage::Meta(_) => {
                            self.meta = b;
                            Stage::PageNumber
                        },
                        Stage::PageNumber => {
                            let p = u32::from_le_bytes(b[..4].try_into().unwrap());
                            if p == END {
                                Stage::Done
                            } else if (p as usize) * PAGE >= mem.len() {
                                return Err("snapshot page outside RAM".into());
                            } else {
                                self.written[p as usize / 64] |= 1 << (p % 64);
                                Stage::PageData {
                                    page: p as usize,
                                    at: 0,
                                }
                            }
                        },
                        _ => unreachable!(),
                    };
                    if let Stage::Meta(0) = self.stage {
                        self.stage = Stage::PageNumber;
                    }
                },
                Stage::PageData { page, at } => {
                    let n = (PAGE - at).min(data.len());
                    let base = page * PAGE + at;
                    let fit = n.min(mem.len().saturating_sub(base));
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            data.as_ptr(),
                            mem.as_mut_ptr().add(base),
                            fit,
                        );
                    }
                    data = &data[n..];
                    self.stage = if at + n == PAGE {
                        Stage::PageNumber
                    } else {
                        Stage::PageData { page, at: at + n }
                    };
                },
                Stage::Done => return Err("data after the end of the snapshot".into()),
            }
        }
        Ok(())
    }

    /// Apply the machine and device state once all data is in.
    pub fn finish<H: Host>(self, board: &Board, cpus: &mut [&mut Cpu<H>]) -> Result<(), String> {
        if !matches!(self.stage, Stage::Done) {
            return Err("truncated snapshot".into());
        }
        zero_unwritten(&board.io.mem, &self.written);
        let mut r = R(&self.meta);
        let cpu_state = r.bytes().ok_or("corrupt snapshot")?;
        Cpu::load_machine_state(cpus, cpu_state)?;
        let mut d = board.devices.lock();
        d.load(&mut r)
            .ok_or("the snapshot's devices don't match this machine's")?;
        for flag in board.io.machine.code_pages.iter() {
            flag.store(0, std::sync::atomic::Ordering::Relaxed);
        }
        // Requests that were waiting for files: ask for those again.
        if let Some((_, p)) = d.p9() {
            for blob in p.wanted_blobs() {
                board.io.out.push(Event::Blob(blob));
            }
        }
        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod restore_tests {
    use super::{Config, PAGE};

    #[test]
    fn restore_clears_what_the_snapshot_lacks_and_keeps_what_it_has() {
        let (mut vm, _clock) = crate::runner::new_vm(Config {
            memory_size: 16 << 20,
            cpus: 1,
            ..Default::default()
        })
        .unwrap();
        let ptr = vm.board.io.mem.as_mut_ptr();
        unsafe { *ptr.add(5 * PAGE + 7) = 0xab };
        let state = vm.save_state();

        // After the snapshot: a page it doesn't have gets dirty, one it has changes
        unsafe {
            *ptr.add(9 * PAGE + 1) = 0xcd;
            *ptr.add(5 * PAGE + 7) = 0x11;
        }
        vm.load_state(&state).unwrap();
        let mem = &vm.board.io.mem;
        assert_eq!(mem[5 * PAGE + 7], 0xab, "a page from the snapshot is restored");
        assert_eq!(mem[9 * PAGE + 1], 0, "a page the snapshot doesn't have is zero again");
        assert!(mem[12 * PAGE..13 * PAGE].iter().all(|&b| b == 0), "untouched pages stay zero");
    }
}
