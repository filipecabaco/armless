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
/// In a snapshot's CPU count: the snapshot has no pages, its RAM is an image (`split_state`).
const ON_IMAGE: u32 = 1 << 31;

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

/// A file's path and contents.
pub type FileContents = (String, Vec<u8>);

/// The devices and everything a host does with them.
pub struct Board {
    pub io: Io,
    pub devices: SpinLock<Devices>,
    pub config: Config,
    /// The last entropy from `seed`: a boot hands it to Linux as the device tree's `/chosen/rng-seed`.
    pub boot_seed: SpinLock<Vec<u8>>,
}

impl Board {
    pub fn new(machine: Arc<Machine>, config: Config, clock: fn() -> u64) -> Board {
        Board {
            io: Io::new(machine, clock),
            devices: SpinLock::new(Devices::new(&config.devices)),
            config,
            boot_seed: SpinLock::new(Vec::new()),
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
                // The budget the host set (set_fs_cache, or DeviceConfig::fs_cache) before the tree came is
                // the new tree's too: a page sets it right after the machine is made, and the tree is read
                // after that
                let limit = p.fs.cache_limit();
                p.fs = fs;
                p.fs.set_cache_limit(limit);
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

    /// The machine's filesystem, for a host that serves a program's file calls itself rather than
    /// through the 9p device (user mode: armless-user). None when the machine has no filesystem. A
    /// read of contents that haven't arrived fails with `Fail::Need`: ask for them with `want_blobs`,
    /// and `provide_blob` delivers them as for the 9p device.
    pub fn with_fs<T>(&self, f: impl FnOnce(&mut crate::dev::fs::Fs) -> T) -> Option<T> {
        let mut d = self.devices.lock();
        let (_, p) = d.p9()?;
        Some(f(&mut p.fs))
    }

    /// Ask the host for blobs a `with_fs` read waits on (`Event::Blob`), each once until it arrives.
    pub fn want_blobs(&self, names: &[String]) {
        let mut d = self.devices.lock();
        if let Some((_, p)) = d.p9() {
            p.request(&self.io, names);
        }
    }

    /// Contents for a blob that the host has already decompressed (`provide_blob` without the zstd check).
    pub fn provide_raw_blob(&self, name: &str, data: Vec<u8>) -> Result<(), String> {
        let mut d = self.devices.lock();
        let Some((t, p)) = d.p9() else { return Ok(()) };
        p.fs.provide_raw(name, data)?;
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
            // The reads that waited on it get EIO; the next read asks the host for it again
            p.fs.blob_failed(name);
            p.blob_arrived(&self.io, t, name);
            p.fs.forget_failure(name);
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

    /// Write a file into the guest's filesystem from the host (absolute guest path), making its
    /// directories: how a host hands a guest its input. Write into a directory the guest has not
    /// looked at yet: its kernel caches what it has seen (cache=loose).
    pub fn fs_write_file(&self, path: &str, data: &[u8], mode: u32) -> Result<(), String> {
        use crate::dev::fs;
        let mut d = self.devices.lock();
        let (_, p) = d.p9().ok_or("the machine has no filesystem")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let err = |e: fs::Fail| match e {
            fs::Fail::Errno(n) => format!("{path}: errno {n}"),
            fs::Fail::Need(b) => format!("{path}: needs blobs {b:?}"),
        };
        let parts: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
        let (name, dirs) = parts
            .split_last()
            .ok_or_else(|| format!("{path}: not a file path"))?;
        let mut dir = fs::ROOT;
        for c in dirs {
            dir = match p.fs.lookup(dir, c.as_bytes()) {
                Ok(i) => i,
                Err(_) => {
                    p.fs.create(dir, c.as_bytes(), fs::S_IFDIR | 0o755, 0, 0, now)
                        .map_err(err)?
                },
            };
        }
        let ino = match p.fs.lookup(dir, name.as_bytes()) {
            Ok(i) => {
                p.fs.truncate(i, 0).map_err(err)?;
                i
            },
            Err(_) => {
                p.fs.create(
                    dir,
                    name.as_bytes(),
                    fs::S_IFREG | (mode & 0o7777),
                    0,
                    0,
                    now,
                )
                .map_err(err)?
            },
        };
        p.fs.write(ino, 0, data, now).map_err(err)?;
        Ok(())
    }

    /// Make a directory in the guest's filesystem from the host, with its parents, as `mkdir -p`
    /// does (absolute guest path): a host saves the guest starting a process for it. As with
    /// `fs_write_file`, make it where the guest has not looked yet.
    pub fn fs_mkdir(&self, path: &str) -> Result<(), String> {
        use crate::dev::fs;
        let mut d = self.devices.lock();
        let (_, p) = d.p9().ok_or("the machine has no filesystem")?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut dir = fs::ROOT;
        for c in path.split('/').filter(|c| !c.is_empty()) {
            dir = match p.fs.lookup(dir, c.as_bytes()) {
                Ok(i) => i,
                Err(_) => {
                    p.fs.create(dir, c.as_bytes(), fs::S_IFDIR | 0o755, 0, 0, now)
                        .map_err(|e| match e {
                            fs::Fail::Errno(n) => format!("{path}: errno {n}"),
                            fs::Fail::Need(b) => format!("{path}: needs blobs {b:?}"),
                        })?
                },
            };
        }
        Ok(())
    }

    /// The regular files under a guest directory, with paths relative to it: how a host takes
    /// back what a guest wrote (after the guest's `sync`). None if there is no such directory.
    pub fn fs_read_tree(&self, path: &str) -> Result<Option<Vec<FileContents>>, String> {
        use crate::dev::fs;
        let mut d = self.devices.lock();
        let (_, p) = d.p9().ok_or("the machine has no filesystem")?;
        let Some(top) = p.fs.lookup_path(path) else {
            return Ok(None);
        };
        let mut out = Vec::new();
        let mut stack = vec![(top, String::new())];
        while let Some((dir, prefix)) = stack.pop() {
            let Ok(entries) = p.fs.list(dir) else {
                continue;
            };
            for (name, ino) in entries {
                if name == b"." || name == b".." {
                    continue;
                }
                let rel = format!("{prefix}{}", String::from_utf8_lossy(&name));
                let mode = p.fs.inode(ino).map(|i| i.mode).unwrap_or(0);
                match mode & fs::S_IFMT {
                    fs::S_IFDIR => stack.push((ino, rel + "/")),
                    fs::S_IFREG => match p.fs.read_all(ino) {
                        Ok(data) => out.push((rel, data)),
                        Err(_) => return Err(format!("{path}/{rel}: contents not loaded")),
                    },
                    _ => {},
                }
            }
        }
        out.sort();
        Ok(Some(out))
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
        self.with_net(|s, io| {
            if s.wants_local(port) {
                s.connect_local(port, &io.out)
            } else {
                s.connect(port)
            }
        })
    }

    // ------------------------------------------- a process with no kernel (armless-user)

    /// The streams of a process with no kernel run on this machine's network: connections to a
    /// port nobody listens on are refused at once. False if the machine has no network.
    pub fn local_enable(&self) -> bool {
        self.with_net(|s, _| s.local_enable()).is_some()
    }
    /// Changes whenever the host does something to a stream or a listener of the process.
    pub fn local_gen(&self) -> u64 {
        self.with_net(|s, _| s.local_gen()).unwrap_or(0)
    }
    /// A connection from the process (`TcpOpen` goes to the host); `None` without a network.
    pub fn local_open(&self, ip: [u8; 4], port: u16) -> Option<u32> {
        self.with_net(|s, io| s.local_open(ip, port, &io.out))
    }
    pub fn local_state(&self, id: u32) -> crate::dev::netstack::LocalState {
        self.with_net(|s, _| s.local_state(id)).unwrap_or_default()
    }
    pub fn local_read(&self, id: u32, max: usize) -> Vec<u8> {
        self.with_net(|s, _| s.local_read(id, max))
            .unwrap_or_default()
    }
    pub fn local_write(&self, id: u32, data: &[u8]) -> bool {
        self.with_net(|s, io| s.local_write(id, data, &io.out))
            .unwrap_or(false)
    }
    pub fn local_shutdown(&self, id: u32) {
        self.with_net(|s, io| s.local_shutdown(id, &io.out));
    }
    pub fn local_drop(&self, id: u32) {
        self.with_net(|s, io| s.local_drop(id, &io.out));
    }
    pub fn local_listen(&self, port: u16) {
        self.with_net(|s, _| s.local_listen(port));
    }
    pub fn local_unlisten(&self, port: u16) {
        self.with_net(|s, _| s.local_unlisten(port));
    }
    /// Whether the process listens on TCP `port` right now.
    pub fn local_listening(&self, port: u16) -> bool {
        self.with_net(|s, _| s.local_listening(port))
            .unwrap_or(false)
    }
    pub fn local_accept(&self, port: u16) -> Option<u32> {
        self.with_net(|s, _| s.local_accept(port)).flatten()
    }
    pub fn local_pending(&self, port: u16) -> usize {
        self.with_net(|s, _| s.local_pending(port)).unwrap_or(0)
    }
    /// The router's answer to a DNS query.
    pub fn local_dns(&self, q: &[u8]) -> Option<Vec<u8>> {
        self.with_net(|s, _| s.local_dns(q)).flatten()
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
        *self.boot_seed.lock() = entropy.to_vec();
        self.with_net(|s, _| {
            s.rng.seed(entropy);
            // Session tickets from before (another copy of this snapshot) stop working
            if let Some(t) = s.tls.as_mut() {
                t.forget_tickets();
            }
        });
        let mut id = [0u8; 16];
        for (i, b) in entropy.iter().enumerate() {
            id[i % 16] ^= b.rotate_left((i / 16) as u32);
        }
        self.devices.lock().vmgenid = id;
        // The guest configured the line as edge-triggered: a rising edge latches it. Not before the
        // guest has state: an edge latched before it booted reaches a driver that is not ready for
        // it (Linux's takes it as soon as it asks for the line, with KVM's GIC, and crashes).
        if self.io.machine.started.load(std::sync::atomic::Ordering::Relaxed) {
            self.io.set_irq(crate::board::SPI_VMGENID, true);
            self.io.set_irq(crate::board::SPI_VMGENID, false);
        }
    }

    /// Moves the guest's counter forward by `ns`: after a restore, the time the machine was not
    /// running. CLOCK_MONOTONIC and CLOCK_REALTIME are the counter's (the RTC is read once, at
    /// boot), so without it a restored guest's clock stays behind by the time it was asleep. Linux
    /// takes a jump of about ten minutes at once (more overflows what it multiplies a delta by):
    /// make long ones in steps, with the guest running between them.
    pub fn advance_clock(&self, ns: u64) {
        let ticks = self.io.machine.ticks_in_ns(ns);
        self.io
            .machine
            .timer_cnt_base
            .fetch_add(ticks, std::sync::atomic::Ordering::SeqCst);
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

    /// Run `f` on the file at an absolute path; the blobs it waits on are asked for as `BootError::Need`.
    fn with_file<T>(
        &self,
        path: &str,
        f: impl FnOnce(&mut crate::dev::fs::Fs, crate::dev::fs::Ino) -> crate::dev::fs::Res<T>,
    ) -> Result<T, BootError> {
        let mut d = self.devices.lock();
        let Some((_, p)) = d.p9() else {
            return Err(BootError::Fail("no filesystem".into()));
        };
        let ino =
            p.fs.lookup_path(path)
                .ok_or_else(|| BootError::Fail(format!("{path}: not found")))?;
        match f(&mut p.fs, ino) {
            Ok(v) => Ok(v),
            Err(crate::dev::fs::Fail::Need(b)) => Err(BootError::Need(b)),
            Err(crate::dev::fs::Fail::Errno(e)) => {
                Err(BootError::Fail(format!("{path}: error {e}")))
            },
        }
    }

    /// A file's contents by absolute path (or the blobs it waits on).
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>, BootError> {
        self.with_file(path, |fs, ino| fs.read_all(ino))
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
    /// A boot from the filesystem that waits for blobs (`boot_from_fs`)
    fs_boot: Option<FsBoot>,
}

/// The most of a file `boot_from_fs` has in the heap at once, as blobs; it is the heap's high-water mark.
const BOOT_WINDOW: u64 = 4 << 20;

/// A boot from the filesystem in progress: the kernel and the initrd go from their files into RAM a window at
/// a time, and the blobs of what is copied are dropped, so that the heap (which never gives memory back) does
/// not grow by their size.
struct FsBoot {
    plan: crate::LinuxPlan,
    /// What is copied, in order: the path, the address in RAM and the length of each file
    files: Vec<(String, u64, u64)>,
    /// How many bytes of those are in RAM
    done: u64,
}

impl<P: Platform> Vm<P> {
    /// `platform(i)` makes CPU `i`'s platform; `clock` is the wall clock in
    /// seconds (for file timestamps).
    pub fn new(
        config: Config,
        clock: fn() -> u64,
        platform: impl FnMut(usize) -> P,
    ) -> Result<Vm<P>, String> {
        Self::check(&config)?;
        let ram = crate::ram::Ram::zeroed(config.memory_size);
        Self::with_ram(config, ram, clock, platform)
    }

    fn check(config: &Config) -> Result<(), String> {
        if config.memory_size as u64 > crate::board::RAM_MAX || config.memory_size < 16 << 20 {
            return Err("memory size must be between 16 MiB and 3.5 GiB".into());
        }
        if !(1..=MAX_CPUS).contains(&config.cpus) {
            return Err(format!("1 to {MAX_CPUS} CPUs"));
        }
        Ok(())
    }

    /// `new`, on `ram` (`Ram::from_image`, say), which must be `config.memory_size` long.
    pub fn with_ram(
        config: Config,
        ram: crate::ram::Ram,
        clock: fn() -> u64,
        mut platform: impl FnMut(usize) -> P,
    ) -> Result<Vm<P>, String> {
        Self::check(&config)?;
        if ram.len() != config.memory_size {
            return Err(format!(
                "RAM is {} MiB, the machine {} MiB",
                ram.len() >> 20,
                config.memory_size >> 20
            ));
        }
        let machine = Arc::new(Machine::with_ram(ram, config.cpus));
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
        Ok(Vm {
            board,
            cpus,
            fs_boot: None,
        })
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
        let seed = self.board.boot_seed.lock().clone();
        crate::boot_linux_seeded(&mut self.cpus[0], &image, initrd, cmdline, &seed)
    }

    /// Boot the kernel and initrd found under /boot in the 9p filesystem. A raw `Image` goes from its blobs
    /// into RAM in windows of a few MiB (the blobs are asked for as they are needed, and dropped once copied):
    /// call again with them provided on `BootError::Need`, as many times as it asks.
    pub fn boot_from_fs(&mut self, cmdline: &str) -> Result<crate::BootLayout, BootError> {
        let r = self.fs_boot_step(cmdline);
        if !matches!(r, Err(BootError::Need(_))) {
            self.fs_boot = None;
        }
        r
    }

    fn fs_boot_step(&mut self, cmdline: &str) -> Result<crate::BootLayout, BootError> {
        let (k, rd) = self.board.find_boot_files().ok_or_else(|| {
            BootError::Fail("no kernel (/boot/vmlinuz*) in the filesystem".into())
        })?;
        if self.fs_boot.is_none() {
            let size = self
                .board
                .with_file(&k, |fs, ino| Ok(fs.inode(ino)?.size))?;
            let mut head = [0u8; 64];
            if size >= 64 {
                self.board
                    .with_file(&k, |fs, ino| fs.read_into(ino, 0, &mut head))?;
            }
            if !crate::is_raw_image(&head) {
                // Compressed: it is decompressed whole
                return self.boot_from_fs_whole(&k, rd.as_deref(), cmdline);
            }
            let rd_len = match &rd {
                Some(p) => Some(self.board.with_file(p, |fs, ino| Ok(fs.inode(ino)?.size))?),
                None => None,
            };
            let seed = self.board.boot_seed.lock().clone();
            let plan = crate::plan_linux(
                &self.cpus[0],
                &head,
                size as usize,
                rd_len.map(|l| l as usize),
                cmdline,
                &seed,
            )
            .map_err(BootError::Fail)?;
            let mut files = vec![(k.clone(), plan.layout.kernel_addr, size)];
            if let (Some(p), Some((at, _)), Some(len)) = (&rd, plan.layout.initrd, rd_len) {
                files.push((p.clone(), at, len));
            }
            self.fs_boot = Some(FsBoot {
                plan,
                files,
                done: 0,
            });
        }
        let fb = self.fs_boot.as_mut().expect("set above");
        let mut start = 0;
        for (path, addr, len) in &fb.files {
            while fb.done < start + len {
                let off = fb.done - start;
                let n = BOOT_WINDOW.min(len - off);
                let at = (addr + off) as usize;
                let mem = &self.cpus[0].mem;
                self.board.with_file(path, |fs, ino| {
                    fs.read_with(ino, off, n, |pos, d| {
                        mem.write(at + pos, d);
                    })
                })?;
                self.board.with_file(path, |fs, ino| {
                    fs.forget_file_before(ino, off + n);
                    Ok(())
                })?;
                fb.done += n;
            }
            start += len;
        }
        self.cpus[0].reset();
        fb.plan.finish(&mut self.cpus[0]).map_err(BootError::Fail)
    }

    /// The kernel and initrd read whole, for a kernel that has to be decompressed.
    fn boot_from_fs_whole(
        &mut self,
        k: &str,
        rd: Option<&str>,
        cmdline: &str,
    ) -> Result<crate::BootLayout, BootError> {
        let kernel = self.board.read_file(k);
        let initrd = rd.map(|p| self.board.read_file(p));
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
    save_state_cpus(board, cpus, n).expect("every CPU is kept, and the GIC is armless's")
}

/// Snapshot for a machine of the first `n` CPUs: the rest must be powered off, taken offline
/// by the guest (`echo 0 > /sys/devices/system/cpu/cpuN/online`). A machine can boot and warm
/// up on more CPUs than it ships with.
pub fn save_state_cpus<H: Host>(
    board: &Board,
    cpus: &mut [&mut Cpu<H>],
    n: usize,
) -> Result<Vec<u8>, String> {
    let mut saver = StateSaver::new(board, cpus, n)?;
    let mut out = Vec::with_capacity(saver.head.len() + board.io.mem.len() / 4);
    while saver.next(board, &mut out, usize::MAX) {}
    Ok(out)
}

/// A snapshot made piece by piece, with the bytes `save_state_cpus` returns: a host that writes it
/// out (or compresses it) as it comes never holds more than one piece of it.
pub struct StateSaver {
    /// The header and the machine state, which are made at once, without the contents of the files
    /// the guest wrote (`later`, in order, read from the file system as they are given out); `at`
    /// is how much of `head` was given out, `later_at` how much of `later[0]`
    head: Vec<u8>,
    at: usize,
    later: std::collections::VecDeque<crate::ser::Later>,
    later_at: usize,
    /// The next RAM page to look at
    page: usize,
    done: bool,
}

impl StateSaver {
    /// Every CPU must be stopped and in `cpus`, in order, as for [`save_state_cpus`], and must
    /// stay so until the last piece is out.
    pub fn new<H: Host>(
        board: &Board,
        cpus: &mut [&mut Cpu<H>],
        n: usize,
    ) -> Result<StateSaver, String> {
        if board.io.machine.gic_v3.load(std::sync::atomic::Ordering::Relaxed) {
            // Its interrupt controller's state is in the host's kernel (KVM's GICv3), which a
            // snapshot does not hold yet
            return Err("a machine with the hypervisor's GICv3 can't be saved yet".into());
        }
        // The header and the state, with the state's length filled in once it is known. The contents
        // of the files the guest wrote stay out of it (W::leaving_contents): they can be most of the
        // state, and are in the file system already, so `next` gives them out from there, a piece
        // at a time, and a save holds no copy of them.
        let mem = &board.io.mem;
        let mut head = W::leaving_contents();
        // Once, with room for the CPUs and the other devices: growing by doubling would leave the
        // smaller buffers in the heap, which a wasm heap does not give back
        let hint = board.devices.lock().save_size_hint(&head);
        head.0.reserve_exact(hint + (1 << 20));
        head.0.extend_from_slice(MAGIC);
        head.u64(mem.len() as u64);
        head.u32(n as u32);
        head.u64(0);
        let state = head.0.len();
        head.bytes(&Cpu::save_machine_state_cpus(cpus, n)?);
        board.devices.lock().save(&mut head);
        let later = head.later();
        let len = (head.0.len() - state + later.iter().map(|l| l.len).sum::<usize>()) as u64;
        head.0[state - 8..state].copy_from_slice(&len.to_le_bytes());
        Ok(StateSaver {
            head: head.0,
            at: 0,
            later: later.into(),
            later_at: 0,
            page: 0,
            done: false,
        })
    }

    /// The header and the state, in pieces of at most `room` bytes appended to `out`, the contents
    /// left out of `head` read where they go. False once all of it was given out.
    fn next_state(&mut self, board: &Board, out: &mut Vec<u8>, room: usize) -> bool {
        let start = out.len();
        while out.len() - start < room {
            let left = room - (out.len() - start);
            let until = self.later.front().map_or(self.head.len(), |l| l.at);
            if self.at < until {
                let n = (until - self.at).min(left);
                out.extend_from_slice(&self.head[self.at..self.at + n]);
                self.at += n;
                continue;
            }
            let Some(l) = self.later.front().copied() else {
                break;
            };
            let n = (l.len - self.later_at).min(left);
            {
                let devices = board.devices.lock();
                // The CPUs stay stopped until the snapshot is out, so the file is as it was. Were it
                // not, zeros keep the snapshot's shape (and the debug build says so).
                let d = devices
                    .saved_contents(l.ino, l.chunk)
                    .filter(|d| d.len() == l.len);
                debug_assert!(
                    d.is_some(),
                    "inode {} changed while it was being saved",
                    l.ino
                );
                match d {
                    Some(d) => out.extend_from_slice(&d[self.later_at..self.later_at + n]),
                    None => out.resize(out.len() + n, 0),
                }
            }
            self.later_at += n;
            if self.later_at == l.len {
                self.later.pop_front();
                self.later_at = 0;
            }
        }
        if self.at == self.head.len() && self.later.is_empty() && !self.head.is_empty() {
            self.head = Vec::new();
            self.at = 0;
        }
        out.len() > start
    }

    /// Appends the next piece to `out`: the header and the state, then the RAM pages, in pieces of
    /// about `max` bytes (at least a page and a number). False when the snapshot was all given out
    /// and nothing was appended.
    pub fn next(&mut self, board: &Board, out: &mut Vec<u8>, max: usize) -> bool {
        let start = out.len();
        if (self.at < self.head.len() || !self.later.is_empty()) && self.next_state(board, out, max)
        {
            return true;
        }
        let mem = &board.io.mem;
        while self.page * PAGE < mem.len() && out.len() - start + 4 + PAGE <= max {
            let from = self.page * PAGE;
            let n = PAGE.min(mem.len() - from);
            if !mem.is_zero(from, n) {
                out.extend_from_slice(&(self.page as u32).to_le_bytes());
                let at = out.len();
                out.resize(at + PAGE, 0);
                mem.read(from, &mut out[at..at + n]);
            }
            self.page += 1;
        }
        if self.page * PAGE >= mem.len() && !self.done {
            out.extend_from_slice(&END.to_le_bytes());
            self.done = true;
        }
        out.len() > start
    }
}

/// Zero the RAM pages the snapshot didn't write. Reading an untouched page doesn't commit it (the
/// host maps its zero page), so only pages the guest had dirtied get written.
fn zero_unwritten(mem: &crate::ram::Ram, written: &[u64]) {
    for (page, at) in (0..mem.len()).step_by(PAGE).enumerate() {
        if written[page / 64] & (1 << (page % 64)) != 0 {
            continue;
        }
        let n = PAGE.min(mem.len() - at);
        if !mem.is_zero(at, n) {
            mem.zero(at, n);
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

/// Splits a snapshot (decompressed, read from `data`) into its RAM and the rest: RAM goes into
/// `image`, each page at its offset and the zero ones left as holes, so that `Ram::from_image` maps
/// it; what is returned is the snapshot without its pages, marked as such: it restores only onto
/// RAM from an image, whose pages it keeps. (Restore it onto its own image: another's would be
/// another machine's memory.) Any other snapshot restored onto RAM from an image zeroes the pages
/// it lacks, as onto any RAM.
#[cfg(unix)]
pub fn split_state(data: &mut dyn std::io::Read, image: &std::fs::File) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::FileExt;
    let e = |e: std::io::Error| e.to_string();
    let mut head = [0u8; 28];
    data.read_exact(&mut head)
        .map_err(|_| "truncated snapshot")?;
    if &head[..8] != MAGIC {
        return Err("not an armless snapshot (or one from an older version)".into());
    }
    let ram = u64::from_le_bytes(head[8..16].try_into().unwrap());
    let meta = u64::from_le_bytes(head[20..28].try_into().unwrap());
    if ram > crate::board::RAM_MAX {
        return Err("corrupt snapshot".into());
    }
    let mut out = head.to_vec();
    let cpus = u32::from_le_bytes(head[16..20].try_into().unwrap()) | ON_IMAGE;
    out[16..20].copy_from_slice(&cpus.to_le_bytes());
    let at = out.len();
    out.resize(at + meta as usize, 0);
    data.read_exact(&mut out[at..])
        .map_err(|_| "truncated snapshot")?;
    image.set_len(0).map_err(e)?;
    let mut page = vec![0u8; PAGE];
    loop {
        let mut n = [0u8; 4];
        data.read_exact(&mut n).map_err(|_| "truncated snapshot")?;
        let p = u32::from_le_bytes(n);
        if p == END {
            break;
        }
        let from = p as u64 * PAGE as u64;
        if from >= ram {
            return Err("snapshot page outside RAM".into());
        }
        data.read_exact(&mut page)
            .map_err(|_| "truncated snapshot")?;
        let fit = (PAGE as u64).min(ram - from) as usize;
        image.write_all_at(&page[..fit], from).map_err(e)?;
    }
    if data.read(&mut [0u8; 1]).map_err(e)? != 0 {
        return Err("data after the end of the snapshot".into());
    }
    image.set_len(ram).map_err(e)?;
    out.extend_from_slice(&END.to_le_bytes());
    Ok(out)
}

/// Whether `data` is a snapshot `split_state` made: one without pages, for RAM from its image.
pub fn is_split_state(data: &[u8]) -> bool {
    data.len() >= 28
        && &data[..8] == MAGIC
        && u32::from_le_bytes(data[16..20].try_into().unwrap()) & ON_IMAGE != 0
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
    /// RAM was all zero before the snapshot's first page: the others need no zeroing
    fresh: bool,
}

impl StateLoader {
    pub fn new(_board: &Board) -> StateLoader {
        StateLoader {
            stage: Stage::Header,
            buf: Vec::new(),
            meta: Vec::new(),
            written: Vec::new(),
            fresh: false,
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
                            let cpus = u32::from_le_bytes(b[16..20].try_into().unwrap());
                            let on_image = cpus & ON_IMAGE != 0;
                            let ncpus = (cpus & !ON_IMAGE) as usize;
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
                            // A snapshot split from its RAM (`split_state`) restores onto that
                            // RAM's image, keeping its pages; any other zeroes the pages it lacks,
                            // the image's too (they would be another machine's memory)
                            if on_image && !mem.is_image() {
                                return Err("this snapshot's RAM is in an image: restore it onto RAM from that image".into());
                            }
                            self.fresh = mem.take_fresh() || on_image;
                            // Once, at its size: the state holds the files the guest wrote, and a
                            // buffer grown by doubling would need half as much again on top while
                            // it moves (and leave the old one in a wasm heap, which never shrinks)
                            self.buf.try_reserve_exact(meta).map_err(|_| {
                                format!("no memory for the snapshot's {} MiB of state", meta >> 20)
                            })?;
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
                    mem.write(base, &data[..fit]);
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
        // RAM that nothing wrote before the snapshot is zero where the snapshot lacks pages:
        // checking that would read (and on macOS commit) every one of them
        if !self.fresh {
            zero_unwritten(&board.io.mem, &self.written);
        }
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
        let mem = vm.board.io.mem.alias();
        mem.store(5 * PAGE + 7, 1, 0xab);
        let state = vm.save_state();

        // After the snapshot: a page it doesn't have gets dirty, one it has changes
        mem.store(9 * PAGE + 1, 1, 0xcd);
        mem.store(5 * PAGE + 7, 1, 0x11);
        vm.load_state(&state).unwrap();
        assert_eq!(
            mem.load(5 * PAGE + 7, 1),
            0xab,
            "a page from the snapshot is restored"
        );
        assert_eq!(
            mem.load(9 * PAGE + 1, 1),
            0,
            "a page the snapshot doesn't have is zero again"
        );
        assert!(mem.is_zero(12 * PAGE, PAGE), "untouched pages stay zero");
    }
}

#[cfg(all(test, unix))]
mod image_tests {
    use super::{Config, PAGE};
    use crate::ram::Ram;

    const MB16: usize = 16 << 20;

    fn config() -> Config {
        Config {
            memory_size: MB16,
            cpus: 1,
            ..Default::default()
        }
    }

    fn temp(name: &str) -> std::fs::File {
        let path = std::env::temp_dir().join(format!("armless-{name}-{}", std::process::id()));
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        f
    }

    fn on_image(image: &std::fs::File) -> crate::Vm<crate::platform::NativePlatform> {
        let clock = crate::platform::Clock::new();
        let ram = Ram::from_image(image, MB16).unwrap();
        crate::Vm::with_ram(config(), ram, crate::platform::unix_secs, move |_| {
            crate::platform::NativePlatform::new(clock.clone())
        })
        .unwrap()
    }

    /// The version a snapshot's CPU state is written in: 2 at the default frequency with armless's
    /// features, 3 with another frequency, 4 with features a guest in hardware saw.
    fn cpu_state_version(snapshot: &[u8]) -> u32 {
        // Its "ARML", not the snapshot's own "ARMLESS"
        let at = (0..snapshot.len() - 8)
            .find(|&i| &snapshot[i..i + 4] == b"ARML" && &snapshot[i + 4..i + 7] != b"ESS")
            .expect("the CPU state");
        u32::from_le_bytes(snapshot[at + 4..at + 8].try_into().unwrap())
    }

    #[test]
    fn a_snapshot_carries_its_frequency_and_the_features_its_guest_saw() {
        use std::sync::atomic::Ordering;
        let (mut a, _clock) = crate::runner::new_vm(config()).unwrap();
        assert_eq!(cpu_state_version(&a.save_state()), 2, "armless's own: older readers take it");

        a.machine().timer_freq.store(24_000_000, Ordering::SeqCst);
        let snap = a.save_state();
        assert_eq!(cpu_state_version(&snap), 3);
        let (mut b, _clock) = crate::runner::new_vm(config()).unwrap();
        b.load_state(&snap).unwrap();
        assert_eq!(b.machine().timer_freq.load(Ordering::SeqCst), 24_000_000);

        // A guest that saw FP16 (ID_AA64PFR0_EL1 with FP and AdvSIMD at 1), as under KVM on 6.8
        let saw = vec![(crate::sysreg::ID_AA64PFR0_EL1, 0x0011_0011u64)];
        *a.machine().features.lock() = saw.clone();
        let snap = a.save_state();
        assert_eq!(cpu_state_version(&snap), 4);
        let (mut c, _clock) = crate::runner::new_vm(config()).unwrap();
        let err = c.load_state(&snap).unwrap_err();
        assert!(err.contains("features armless doesn't emulate"), "{err}");
        // Where it can run in hardware it restores, and goes on seeing what it saw
        let (mut d, _clock) = crate::runner::new_vm(config()).unwrap();
        d.machine().hw_backend.store(true, Ordering::SeqCst);
        d.load_state(&snap).unwrap();
        assert_eq!(*d.machine().features.lock(), saw);
        assert_eq!(d.machine().timer_freq.load(Ordering::SeqCst), 24_000_000);
    }

    #[test]
    fn a_split_snapshot_restores_onto_its_image() {
        let (mut a, _clock) = crate::runner::new_vm(config()).unwrap();
        a.board.io.mem.store(3 * PAGE + 1, 1, 0x33);
        a.board.io.mem.store(200 * PAGE, 8, 0x0102_0304_0506_0708);
        let whole = a.save_state();

        let image = temp("split");
        let rest = super::split_state(&mut &whole[..], &image).unwrap();
        assert!(rest.len() < 64 << 10, "no pages left in it: {}", rest.len());
        assert!(super::is_split_state(&rest) && !super::is_split_state(&whole));
        assert_eq!(image.metadata().unwrap().len(), MB16 as u64);

        // Two machines on the image: the snapshot's RAM, each writing its own
        let mut b = on_image(&image);
        let mut c = on_image(&image);
        b.load_state(&rest).unwrap();
        c.load_state(&rest).unwrap();
        let (mb, mc) = (b.board.io.mem.alias(), c.board.io.mem.alias());
        assert_eq!(
            mb.load(3 * PAGE + 1, 1),
            0x33,
            "an image page kept, not zeroed"
        );
        assert_eq!(mb.load(200 * PAGE, 8), 0x0102_0304_0506_0708);
        assert!(mb.is_zero(4 * PAGE, PAGE));
        mb.store(3 * PAGE + 1, 1, 0xbb);
        assert_eq!(mc.load(3 * PAGE + 1, 1), 0x33, "b's write is b's");

        // The same snapshot whole restores onto an image too
        let mut d = on_image(&image);
        d.load_state(&whole).unwrap();
        assert_eq!(d.board.io.mem.load(200 * PAGE, 8), 0x0102_0304_0506_0708);

        // The split one needs its image
        let (mut e, _clock) = crate::runner::new_vm(config()).unwrap();
        let err = e.load_state(&rest).unwrap_err();
        assert!(err.contains("image"), "{err}");
    }

    #[test]
    fn another_snapshot_onto_an_image_doesnt_show_the_images_pages() {
        let (mut a, _clock) = crate::runner::new_vm(config()).unwrap();
        a.board.io.mem.store(3 * PAGE, 1, 0x33);
        let image = temp("other");
        super::split_state(&mut &a.save_state()[..], &image).unwrap();

        // A machine saved with page 3 zero (and page 9 not), restored onto a's image
        let (mut b, _clock) = crate::runner::new_vm(config()).unwrap();
        b.board.io.mem.store(9 * PAGE, 1, 0x99);
        let other = b.save_state();
        let mut c = on_image(&image);
        c.load_state(&other).unwrap();
        let mem = &c.board.io.mem;
        assert_eq!(
            mem.load(3 * PAGE, 1),
            0,
            "the image's page is not the snapshot's"
        );
        assert_eq!(mem.load(9 * PAGE, 1), 0x99);
        // Its own pages stay in the image, for the next machine on it
        let d = on_image(&image);
        assert_eq!(d.board.io.mem.load(3 * PAGE, 1), 0x33);
    }

    #[test]
    fn a_split_refuses_what_isnt_a_whole_snapshot() {
        let (mut a, _clock) = crate::runner::new_vm(config()).unwrap();
        a.board.io.mem.store(PAGE, 1, 1);
        let whole = a.save_state();
        let image = temp("refuse");
        assert!(super::split_state(&mut &whole[..whole.len() - 10], &image).is_err());
        let mut more = whole.clone();
        more.push(0);
        assert!(super::split_state(&mut &more[..], &image).is_err());
        assert!(super::split_state(&mut &b"not one"[..], &image).is_err());
    }

    #[test]
    fn a_machine_takes_ram_of_its_size_only() {
        let image = temp("size");
        image.set_len(MB16 as u64).unwrap();
        let clock = crate::platform::Clock::new();
        let ram = Ram::from_image(&image, MB16).unwrap();
        let bigger = Config {
            memory_size: 32 << 20,
            ..config()
        };
        assert!(
            crate::Vm::with_ram(bigger, ram, crate::platform::unix_secs, move |_| {
                crate::platform::NativePlatform::new(clock.clone())
            })
            .is_err()
        );
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod fresh_restore_tests {
    use super::{Config, PAGE};
    use crate::ram::Ram;

    fn vm() -> crate::Vm<crate::platform::NativePlatform> {
        let config = Config {
            memory_size: 16 << 20,
            cpus: 1,
            ..Default::default()
        };
        crate::runner::new_vm(config).unwrap().0
    }

    #[test]
    fn a_restore_zeroes_what_its_snapshot_lacks_whether_or_not_ram_was_fresh() {
        let mut a = vm();
        a.board.io.mem.store(3 * PAGE, 1, 0x33);
        a.board.io.mem.store(4 * PAGE, 1, 0x44);
        let with_3_and_4 = a.save_state();
        let mut b = vm();
        b.board.io.mem.store(7 * PAGE, 1, 0x77);
        let with_7 = b.save_state();

        // Into fresh RAM: nothing to zero
        let mut c = vm();
        let mem = c.board.io.mem.alias();
        c.load_state(&with_3_and_4).unwrap();
        assert_eq!(mem.load(3 * PAGE, 1), 0x33);
        assert!(mem.is_zero(7 * PAGE, PAGE));
        // Into RAM the last restore wrote: the pages this snapshot lacks are zeroed
        c.load_state(&with_7).unwrap();
        assert_eq!(mem.load(7 * PAGE, 1), 0x77);
        assert!(mem.is_zero(3 * PAGE, PAGE) && mem.is_zero(4 * PAGE, PAGE));
    }

    #[test]
    fn ram_is_fresh_until_a_cpu_runs_or_anything_writes_it() {
        let v = vm();
        assert!(v.board.io.mem.take_fresh(), "a new machine's RAM");
        assert!(!v.board.io.mem.take_fresh(), "taken once");

        let mut c = vm();
        c.cpus[0].run(1);
        assert!(!c.board.io.mem.take_fresh(), "a CPU ran");

        let writes: [fn(&Ram); 4] = [
            |m| assert!(m.write(0, &[1])),
            |m| assert!(m.zero(0, 1)),
            |m| m.store(0, 1, 1),
            |m| assert_eq!(m.cmpxchg(0, 1, 0, 1), Some(true)),
        ];
        for write in writes {
            let v = vm();
            // Through an alias, as CPUs and devices hold it
            write(&v.board.io.mem.alias());
            assert!(!v.board.io.mem.take_fresh());
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod host_fs_tests {
    use super::Config;
    use crate::dev::DeviceConfig;

    #[test]
    fn the_host_makes_directories_with_their_parents_and_files_beside_them() {
        let config = Config {
            memory_size: 16 << 20,
            cpus: 1,
            devices: DeviceConfig {
                fs_tag: Some("root".into()),
                ..Default::default()
            },
        };
        let (vm, _clock) = crate::runner::new_vm(config).unwrap();
        let b = &vm.board;
        b.load_filesystem(br#"{"version":3,"size":0,"fsroot":[]}"#)
            .unwrap();
        assert_eq!(b.fs_read_tree("/job/out").unwrap(), None);
        b.fs_mkdir("/job/out").unwrap();
        b.fs_mkdir("/job/out").unwrap(); // already there: as mkdir -p
        b.fs_mkdir("job/in/").unwrap();
        assert_eq!(b.fs_read_tree("/job/out").unwrap(), Some(vec![]));
        b.fs_write_file("/job/in/a", b"a", 0o644).unwrap();
        assert_eq!(
            b.fs_read_tree("/job").unwrap(),
            Some(vec![("in/a".to_string(), b"a".to_vec())])
        );
        // A file in the way
        assert!(b.fs_mkdir("/job/in/a/b").is_err());
    }

    #[test]
    fn the_cache_budget_a_host_sets_stays_when_the_tree_is_loaded() {
        let config = Config {
            memory_size: 16 << 20,
            cpus: 1,
            devices: DeviceConfig {
                fs_tag: Some("root".into()),
                ..Default::default()
            },
        };
        let (vm, _clock) = crate::runner::new_vm(config).unwrap();
        let b = &vm.board;
        let limit = || b.devices.lock().fs_stats().cache_limit;
        assert_eq!(limit(), crate::dev::fs::DEFAULT_CACHE);
        // The order of armless.js: the budget, then the tree
        b.set_fs_cache(64 << 20);
        b.load_filesystem(br#"{"version":3,"size":0,"fsroot":[]}"#)
            .unwrap();
        assert_eq!(limit(), 64 << 20);
        // And again: a host that loads another tree keeps it
        b.load_filesystem(br#"{"version":3,"size":0,"fsroot":[]}"#)
            .unwrap();
        assert_eq!(limit(), 64 << 20);
    }

    #[test]
    fn a_save_gives_the_files_the_guest_wrote_out_from_the_file_system() {
        let config = || Config {
            memory_size: 16 << 20,
            cpus: 1,
            devices: DeviceConfig {
                fs_tag: Some("root".into()),
                ..Default::default()
            },
        };
        let (mut vm, _clock) = crate::runner::new_vm(config()).unwrap();
        vm.board
            .load_filesystem(br#"{"version":3,"size":0,"fsroot":[]}"#)
            .unwrap();
        let big: Vec<u8> = (0..3_000_000u32)
            .map(|i| (i * 7 + i / 4096) as u8)
            .collect();
        vm.board.fs_write_file("/big", &big, 0o644).unwrap();
        vm.board.fs_write_file("/small", b"small", 0o644).unwrap();
        vm.board.io.mem.store(3 * super::PAGE, 1, 0x5a);
        let whole = vm.save_state();

        // In pieces of about 64 KiB, none of them held twice: the saver holds no copy of the file
        let mut refs: Vec<_> = vm.cpus.iter_mut().collect();
        let mut saver = super::StateSaver::new(&vm.board, &mut refs, 1).unwrap();
        assert!(
            saver.head.len() < 1 << 20,
            "the head holds the file: {}",
            saver.head.len()
        );
        let mut pieces = Vec::new();
        loop {
            let mut piece = Vec::new();
            if !saver.next(&vm.board, &mut piece, 64 << 10) {
                break;
            }
            assert!(piece.len() <= 64 << 10);
            pieces.push(piece);
        }
        assert!(pieces.len() > 3_000_000 / (64 << 10));
        // The same bytes as a save in one, but for the CPUs' state (their timers moved on): the
        // header, its length, then the rest
        let got = pieces.concat();
        assert_eq!(got.len(), whole.len());
        let cpu_len = |s: &[u8]| u64::from_le_bytes(s[27..35].try_into().unwrap()) as usize;
        assert_eq!(cpu_len(&got), cpu_len(&whole));
        let rest = 35 + cpu_len(&whole);
        assert!(got[..35] == whole[..35], "the header");
        assert!(
            got[rest..] == whole[rest..],
            "the devices, the files and the RAM"
        );

        // Restored, onto a fresh machine and onto this one (whose files go first), keeping each
        // machine's own budget for file contents
        let limit = |vm: &mut crate::Vm<_>| vm.board.devices.lock().p9().unwrap().1.fs.cache_limit();
        let (mut back, _clock) = crate::runner::new_vm(config()).unwrap();
        back.board.set_fs_cache(1 << 20);
        back.load_state(&whole).unwrap();
        assert_eq!(limit(&mut back), 1 << 20);
        vm.board.fs_write_file("/big", b"changed", 0o644).unwrap();
        vm.load_state(&whole).unwrap();
        for vm in [&mut back, &mut vm] {
            let files = vm.board.fs_read_tree("/").unwrap().unwrap();
            assert!(files.contains(&("big".to_string(), big.clone())));
            assert!(files.contains(&("small".to_string(), b"small".to_vec())));
            assert_eq!(vm.board.io.mem.load(3 * super::PAGE, 1), 0x5a);
        }
    }
}
