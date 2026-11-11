//! User mode for armless: a Linux program on an armless CPU with no guest kernel. The CPU runs it
//! at EL0 with the MMU off and no vector table, so every exception it takes stops the CPU
//! (`StopReason::UnhandledException`) and is answered here: system calls by a Linux personality,
//! faults by stopping the process.
//!
//! What it gives a program: static or dynamically linked executables (the dynamic loader comes
//! from the filesystem, as execve has it), the machine's filesystem (armless's, the one the 9p
//! device serves a Linux guest), a terminal (termios, the window size, a line discipline), pipes,
//! threads (clone, futexes), epoll, poll and select, clocks and sleeps. Blocking calls park their
//! thread and run again when something changes, so a snapshot can be taken whatever it waits on.
//!
//! Not yet: page tables (VA = PA, so no PROT_NONE, no nil-pointer faults, no fork), sockets,
//! signal delivery (handlers are recorded, signals dropped), and more than one vCPU (threads take
//! turns on one CPU).

#![forbid(unsafe_code)]

mod elf;
mod files;
mod ser;
mod sys;
mod tty;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use armless::cpu::{Cpu, Exit, StopReason};
use armless::dev::fs::{Fs, Ino, ROOT};
use armless::{Board, Host, Ram};

use files::Files;
use ser::{R, W};
use tty::Tty;

pub(crate) const PAGE: u64 = 4096;
/// Instructions a guest thread runs before the next one gets the CPU.
const SLICE: u64 = 1 << 20;
const STACK_SIZE: u64 = 8 << 20;
const BRK_SIZE: u64 = 64 << 20;
/// Where a position-independent executable is loaded
const PIE_BASE: u64 = 0x10_0000;

// ---------------------------------------------------------------- guest threads

#[derive(Clone)]
pub(crate) struct Ctx {
    pub x: [u64; 31],
    pub sp: u64,
    pub pc: u64,
    pub nzcv: [bool; 4],
    pub v: [u128; 32],
    pub fpcr: u64,
    pub fpsr: u64,
    pub tpidr: u64,
}

impl Ctx {
    fn save<H: Host>(cpu: &Cpu<H>) -> Ctx {
        let mut x = [0; 31];
        x.copy_from_slice(&cpu.x[..31]);
        Ctx {
            x,
            sp: cpu.sp_el0,
            pc: cpu.pc,
            nzcv: [cpu.n, cpu.z, cpu.c, cpu.v],
            v: cpu.vreg,
            fpcr: cpu.sys.fpcr,
            fpsr: cpu.sys.fpsr,
            tpidr: cpu.sys.tpidr_el0,
        }
    }

    fn load<H: Host>(&self, cpu: &mut Cpu<H>) {
        cpu.x[..31].copy_from_slice(&self.x);
        cpu.sp_el0 = self.sp;
        cpu.pc = self.pc;
        [cpu.n, cpu.z, cpu.c, cpu.v] = self.nzcv;
        cpu.vreg = self.v;
        cpu.sys.fpcr = self.fpcr;
        cpu.sys.fpsr = self.fpsr;
        cpu.sys.tpidr_el0 = self.tpidr;
    }
}

/// What a parked thread waits for.
#[derive(Clone, Debug)]
pub(crate) enum Wait {
    /// A futex wake (x0 0), or its deadline (ETIMEDOUT)
    Futex { addr: u64, mask: u32 },
    /// Its deadline (x0 0)
    Sleep,
    /// Anything that may let its call complete (input, a pipe, an eventfd, a close): the call,
    /// rewound to its svc, runs again. At its deadline it returns `on_timeout` instead.
    Retry { on_timeout: i64 },
    /// Blobs of the filesystem: the call runs again once they have all arrived
    Blobs(Vec<String>),
}

pub(crate) struct Thread {
    pub tid: i64,
    pub ctx: Ctx,
    pub clear_tid: u64,
    pub wait: Option<(Wait, Option<Instant>)>,
    /// The deadline a retried call had when it was parked, for it to keep
    pub resume_deadline: Option<Instant>,
    pub exited: bool,
}

/// What a syscall does to the thread that made it.
pub(crate) enum Ret {
    Val(i64),
    /// Park with the call done (futexes, sleeps): its wait sets x0
    Block(Wait, Option<Instant>),
    /// Park and run the call again when something changes, or at the deadline return `on_timeout`
    Retry {
        deadline: Option<Instant>,
        on_timeout: i64,
    },
    /// Park until these blobs arrive, then run the call again
    Need(Vec<String>),
    Yield,
    ThreadExit,
    GroupExit(i32),
}

/// Why exec failed: blobs of the filesystem to wait for (call it again once they arrived), or
/// a reason.
#[derive(Debug)]
pub enum ExecError {
    Need(Vec<String>),
    Fail(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Need(b) => write!(f, "waiting for {} blob(s) of the filesystem", b.len()),
            ExecError::Fail(s) => f.write_str(s),
        }
    }
}

/// A program's process: its threads, memory map, descriptors, terminal and clock. A snapshot
/// carries it (`save`, `restore`) next to the machine's own state, which holds its memory, the
/// CPU and the filesystem.
pub struct Process {
    pub(crate) ram: Ram,
    pub(crate) ram_size: u64,
    pub(crate) board: Option<Arc<Board>>,
    pub(crate) brk_start: u64,
    pub(crate) brk: u64,
    pub(crate) mmap_floor: u64,
    pub(crate) mmap_top: u64,
    /// Free ranges of the mmap area, start -> end, coalesced. Freed pages are zeroed, so
    /// anything handed out from here reads as zero.
    pub(crate) free: BTreeMap<u64, u64>,
    pub(crate) threads: Vec<Thread>,
    pub(crate) next_tid: i64,
    pub(crate) files: Files,
    pub(crate) tty: Tty,
    pub(crate) cwd: Ino,
    pub(crate) umask: u32,
    /// The executable's path, for /proc/self/exe
    pub(crate) exe: Vec<u8>,
    pub(crate) sigactions: BTreeMap<u64, [u8; 32]>,
    /// The monotonic clock reads `mono_base` ns at `start`, and goes on from there across a
    /// snapshot and its restores.
    pub(crate) start: Instant,
    pub(crate) mono_base: u64,
    pub(crate) output: Vec<u8>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) fault: Option<String>,
    /// Something changed that parked calls may wait on: they run again
    pub(crate) poked: bool,
    /// The thread whose turn it is, and the one whose registers are on the CPU.
    pub(crate) cur: usize,
    pub(crate) loaded: Option<usize>,
    /// The deadline the current call had when it was last parked (Thread::resume_deadline)
    pub(crate) resume_deadline: Option<Instant>,
    /// Print every syscall to stderr.
    pub trace: bool,
    pub stats: Stats,
}

/// What the program asked of its kernel, for reports.
#[derive(Default, Debug)]
pub struct Stats {
    pub syscalls: BTreeMap<u64, u64>,
    pub unknown: BTreeMap<u64, u64>,
    pub opens: Vec<String>,
    pub signals_dropped: u64,
    pub switches: u64,
}

/// What a run of the process ended with.
#[derive(Debug)]
pub enum Status {
    /// Its time ran out: run it again.
    Running,
    /// Every thread waits: until this instant, or (None) for input or blobs only.
    Idle(Option<Instant>),
    Exited(i32),
    Fault(String),
}

pub(crate) fn page_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

/// EL0, interrupts masked, MMU off and no vector table: every exception the program takes stops
/// the CPU, for the process to answer.
fn setup_cpu<H: Host>(cpu: &mut Cpu<H>) {
    cpu.el = 0;
    cpu.spsel = false;
    cpu.daif = 0xf << 6;
    cpu.sys.vbar = 0;
    // UCT (CTR_EL0), DZE (DC ZVA), nTWI, nTWE, UCI at EL0; FP at EL0; EL0 may read the counters
    cpu.sys.sctlr =
        (cpu.sys.sctlr & !1) | (1 << 15) | (1 << 14) | (1 << 16) | (1 << 18) | (1 << 26);
    cpu.sys.cpacr = 3 << 20;
    cpu.sys.cntkctl = 3;
}

impl Process {
    // ------------------------------------------------------------ guest memory

    pub(crate) fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        addr.checked_add(buf.len() as u64)
            .is_some_and(|end| end <= self.ram_size)
            && self.ram.read(addr as usize, buf)
    }

    pub(crate) fn write(&self, addr: u64, data: &[u8]) -> bool {
        addr.checked_add(data.len() as u64)
            .is_some_and(|end| end <= self.ram_size)
            && self.ram.write(addr as usize, data)
    }

    pub(crate) fn bytes_at(&self, addr: u64, len: usize) -> Option<Vec<u8>> {
        let mut b = vec![0; len];
        self.read(addr, &mut b).then_some(b)
    }

    pub(crate) fn u32_at(&self, addr: u64) -> Option<u32> {
        let mut b = [0; 4];
        self.read(addr, &mut b).then(|| u32::from_le_bytes(b))
    }

    pub(crate) fn u64_at(&self, addr: u64) -> Option<u64> {
        let mut b = [0; 8];
        self.read(addr, &mut b).then(|| u64::from_le_bytes(b))
    }

    pub(crate) fn put_u64(&self, addr: u64, v: u64) -> bool {
        self.write(addr, &v.to_le_bytes())
    }

    pub(crate) fn put_u32(&self, addr: u64, v: u32) -> bool {
        self.write(addr, &v.to_le_bytes())
    }

    /// A NUL-terminated string of the program's (at most 4 KiB)
    pub(crate) fn cstr(&self, mut addr: u64) -> Vec<u8> {
        let mut s = Vec::new();
        let mut b = [0u8];
        while s.len() < 4096 && self.read(addr, &mut b) && b[0] != 0 {
            s.push(b[0]);
            addr += 1;
        }
        s
    }

    /// Zero a range the program gave back, and the host memory under it with it.
    pub(crate) fn zero_range(&self, addr: u64, len: u64) {
        let start = addr & !(PAGE - 1);
        let end = page_up(addr + len).min(self.ram_size);
        if start < end {
            self.ram.discard(start as usize, (end - start) as usize);
        }
    }

    /// Highest free range of `len` bytes in the mmap area.
    pub(crate) fn alloc(&mut self, len: u64) -> Option<u64> {
        let (&s, &e) = self.free.iter().rev().find(|(&s, &e)| e - s >= len)?;
        let at = e - len;
        self.free.remove(&s);
        if at > s {
            self.free.insert(s, at);
        }
        Some(at)
    }

    /// Take `[a, b)` out of the free list (MAP_FIXED over it).
    pub(crate) fn carve(&mut self, a: u64, b: u64) {
        let hit: Vec<(u64, u64)> = self
            .free
            .range(..b)
            .filter(|(_, &e)| e > a)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in hit {
            self.free.remove(&s);
            if s < a {
                self.free.insert(s, a);
            }
            if e > b {
                self.free.insert(b, e);
            }
        }
    }

    /// Whether `[a, b)` is all free.
    pub(crate) fn is_free(&self, a: u64, b: u64) -> bool {
        self.free
            .range(..=a)
            .next_back()
            .is_some_and(|(&s, &e)| s <= a && e >= b)
    }

    /// Give `[a, b)` back to the free list, zeroed, merged with its neighbours.
    pub(crate) fn release(&mut self, a: u64, b: u64) {
        let (a, b) = (a.max(self.mmap_floor), b.min(self.mmap_top));
        if a >= b {
            return;
        }
        self.zero_range(a, b - a);
        self.carve(a, b);
        let (mut s, mut e) = (a, b);
        if let Some((&ps, &pe)) = self.free.range(..s).next_back() {
            if pe == s {
                self.free.remove(&ps);
                s = ps;
            }
        }
        if let Some(&ne) = self.free.get(&e) {
            self.free.remove(&e);
            e = ne;
        }
        self.free.insert(s, e);
    }

    // ------------------------------------------------------------ clocks

    pub(crate) fn now_ns(&self, clock: u64) -> u64 {
        match clock {
            // CLOCK_REALTIME, CLOCK_REALTIME_COARSE
            0 | 5 => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64),
            // Monotonic: from a second before the program started, on across snapshots
            _ => self.mono_base + self.start.elapsed().as_nanos() as u64,
        }
    }

    pub(crate) fn timespec(&self, addr: u64) -> Option<Duration> {
        let s = self.u64_at(addr)?;
        let ns = self.u64_at(addr + 8)?;
        Some(Duration::new(s, (ns % 1_000_000_000) as u32))
    }

    /// The host instant at which `clock` reads `abs`.
    pub(crate) fn deadline_at(&self, clock: u64, abs: Duration) -> Instant {
        let now = self.now_ns(clock);
        Instant::now() + Duration::from_nanos((abs.as_nanos() as u64).saturating_sub(now))
    }

    /// The filesystem's time now, for what a program creates or writes
    pub(crate) fn now_secs(&self) -> u64 {
        self.now_ns(0) / 1_000_000_000
    }

    // ------------------------------------------------------------ the filesystem

    /// Runs `f` on the machine's filesystem; None when the machine has none.
    pub(crate) fn fs<T>(&self, f: impl FnOnce(&mut Fs) -> T) -> Option<T> {
        self.board.as_ref()?.with_fs(f)
    }

    pub(crate) fn want_blobs(&self, names: &[String]) {
        if let Some(b) = &self.board {
            b.want_blobs(names);
        }
    }

    // ------------------------------------------------------------ exec

    /// Starts `args[0]`, found in the machine's filesystem (by PATH from `env` when it has no
    /// slash), as execve would: a dynamically linked one with its loader (PT_INTERP) from the
    /// filesystem too. `cwd` is the working directory.
    pub fn exec<H: Host>(
        cpu: &mut Cpu<H>,
        board: Arc<Board>,
        args: &[String],
        env: &[String],
        cwd: &str,
    ) -> Result<Process, ExecError> {
        let fail = |s: String| ExecError::Fail(s);
        let name = args.first().ok_or_else(|| fail("no program".into()))?;
        let path_var = env
            .iter()
            .find_map(|e| e.strip_prefix("PATH="))
            .unwrap_or("/usr/local/bin:/usr/bin:/bin");
        let candidates: Vec<String> = if name.contains('/') {
            vec![name.clone()]
        } else {
            path_var.split(':').map(|d| format!("{d}/{name}")).collect()
        };
        let found = board
            .with_fs(|fs| {
                let cwd_ino = files::resolve(fs, ROOT, cwd.as_bytes(), true).unwrap_or(ROOT);
                candidates.iter().find_map(|p| {
                    let ino = files::resolve(fs, cwd_ino, p.as_bytes(), true).ok()?;
                    fs.get(ino)
                        .filter(|i| !i.is_dir())
                        .map(|_| (p.clone(), ino, cwd_ino))
                })
            })
            .ok_or_else(|| fail("the machine has no filesystem".into()))?;
        let (path, ino, cwd_ino) = found.ok_or_else(|| fail(format!("{name}: not found")))?;
        let elf = read_all(&board, ino)?;
        let mut p = Process::start(cpu, Some(board), &elf, path.as_bytes(), args, env)?;
        p.cwd = cwd_ino;
        Ok(p)
    }

    /// Starts a program given as bytes, with no filesystem (a static one) or with the machine's.
    pub fn exec_bytes<H: Host>(
        cpu: &mut Cpu<H>,
        board: Option<Arc<Board>>,
        elf: &[u8],
        args: &[String],
        env: &[String],
    ) -> Result<Process, ExecError> {
        let exe = args
            .first()
            .map_or(b"/program".to_vec(), |a| a.as_bytes().to_vec());
        Process::start(cpu, board, elf, &exe, args, env)
    }

    fn start<H: Host>(
        cpu: &mut Cpu<H>,
        board: Option<Arc<Board>>,
        elf: &[u8],
        exe: &[u8],
        args: &[String],
        env: &[String],
    ) -> Result<Process, ExecError> {
        let fail = ExecError::Fail;
        let ram = cpu.mem.alias();
        let ram_size = ram.len() as u64;
        let main = elf::header(elf).map_err(fail)?;
        let base = if main.dyn_ { PIE_BASE } else { 0 };
        let loaded = elf::load(&ram, ram_size, elf, base).map_err(fail)?;
        let stack_top = ram_size - PAGE;
        let brk_start = page_up(loaded.end);
        let mmap_top = stack_top - STACK_SIZE - PAGE;
        if brk_start + BRK_SIZE >= mmap_top {
            return Err(fail("RAM too small for the program".into()));
        }
        let mut p = Process {
            ram,
            ram_size,
            board,
            brk_start,
            brk: brk_start,
            mmap_floor: brk_start + BRK_SIZE,
            mmap_top,
            free: BTreeMap::from([(brk_start + BRK_SIZE, mmap_top)]),
            threads: Vec::new(),
            next_tid: 1001,
            files: Files::standard(),
            tty: Tty::new(1000),
            cwd: ROOT,
            umask: 0o022,
            exe: exe.to_vec(),
            sigactions: BTreeMap::new(),
            start: Instant::now(),
            mono_base: 1_000_000_000,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            poked: false,
            cur: 0,
            loaded: None,
            resume_deadline: None,
            trace: false,
            stats: Stats::default(),
        };
        // The dynamic loader, at a base of its own from the top of the mmap area
        let (entry, interp_base) = match &loaded.interp {
            None => (loaded.entry, 0),
            Some(path) => {
                let board = p.board.clone().ok_or_else(|| {
                    fail("dynamically linked: needs the machine's filesystem".into())
                })?;
                let ino = board
                    .with_fs(|fs| files::resolve(fs, ROOT, path, true).ok())
                    .flatten()
                    .ok_or_else(|| {
                        fail(format!(
                            "its loader {} is not in the filesystem",
                            String::from_utf8_lossy(path)
                        ))
                    })?;
                let ld = read_all(&board, ino)?;
                let h = elf::header(&ld).map_err(fail)?;
                let at = p
                    .alloc(h.span)
                    .ok_or_else(|| fail("no room for the loader".into()))?;
                let l = elf::load(&p.ram, ram_size, &ld, at).map_err(fail)?;
                (l.entry, at)
            },
        };
        let mut random = [0u8; 16];
        random.copy_from_slice(&armless::native::random_bytes(16));
        let sp = {
            let write = |a: u64, d: &[u8]| p.write(a, d);
            elf::stack(
                &write,
                stack_top,
                args,
                env,
                &loaded,
                interp_base,
                exe,
                &random,
            )
        };
        setup_cpu(cpu);
        let mut first = Ctx::save(cpu);
        first.x = [0; 31];
        first.v = [0; 32];
        (first.fpcr, first.fpsr, first.tpidr) = (0, 0, 0);
        first.sp = sp;
        first.pc = entry;
        p.threads.push(Thread {
            tid: 1000,
            ctx: first,
            clear_tid: 0,
            wait: None,
            resume_deadline: None,
            exited: false,
        });
        Ok(p)
    }

    // ------------------------------------------------------------ the host's side

    /// Bytes typed into the terminal (keys: the line discipline echoes and edits them as termios
    /// says, and Ctrl-C and the like are signals).
    pub fn input(&mut self, data: &[u8]) {
        let (echo, signals) = self.tty.input(data);
        self.output.extend_from_slice(&echo);
        for sig in signals {
            self.signal(sig);
        }
        self.poked = true;
    }

    /// The end of the terminal's input: reads past what is left return 0.
    pub fn close_input(&mut self) {
        self.tty.closed = true;
        self.poked = true;
    }

    /// The terminal's size changed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.tty.cols = cols;
        self.tty.rows = rows;
        self.signal(28); // SIGWINCH
    }

    /// What the program wrote to its terminal since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn threads(&self) -> usize {
        self.threads.iter().filter(|t| !t.exited).count()
    }

    /// Guest pages that hold anything: what of RAM a snapshot carries. (Reads all of RAM.)
    pub fn guest_pages(&self) -> u64 {
        (0..self.ram_size / PAGE)
            .filter(|&i| !self.ram.is_zero((i * PAGE) as usize, PAGE as usize))
            .count() as u64
    }

    /// A signal for the process. Until signals are delivered: one whose action is to end the
    /// process ends it, as Linux would with no handler; one with a handler is dropped.
    pub(crate) fn signal(&mut self, sig: u64) {
        let handler = self
            .sigactions
            .get(&sig)
            .map(|a| u64::from_le_bytes(a[..8].try_into().unwrap()))
            .unwrap_or(0);
        // SIG_DFL whose default is to end the process (not SIGCHLD, SIGWINCH, SIGURG, SIGCONT)
        if handler == 0 && !matches!(sig, 17 | 18 | 23 | 28) {
            self.exit_code = Some(128 + sig as i32);
        } else {
            self.stats.signals_dropped += 1;
        }
    }

    // ------------------------------------------------------------ running

    /// Run the program for up to `budget` of wall time, or until every thread waits.
    pub fn run<H: Host>(&mut self, cpu: &mut Cpu<H>, budget: Duration) -> Status {
        let until = Instant::now() + budget;
        'run: loop {
            if let Some(c) = self.exit_code {
                return Status::Exited(c);
            }
            if let Some(f) = &self.fault {
                return Status::Fault(f.clone());
            }
            if Instant::now() >= until {
                return Status::Running;
            }
            self.wake();
            // The next runnable thread, round robin, starting with the current one
            let n = self.threads.len();
            let cur = self.cur;
            let Some(next) = (0..n)
                .map(|i| (cur + i) % n)
                .find(|&i| !self.threads[i].exited && self.threads[i].wait.is_none())
            else {
                if self.threads.iter().all(|t| t.exited) {
                    self.exit_code = Some(0);
                    continue;
                }
                return Status::Idle(
                    self.threads
                        .iter()
                        .filter_map(|t| t.wait.as_ref().and_then(|(_, d)| *d))
                        .min(),
                );
            };
            if self.loaded != Some(next) {
                self.park(cpu);
                self.threads[next].ctx.load(cpu);
                self.loaded = Some(next);
                self.stats.switches += 1;
            }
            self.cur = next;

            // Run it until it blocks, faults, or its slice ends: syscalls answered on the way
            // count against the slice, so a thread that polls the clock cannot keep the CPU.
            let slice_start = cpu.insn_count;
            loop {
                let used = cpu.insn_count - slice_start;
                if used >= SLICE {
                    self.cur = (self.cur + 1) % self.threads.len();
                    continue 'run;
                }
                match cpu.run(SLICE - used) {
                    Exit::Budget | Exit::Idle { .. } => {
                        self.cur = (self.cur + 1) % self.threads.len();
                        continue 'run;
                    },
                    Exit::Stop(StopReason::UnhandledException { esr, elr, far }) => {
                        let ec = esr >> 26;
                        if ec != 0x15 {
                            self.fault = Some(format!("exception class {ec:#x} (esr {esr:#x}) at pc {elr:#x}, far {far:#x}"));
                            continue 'run;
                        }
                        cpu.pc = elr;
                        let cur = self.cur;
                        self.resume_deadline = self.threads[cur].resume_deadline.take();
                        let (nr, a0, a1, a2) = (cpu.x[8], cpu.x[0], cpu.x[1], cpu.x[2]);
                        let ret = self.syscall(cpu, cur);
                        if self.trace {
                            let r = match &ret {
                                Ret::Val(r) => format!("{r:#x}"),
                                Ret::Block(w, d) => format!(
                                    "block {w:?} {:?}",
                                    d.map(|d| d.saturating_duration_since(Instant::now()))
                                ),
                                Ret::Retry { deadline, .. } => format!(
                                    "wait {:?}",
                                    deadline.map(|d| d.saturating_duration_since(Instant::now()))
                                ),
                                Ret::Need(b) => format!("needs {} blob(s)", b.len()),
                                Ret::Yield => "yield".into(),
                                Ret::ThreadExit => "thread exit".into(),
                                Ret::GroupExit(c) => format!("exit {c}"),
                            };
                            eprintln!(
                                "[{}] sys {nr}({a0:#x}, {a1:#x}, {a2:#x}) = {r}",
                                self.threads[cur].tid
                            );
                        }
                        match ret {
                            Ret::Val(r) => {
                                cpu.x[0] = r as u64;
                                if Instant::now() >= until || self.exit_code.is_some() {
                                    continue 'run;
                                }
                            },
                            Ret::Yield => {
                                cpu.x[0] = 0;
                                self.cur = (cur + 1) % self.threads.len();
                                continue 'run;
                            },
                            Ret::Block(w, d) => {
                                self.park(cpu);
                                self.threads[cur].wait = Some((w, d));
                                self.cur = (cur + 1) % self.threads.len();
                                continue 'run;
                            },
                            Ret::Retry {
                                deadline,
                                on_timeout,
                            } => {
                                cpu.pc = elr - 4;
                                self.park(cpu);
                                self.threads[cur].wait =
                                    Some((Wait::Retry { on_timeout }, deadline));
                                self.cur = (cur + 1) % self.threads.len();
                                continue 'run;
                            },
                            Ret::Need(blobs) => {
                                self.want_blobs(&blobs);
                                cpu.pc = elr - 4;
                                self.park(cpu);
                                self.threads[cur].wait = Some((Wait::Blobs(blobs), None));
                                self.cur = (cur + 1) % self.threads.len();
                                continue 'run;
                            },
                            Ret::ThreadExit => {
                                let ct = self.threads[cur].clear_tid;
                                if ct != 0 {
                                    self.write(ct, &0u32.to_le_bytes());
                                    self.futex_wake(ct, 1, u32::MAX);
                                }
                                self.threads[cur].exited = true;
                                self.loaded = None;
                                continue 'run;
                            },
                            Ret::GroupExit(code) => {
                                self.exit_code = Some(code);
                                continue 'run;
                            },
                        }
                    },
                    Exit::Stop(r) => {
                        self.fault = Some(format!("stopped: {r:?}"));
                        continue 'run;
                    },
                }
            }
        }
    }

    /// Wakes the parked threads whose wait is over: deadlines passed, blobs arrived, and (when
    /// something changed) the calls that retry.
    fn wake(&mut self) {
        let now = Instant::now();
        let poked = std::mem::take(&mut self.poked);
        let blobs_in: Vec<bool> = self
            .threads
            .iter()
            .map(|t| match &t.wait {
                Some((Wait::Blobs(names), _)) => self
                    .fs(|fs| names.iter().all(|n| fs.has_blob(n)))
                    .unwrap_or(true),
                _ => false,
            })
            .collect();
        for (t, blobs_in) in self.threads.iter_mut().zip(blobs_in) {
            let Some((w, deadline)) = t.wait.clone() else {
                continue;
            };
            let expired = deadline.is_some_and(|d| d <= now);
            match w {
                Wait::Futex { .. } if expired => {
                    t.ctx.x[0] = (-110i64) as u64; // ETIMEDOUT
                    t.wait = None;
                },
                Wait::Sleep if expired => {
                    t.ctx.x[0] = 0;
                    t.wait = None;
                },
                Wait::Retry { on_timeout } if expired => {
                    // Its call returns rather than run again: past its svc
                    t.ctx.pc += 4;
                    t.ctx.x[0] = on_timeout as u64;
                    t.wait = None;
                },
                Wait::Retry { .. } if poked => {
                    t.resume_deadline = deadline;
                    t.wait = None;
                },
                Wait::Blobs(_) if blobs_in => t.wait = None,
                _ => {},
            }
        }
    }

    pub(crate) fn futex_wake(&mut self, addr: u64, n: u64, mask: u32) -> i64 {
        let mut woken = 0;
        for t in self.threads.iter_mut() {
            if woken >= n {
                break;
            }
            if let Some((Wait::Futex { addr: a, mask: m }, _)) = t.wait {
                if a == addr && m & mask != 0 {
                    t.wait = None;
                    t.ctx.x[0] = 0;
                    woken += 1;
                }
            }
        }
        woken as i64
    }

    /// Takes the registers of the thread on the CPU back into the process, so that every thread
    /// is in the process's state and none in the CPU's.
    pub fn park<H: Host>(&mut self, cpu: &Cpu<H>) {
        if let Some(i) = self.loaded.take() {
            self.threads[i].ctx = Ctx::save(cpu);
        }
    }

    // ------------------------------------------------------------ snapshots

    /// The process's state for a snapshot (the machine's own snapshot holds its memory and
    /// filesystem): every thread, its waits as time left, the memory map, the descriptors, the
    /// terminal, the clock.
    pub fn save<H: Host>(&mut self, cpu: &Cpu<H>) -> Vec<u8> {
        self.park(cpu);
        let now = Instant::now();
        let mut w = W(MAGIC.to_vec());
        for v in [
            self.ram_size,
            self.brk_start,
            self.brk,
            self.mmap_floor,
            self.mmap_top,
        ] {
            w.u(v);
        }
        w.u(self.free.len() as u64);
        for (&s, &e) in &self.free {
            w.u(s);
            w.u(e);
        }
        w.u(self.next_tid as u64);
        w.u(self.now_ns(1));
        w.u(self.cur as u64);
        w.u(self.threads.len() as u64);
        for t in &self.threads {
            w.u(t.tid as u64);
            w.u(t.clear_tid);
            w.u(t.exited as u64);
            let c = &t.ctx;
            c.x.iter().for_each(|&x| w.u(x));
            w.u(c.sp);
            w.u(c.pc);
            w.u(c
                .nzcv
                .iter()
                .enumerate()
                .fold(0, |a, (i, &b)| a | (b as u64) << i));
            for v in c.v {
                w.u(v as u64);
                w.u((v >> 64) as u64);
            }
            w.u(c.fpcr);
            w.u(c.fpsr);
            w.u(c.tpidr);
            let left = |d: Option<Instant>| {
                d.map_or(u64::MAX, |d| {
                    d.saturating_duration_since(now).as_nanos() as u64
                })
            };
            match &t.wait {
                None => w.u(0),
                Some((Wait::Futex { addr, mask }, _)) => {
                    w.u(1);
                    w.u(*addr);
                    w.u(*mask as u64);
                },
                Some((Wait::Sleep, _)) => w.u(2),
                Some((Wait::Retry { on_timeout }, _)) => {
                    w.u(3);
                    w.u(*on_timeout as u64);
                },
                Some((Wait::Blobs(names), _)) => {
                    w.u(4);
                    w.u(names.len() as u64);
                    names.iter().for_each(|n| w.b(n.as_bytes()));
                },
            }
            w.u(left(t.wait.as_ref().and_then(|(_, d)| *d)));
            w.u(left(t.resume_deadline));
        }
        self.files.save(&mut w);
        self.tty.save(&mut w);
        w.u(self.cwd);
        w.u(self.umask as u64);
        w.b(&self.exe);
        w.u(self.sigactions.len() as u64);
        for (&sig, act) in &self.sigactions {
            w.u(sig);
            w.b(act);
        }
        w.0
    }

    /// The process from its `save`, on a CPU the machine's snapshot was restored to (and the
    /// machine, for its filesystem).
    pub fn restore<H: Host>(
        cpu: &mut Cpu<H>,
        board: Option<Arc<Board>>,
        data: &[u8],
    ) -> Result<Process, String> {
        let mut r = R(data.strip_prefix(MAGIC).ok_or("not a process state")?);
        let ram = cpu.mem.alias();
        let ram_size = r.u()?;
        if ram_size != ram.len() as u64 {
            return Err(format!(
                "the process had {ram_size} bytes of RAM, the machine {}",
                ram.len()
            ));
        }
        let (brk_start, brk, mmap_floor, mmap_top) = (r.u()?, r.u()?, r.u()?, r.u()?);
        let mut free = BTreeMap::new();
        for _ in 0..r.u()? {
            free.insert(r.u()?, r.u()?);
        }
        let next_tid = r.u()? as i64;
        let mono_base = r.u()?;
        let cur = r.u()? as usize;
        let now = Instant::now();
        let at = |left: u64| (left != u64::MAX).then(|| now + Duration::from_nanos(left));
        let mut threads = Vec::new();
        for _ in 0..r.u()? {
            let (tid, clear_tid, exited) = (r.u()? as i64, r.u()?, r.u()? != 0);
            let mut x = [0; 31];
            for v in x.iter_mut() {
                *v = r.u()?;
            }
            let (sp, pc, flags) = (r.u()?, r.u()?, r.u()?);
            let mut v = [0u128; 32];
            for q in v.iter_mut() {
                *q = r.u()? as u128 | (r.u()? as u128) << 64;
            }
            let ctx = Ctx {
                x,
                sp,
                pc,
                nzcv: [
                    flags & 1 != 0,
                    flags & 2 != 0,
                    flags & 4 != 0,
                    flags & 8 != 0,
                ],
                v,
                fpcr: r.u()?,
                fpsr: r.u()?,
                tpidr: r.u()?,
            };
            let wait = match r.u()? {
                0 => None,
                1 => Some(Wait::Futex {
                    addr: r.u()?,
                    mask: r.u()? as u32,
                }),
                2 => Some(Wait::Sleep),
                3 => Some(Wait::Retry {
                    on_timeout: r.u()? as i64,
                }),
                4 => {
                    let n = r.u()?;
                    let names = (0..n)
                        .map(|_| r.b().map(|b| String::from_utf8_lossy(b).into_owned()))
                        .collect::<Result<_, _>>()?;
                    Some(Wait::Blobs(names))
                },
                t => return Err(format!("unknown wait {t}")),
            };
            let deadline = at(r.u()?);
            let resume_deadline = at(r.u()?);
            threads.push(Thread {
                tid,
                ctx,
                clear_tid,
                wait: wait.map(|w| (w, deadline)),
                resume_deadline,
                exited,
            });
        }
        let files = Files::load(&mut r)?;
        let tty = Tty::load(&mut r)?;
        let cwd = r.u()?;
        let umask = r.u()? as u32;
        let exe = r.b()?.to_vec();
        let mut sigactions = BTreeMap::new();
        for _ in 0..r.u()? {
            let sig = r.u()?;
            sigactions.insert(sig, r.b()?.try_into().map_err(|_| "bad sigaction")?);
        }
        setup_cpu(cpu);
        let p = Process {
            ram,
            ram_size,
            board,
            brk_start,
            brk,
            mmap_floor,
            mmap_top,
            free,
            threads,
            next_tid,
            files,
            tty,
            cwd,
            umask,
            exe,
            sigactions,
            start: now,
            mono_base,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            // Whatever waited on blobs asks for them again: the host that restores may not have them
            poked: true,
            cur,
            loaded: None,
            resume_deadline: None,
            trace: false,
            stats: Stats::default(),
        };
        let wanted: Vec<String> = p
            .threads
            .iter()
            .filter_map(|t| match &t.wait {
                Some((Wait::Blobs(b), _)) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        p.want_blobs(&wanted);
        Ok(p)
    }
}

/// A whole file of the machine's filesystem, for exec (its blobs asked for when they aren't there).
fn read_all(board: &Board, ino: Ino) -> Result<Vec<u8>, ExecError> {
    match board.with_fs(|fs| files::got(fs.read_all(ino))) {
        Some(files::Got::Ok(b)) => Ok(b),
        Some(files::Got::Need(blobs)) => {
            board.want_blobs(&blobs);
            Err(ExecError::Need(blobs))
        },
        Some(files::Got::Err(e)) => Err(ExecError::Fail(format!("reading it: errno {}", -e))),
        None => Err(ExecError::Fail("the machine has no filesystem".into())),
    }
}

const MAGIC: &[u8; 8] = b"ARMLUSR2";
