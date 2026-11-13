//! User mode for armless: a Linux program on an armless CPU with no guest kernel. The CPU runs it
//! at EL0 with no vector table, and every exception it takes stops the CPU
//! (`StopReason::UnhandledException`, `Cpu::stop_on_exceptions`) and is answered here: system
//! calls by a Linux personality, a touch of a page that has no frame yet by giving it one (mm.rs),
//! other faults by stopping the process.
//!
//! What it gives a program: static or dynamically linked executables (the dynamic loader comes
//! from the filesystem, as execve has it), the machine's filesystem (armless's, the one the 9p
//! device serves a Linux guest), a terminal (termios, the window size, a line discipline), pipes,
//! sockets (net.rs: AF_UNIX, and TCP and UDP on the loopback in memory, a network at the host for
//! the rest), threads (clone, futexes), processes (fork, vfork, execve, wait4, kill: proc.rs,
//! exec.rs), epoll, poll and select, clocks and sleeps. Blocking calls park their thread and run
//! again when something changes, so a snapshot can be taken whatever it waits on.
//!
//! Signals are delivered as Linux delivers them (`signal`): handlers on the frame the kernel
//! builds, masks, alternate stacks, blocking calls interrupted (EINTR, or restarted with
//! SA_RESTART), and the terminal's Ctrl-C, Ctrl-\ and window size as SIGINT, SIGQUIT and
//! SIGWINCH. A host signals the process with `kill`.
//!
//! Memory is virtual (mm.rs): the MMU is on, with page tables of the process's own, and a page
//! has a frame of RAM when the program first touches it. Files are mapped lazily: the
//! executable's segments, its loader's, and mmap of a file get a page's contents when it is
//! first touched, so a snapshot holds only the pages a program used. Protections hold
//! (PROT_NONE, read-only and no-execute pages fault). The address space (512 GiB) has nothing to
//! do with the RAM: what a program maps costs frames only where it touches, so a reservation far
//! larger than the RAM is fine, and a fork shares its pages copy on write.
//!
//! Not yet: more than one vCPU (threads of all processes take turns on one CPU).

// No `unsafe` but in the exports JavaScript calls (`wasm`: raw pointers into the wasm memory)
#![deny(unsafe_code)]

mod creds;
mod elf;
mod exec;
mod files;
pub mod host;
mod mm;
mod net;
mod netlink;
mod proc;
mod sched;
mod ser;
mod itimer;
mod shm;
mod timerfd;
mod signal;
mod sys;
mod tty;
#[cfg(any(target_arch = "wasm32", feature = "wasm-api"))]
#[allow(unsafe_code)]
pub mod wasm;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use armless::cpu::{Cpu, Exit, StopReason};
use armless::dev::fs::{Fs, Ino, ROOT};
use armless::{Board, Host, Ram};

use files::Files;
use host::Instant;
use ser::{R, W};
use signal::{Signals, ThreadSig};
use tty::Tty;

pub(crate) const PAGE: u64 = 4096;
/// The pid of the first process, the program the machine runs: when it ends, the machine does
pub(crate) const INIT: i64 = 1000;
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
    pub(crate) fn save<H: Host>(cpu: &Cpu<H>) -> Ctx {
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

    pub(crate) fn load<H: Host>(&self, cpu: &mut Cpu<H>) {
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
    /// A child made by vfork, until it execs or exits (its pid): x0 is then the child's pid
    Vfork(i64),
}

pub(crate) struct Thread {
    pub tid: i64,
    /// The process it belongs to
    pub pid: i64,
    pub ctx: Ctx,
    pub clear_tid: u64,
    pub wait: Option<(Wait, Option<Instant>)>,
    /// The deadline a retried call had when it was parked, for it to keep
    pub resume_deadline: Option<Instant>,
    pub exited: bool,
    pub sig: ThreadSig,
    pub sched: sched::Sched,
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
    /// The thread ends (exit), with this exit code if it is the process's last
    ThreadExit(i32),
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

/// What a process has of its own: its place in the process tree, address space, descriptors,
/// working directory, signal actions. The process that runs (`Process::p`) has its descriptors in
/// `Process::files`, the others in here (`Process::switch_to` swaps them).
pub(crate) struct Proc {
    pub pid: i64,
    pub ppid: i64,
    /// Process group and session
    pub pgid: i64,
    pub sid: i64,
    pub mm: mm::SharedMm,
    /// fd -> (description, close on exec): empty while this is the process that runs
    pub fds: BTreeMap<i32, (u32, bool)>,
    /// The signal its parent gets when it ends (0: none)
    pub exit_signal: u32,
    /// Its wait status once it has ended and no one has waited for it
    pub zombie: Option<i32>,
    /// Nobody will wait for it (its parent ignores its end): it goes once it is not the current one
    pub reap: bool,
    pub cwd: Ino,
    pub umask: u32,
    /// The executable's path, for /proc/self/exe
    pub exe: Vec<u8>,
    pub sig: Signals,
    pub creds: Creds,
    /// ITIMER_REAL, when armed (itimer.rs): a child doesn't inherit it, exec keeps it
    pub alarm: Option<itimer::Alarm>,
}

/// Who a process is: its real, effective and saved user and group ids. A program starts as root;
/// su, pg_ctl and the like become another user with setuid and setgid, and Postgres refuses to
/// run as root, so what they set is kept, given to children and to what the process execs, and
/// owns the files it makes. Permissions aren't checked by them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Creds {
    /// Real, effective, saved
    pub uid: [u32; 3],
    pub gid: [u32; 3],
}

impl Creds {
    pub(crate) fn euid(&self) -> u32 {
        self.uid[1]
    }

    pub(crate) fn egid(&self) -> u32 {
        self.gid[1]
    }

    fn save(&self, w: &mut W) {
        for v in self.uid.iter().chain(&self.gid) {
            w.u(*v as u64);
        }
    }

    fn load(r: &mut R) -> Result<Creds, String> {
        let mut c = Creds::default();
        for v in c.uid.iter_mut().chain(c.gid.iter_mut()) {
            *v = r.u()? as u32;
        }
        Ok(c)
    }
}

/// A program's process: its threads, memory map, descriptors, terminal and clock. A snapshot
/// carries it (`save`, `restore`) next to the machine's own state, which holds its memory, the
/// CPU and the filesystem.
pub struct Process {
    pub(crate) ram: Ram,
    pub(crate) ram_size: u64,
    pub(crate) board: Option<Arc<Board>>,
    /// The frames of RAM the address spaces are made of (mm.rs)
    pub(crate) frames: RefCell<mm::Frames>,
    /// The next ASID to hand out, and the generation they are from
    pub(crate) asids: Cell<(u64, u64)>,
    /// Blobs a host access to the program's memory needs and hasn't got (collected by `syscall_lazily`)
    pub(crate) need: RefCell<Vec<String>>,
    /// Pages of files read into frames so far
    pub(crate) filled: Cell<u64>,
    /// The pages of files mapped MAP_SHARED, one frame each whoever maps them: (file, offset)
    /// -> frame, which this holds a reference on (mm.rs `populate`)
    pub(crate) file_pages: RefCell<BTreeMap<(Ino, u64), u64>>,
    /// The process that runs
    pub(crate) p: Proc,
    /// The other processes (zombies too), by pid
    pub(crate) procs: BTreeMap<i64, Proc>,
    /// The threads of every process, in the order they take turns
    pub(crate) threads: Vec<Thread>,
    /// The next pid or tid: they are one number space
    pub(crate) next_id: i64,
    /// Something was signalled or a mask changed: what is pending may be deliverable now
    pub(crate) sig_dirty: bool,
    /// The descriptions every process shares, and the current process's descriptors
    pub(crate) files: Files,
    pub(crate) tty: Tty,
    /// The monotonic clock reads `mono_base` ns at `start`, and goes on from there across a
    /// snapshot and its restores.
    pub(crate) start: Instant,
    pub(crate) mono_base: u64,
    pub(crate) output: Vec<u8>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) fault: Option<String>,
    /// Something changed that parked calls may wait on: they run again
    pub(crate) poked: bool,
    /// How many times something changed (`poke`): an edge-triggered epoll watch on a descriptor
    /// still ready is reported again when this has moved since it was last reported
    pub(crate) epoch: u64,
    /// The thread whose turn it is, and the one whose registers are on the CPU.
    pub(crate) cur: usize,
    pub(crate) loaded: Option<usize>,
    /// The deadline the current call had when it was last parked (Thread::resume_deadline)
    pub(crate) resume_deadline: Option<Instant>,
    /// Print every syscall to stderr.
    /// The machine's System V shared memory (shm.rs)
    pub(crate) shm: shm::Shm,
    pub trace: bool,
    pub stats: Stats,
}

/// What the program asked of its kernel, for reports.
#[derive(Default, Debug)]
pub struct Stats {
    pub syscalls: BTreeMap<u64, u64>,
    pub unknown: BTreeMap<u64, u64>,
    pub opens: Vec<String>,
    /// Signals dropped: ignored, by their action or by default
    pub signals_ignored: u64,
    /// Signals whose handler ran
    pub signals_delivered: u64,
    pub switches: u64,
    /// What a process that is not the first faulted on: it died of SIGSEGV
    pub faults: Vec<String>,
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

/// Where an executable's pieces go: its segments, the stack, the break and the mmap area.
struct Layout {
    loaded: elf::Loaded,
    segs: Vec<elf::Seg>,
    stack_top: u64,
    brk_start: u64,
    mmap_top: u64,
}

impl Layout {
    /// `image`'s layout: its headers are read from wherever they are in its file (`board`'s).
    fn of(image: &mm::Image, board: Option<&Board>) -> Result<Layout, ExecError> {
        let main = elf::parse(image, board)?;
        let base = if main.header().dyn_ { PIE_BASE } else { 0 };
        let (loaded, segs) = elf::plan(&main, mm::VA_END, base).map_err(ExecError::Fail)?;
        let stack_top = mm::VA_END - PAGE;
        let brk_start = page_up(loaded.end);
        let mmap_top = stack_top - STACK_SIZE - PAGE;
        if brk_start + BRK_SIZE >= mmap_top {
            return Err(ExecError::Fail(
                "the program is too big for the address space".into(),
            ));
        }
        Ok(Layout {
            loaded,
            segs,
            stack_top,
            brk_start,
            mmap_top,
        })
    }

    /// An empty address space for it: the stack, and nothing else.
    fn space(&self, frames: &mut mm::Frames) -> Result<mm::Mm, ExecError> {
        let mut mm = mm::Mm::new(
            frames,
            self.brk_start,
            self.brk_start + BRK_SIZE,
            self.mmap_top,
        )
        .ok_or_else(|| ExecError::Fail("no memory for the program".into()))?;
        let bottom = self.stack_top - STACK_SIZE;
        mm.insert(
            bottom,
            self.stack_top,
            mm::Vma {
                end: self.stack_top,
                prot: mm::PROT_READ | mm::PROT_WRITE,
                shared: false,
                file: None,
            },
        );
        Ok(mm)
    }
}

/// EL0, interrupts masked and no vector table: every exception the program takes stops the CPU,
/// for the process to answer. The MMU is switched on with the process's tables (`sync_mmu`).
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
    cpu.stop_on_exceptions = true;
}

impl Process {
    // ------------------------------------------------------------ guest memory

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
    pub(crate) fn cstr(&self, addr: u64) -> Vec<u8> {
        let mut s = Vec::new();
        let mut at = addr;
        while s.len() < 4096 {
            let n = (PAGE - at % PAGE) as usize;
            let mut b = vec![0u8; n];
            if !self.read(at, &mut b) {
                break;
            }
            match b.iter().position(|&c| c == 0) {
                Some(i) => {
                    s.extend_from_slice(&b[..i]);
                    return s;
                },
                None => s.extend_from_slice(&b),
            }
            at += n as u64;
        }
        s.truncate(4096);
        s
    }

    // ------------------------------------------------------------ clocks

    pub(crate) fn now_ns(&self, clock: u64) -> u64 {
        match clock {
            // CLOCK_REALTIME, CLOCK_REALTIME_COARSE
            0 | 5 => host::wall_ns(),
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
    /// filesystem too, and a `#!` script with its interpreter (an image's command is often a
    /// script: Postgres's start-postgres, Supabase's launcher). `cwd` is the working directory.
    pub fn exec<H: Host>(
        cpu: &mut Cpu<H>,
        board: Arc<Board>,
        args: &[String],
        env: &[String],
        cwd: &str,
    ) -> Result<Process, ExecError> {
        Process::exec_script(cpu, board, args, env, cwd, 0)
    }

    fn exec_script<H: Host>(
        cpu: &mut Cpu<H>,
        board: Arc<Board>,
        args: &[String],
        env: &[String],
        cwd: &str,
        depth: usize,
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
        board.with_fs(dev_shm);
        let image = mm::Image::of(&board, ino)?;
        // A script: its interpreter, the interpreter's argument, the script's path, then what
        // the script was given after its own name (as execve does)
        if let Some((interp, arg)) = exec::shebang(image.head()) {
            if depth >= exec::MAX_SCRIPT_DEPTH {
                return Err(fail(format!("{name}: too many levels of #! interpreters")));
            }
            let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
            let mut next = vec![s(&interp)];
            next.extend(arg.as_deref().map(s));
            next.push(path);
            next.extend(args[1..].iter().cloned());
            return Process::exec_script(cpu, board, &next, env, cwd, depth + 1);
        }
        let exe = board
            .with_fs(|fs| files::canonical(fs, cwd_ino, path.as_bytes()).ok())
            .flatten();
        let mut p = Process::start(cpu, Some(board), image, path.as_bytes(), args, env)?;
        p.p.cwd = cwd_ino;
        // /proc/self/exe: the file itself, symlinks resolved (exec.rs says why)
        if let Some(exe) = exe {
            p.p.exe = exe;
        }
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
        Process::start(cpu, board, mm::Image::Bytes(elf.into()), &exe, args, env)
    }

    fn start<H: Host>(
        cpu: &mut Cpu<H>,
        board: Option<Arc<Board>>,
        image: mm::Image,
        exe: &[u8],
        args: &[String],
        env: &[String],
    ) -> Result<Process, ExecError> {
        let ram = cpu.mem.alias();
        let ram_size = ram.len() as u64;
        let layout = Layout::of(&image, board.as_deref())?;
        // What the CPUs decoded from a page of RAM is dropped when its frame is freed or the host
        // writes it
        let machine = cpu.machine.clone();
        let purge = Box::new(move |page: u64| {
            let flag = &machine.code_pages[page as usize];
            if flag.load(Ordering::Relaxed) != 0 {
                flag.store(0, Ordering::Relaxed);
                machine.invalidate_code(page as u32, usize::MAX);
            }
        });
        let mut frames = mm::Frames::new(ram.alias(), purge);
        let mm = layout.space(&mut frames)?;
        let mut p = Process {
            ram,
            ram_size,
            board,
            frames: RefCell::new(frames),
            asids: Cell::new((1, 0)),
            need: RefCell::new(Vec::new()),
            filled: Cell::new(0),
            file_pages: RefCell::new(BTreeMap::new()),
            p: Proc {
                pid: INIT,
                ppid: 1,
                pgid: INIT,
                sid: INIT,
                mm: Rc::new(RefCell::new(mm)),
                fds: BTreeMap::new(),
                exit_signal: 0,
                zombie: None,
                reap: false,
                cwd: ROOT,
                umask: 0o022,
                exe: exe.to_vec(),
                sig: Signals::default(),
                creds: Creds::default(),
                alarm: None,
            },
            procs: BTreeMap::new(),
            threads: Vec::new(),
            next_id: INIT + 1,
            sig_dirty: false,
            files: Files::standard(),
            tty: Tty::new(INIT),
            start: Instant::now(),
            mono_base: 1_000_000_000,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            poked: false,
            epoch: 0,
            cur: 0,
            loaded: None,
            resume_deadline: None,
            trace: false,
            stats: Stats::default(),
            shm: shm::Shm::default(),
        };
        p.net_attach();
        let (entry, sp) = match p.load_image(&image, &layout, args, env, exe) {
            Ok(r) => r,
            Err(e) => {
                // What it took of RAM goes back to zero, for a start that runs again
                p.teardown();
                return Err(e);
            },
        };
        setup_cpu(cpu);
        p.sync_mmu(cpu);
        let mut first = Ctx::save(cpu);
        first.x = [0; 31];
        first.v = [0; 32];
        (first.fpcr, first.fpsr, first.tpidr) = (0, 0, 0);
        first.sp = sp;
        first.pc = entry;
        p.threads.push(Thread {
            tid: INIT,
            pid: INIT,
            ctx: first,
            clear_tid: 0,
            wait: None,
            resume_deadline: None,
            exited: false,
            sig: ThreadSig::default(),
            sched: Default::default(),
        });
        Ok(p)
    }

    /// `image`, its loader if it has one, and the initial stack placed in the address space
    /// `layout` made (`self.p.mm`): where it starts, and the stack pointer.
    fn load_image(
        &mut self,
        image: &mm::Image,
        layout: &Layout,
        args: &[String],
        env: &[String],
        exe: &[u8],
    ) -> Result<(u64, u64), ExecError> {
        let fail = ExecError::Fail;
        let loaded = &layout.loaded;
        // The dynamic loader's first, so what waits for blobs does before anything is mapped
        let ld = match &loaded.interp {
            None => None,
            Some(path) => {
                let board = self.board.clone().ok_or_else(|| {
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
                Some(mm::Image::of(&board, ino)?)
            },
        };
        self.place(image, &layout.segs)?;
        // The dynamic loader, at a base of its own from the top of the mmap area
        let (entry, interp_base) = match ld {
            None => (loaded.entry, 0),
            Some(ld) => {
                let parsed = elf::parse(&ld, self.board.as_deref())?;
                let at = self
                    .p
                    .mm
                    .borrow_mut()
                    .alloc(parsed.header().span)
                    .ok_or_else(|| fail("no room for the loader".into()))?;
                let (l, segs) = elf::plan(&parsed, mm::VA_END, at).map_err(fail)?;
                self.place(&ld, &segs)?;
                (l.entry, at)
            },
        };
        let mut random = [0u8; 16];
        random.copy_from_slice(&host::random_bytes(16));
        let ok = std::cell::Cell::new(true);
        let sp = {
            let write = |a: u64, d: &[u8]| {
                let w = self.write_any(a, d);
                ok.set(ok.get() && w);
                w
            };
            elf::stack(
                &write,
                layout.stack_top,
                args,
                env,
                loaded,
                interp_base,
                exe,
                &random,
            )
        };
        if !ok.get() {
            return Err(fail("no memory for the stack".into()));
        }
        Ok((entry, sp))
    }

    /// Gives the address space back: everything in it, its frames zeroed.
    fn teardown(&mut self) {
        let refs = {
            let mut mm = self.p.mm.borrow_mut();
            mm.destroy(&mut self.frames.borrow_mut());
            mm.take_refs()
        };
        self.settle_refs(refs);
        self.trim_file_pages();
    }

    // ------------------------------------------------------------ the host's side

    /// Bytes typed into the terminal (keys: the line discipline echoes and edits them as termios
    /// says, and Ctrl-C and the like are signals).
    pub fn input(&mut self, data: &[u8]) {
        let typed = self.tty.input(data);
        if typed.flush_output {
            // What the program wrote that the host hasn't taken yet goes, as Linux flushes it
            self.output.clear();
        }
        self.output.extend_from_slice(&typed.echo);
        for sig in typed.signals {
            self.signal_foreground(sig);
        }
        self.poke();
    }

    /// Something changed that parked calls may wait on.
    pub(crate) fn poke(&mut self) {
        self.poked = true;
        self.epoch += 1;
    }

    /// A signal for the first process from outside it (`kill`: SIGINT for an interrupt the
    /// host's user asked for outside the terminal, say). Delivered when the process next runs: a
    /// thread waiting in a call is woken for it, so run it after.
    pub fn kill(&mut self, sig: u32) {
        self.raise_in(INIT, sig, None, signal::SI_KERNEL);
    }

    /// The end of the terminal's input: reads past what is left return 0.
    pub fn close_input(&mut self) {
        self.tty.closed = true;
        self.poke();
    }

    /// The terminal's size changed.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (self.tty.cols, self.tty.rows) != (cols, rows) {
            self.tty.cols = cols;
            self.tty.rows = rows;
            self.signal_foreground(signal::SIGWINCH);
        }
    }

    /// What the program wrote to its terminal since the last call.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// The live threads of all processes.
    pub fn threads(&self) -> usize {
        self.threads.iter().filter(|t| !t.exited).count()
    }

    /// The processes that haven't ended.
    pub fn processes(&self) -> usize {
        self.proc_list().filter(|p| p.zombie.is_none()).count()
    }

    /// Whether a thread waits on contents of the filesystem the host hasn't given yet (blobs).
    /// When every thread waits (`Status::Idle`), this tells a program that is busy reading files
    /// from one that waits for input: a host that answers when the program is done (a REPL's
    /// answer, up to its next prompt) must not take the first for the second.
    pub fn waiting_on_blobs(&self) -> bool {
        self.threads
            .iter()
            .any(|t| !t.exited && matches!(t.wait, Some((Wait::Blobs(_), _))))
    }

    /// Whether the program waits for input: a thread is parked in a call that input could
    /// complete (a read of the terminal or a pipe, poll, select, epoll), and none waits on blobs.
    /// When every thread waits (`Status::Idle`), this tells a REPL at its prompt from a program
    /// that only sleeps (`time.sleep` in the middle of a command, whose threads wait on their
    /// deadlines or on each other) — both are idle, only the first is done.
    pub fn waiting_for_input(&self) -> bool {
        // (A parent that waits for its child, wait4 and waitid, waits for no input.)
        !self.waiting_on_blobs()
            && self.threads.iter().any(|t| {
                !t.exited
                    && matches!(t.wait, Some((Wait::Retry { .. }, _)))
                    && !matches!(t.ctx.x[8], 260 | 95)
            })
    }

    /// Frames of RAM in use: the pages of every process that has one, and their page tables.
    pub fn frames_in_use(&self) -> u64 {
        self.frames.borrow().live
    }

    /// Guest pages that hold anything: what of RAM a snapshot carries. (Reads all of RAM.)
    pub fn guest_pages(&self) -> u64 {
        (0..self.ram_size / PAGE)
            .filter(|&i| !self.ram.is_zero((i * PAGE) as usize, PAGE as usize))
            .count() as u64
    }

    // ------------------------------------------------------------ running

    /// Run the program for up to `budget` of wall time, or until every thread waits.
    pub fn run<H: Host>(&mut self, cpu: &mut Cpu<H>, budget: Duration) -> Status {
        let until = Instant::now() + budget;
        'run: loop {
            if let Some(c) = self.exit_code {
                self.net_exit();
                return Status::Exited(c);
            }
            if let Some(f) = &self.fault {
                return Status::Fault(f.clone());
            }
            if Instant::now() >= until {
                return Status::Running;
            }
            // Interval timers that are due raise SIGALRM
            self.fire_alarms();
            // timerfds that fired wake what waits on them
            self.timerfds_due();
            // Signals first: a call a handler interrupts ends rather than runs again
            self.deliver_signals(cpu);
            if self.exit_code.is_some() || self.fault.is_some() {
                continue;
            }
            self.compact();
            // (What a process that ended left for the host's side: its sockets)
            self.flush_net();
            self.wake();
            // The next runnable thread, round robin, starting with the current one
            let n = self.threads.len();
            let cur = self.cur;
            let Some(next) = (0..n)
                .map(|i| (cur + i) % n)
                .find(|&i| !self.threads[i].exited && self.threads[i].wait.is_none())
            else {
                if self.threads.is_empty() {
                    // Only the first process ends the machine (`end_process`), so this is a
                    // state that can't be
                    self.exit_code = Some(0);
                    continue;
                }
                return Status::Idle(
                    self.threads
                        .iter()
                        .filter_map(|t| t.wait.as_ref().and_then(|(_, d)| *d))
                        .chain(self.next_alarm())
                        .chain(self.next_timerfd())
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
            // Its process's address space (and descriptors)
            let pid = self.threads[next].pid;
            self.switch_to(pid);
            self.sync_mmu(cpu);

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
                        if esr >> 26 != 0x15 {
                            // A lazy page (filled: on), or a fault
                            if self.abort(cpu, esr, elr, far) {
                                continue;
                            }
                            continue 'run;
                        }
                        cpu.pc = elr;
                        let cur = self.cur;
                        self.resume_deadline = self.threads[cur].resume_deadline.take();
                        let (nr, a0, a1, a2) = (cpu.x[8], cpu.x[0], cpu.x[1], cpu.x[2]);
                        let ret = self.syscall_lazily(cpu, cur);
                        self.flush_net();
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
                                Ret::ThreadExit(c) => format!("thread exit {c}"),
                                Ret::GroupExit(c) => format!("exit {c}"),
                            };
                            eprintln!(
                                "[{}] sys {nr}({a0:#x}, {a1:#x}, {a2:#x}) = {r}",
                                self.threads.get(cur).map_or(0, |t| t.tid)
                            );
                        }
                        match ret {
                            Ret::Val(r) => {
                                cpu.x[0] = r as u64;
                                // (Its thread may be gone: its process ended)
                                if Instant::now() >= until
                                    || self.loaded != Some(cur)
                                    || self.exit_code.is_some()
                                    || self.fault.is_some()
                                    || self.signal_ready()
                                {
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
                            Ret::ThreadExit(code) => {
                                self.thread_exit(cur, code);
                                continue 'run;
                            },
                            Ret::GroupExit(code) => {
                                let pid = self.p.pid;
                                self.end_process(pid, proc::End::Exit(code));
                                continue 'run;
                            },
                        }
                    },
                    Exit::Stop(r) => {
                        self.crash(format!("stopped: {r:?}"));
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
        if self.net_changed() {
            self.poke();
        }
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

    /// Wakes up to `n` threads waiting on the futex `addr` (`futex_key`'s): of the process that
    /// runs, or of any process for a shared one (a frame's).
    pub(crate) fn futex_wake(&mut self, addr: u64, n: u64, mask: u32) -> i64 {
        let pid = self.p.pid;
        let shared = addr & mm::SHARED_FUTEX != 0;
        let mut woken = 0;
        for t in self.threads.iter_mut() {
            if woken >= n {
                break;
            }
            if t.pid != pid && !shared {
                continue;
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

    /// FUTEX_REQUEUE: wakes up to `nwake` threads waiting on `addr`, then moves up to `nrequeue`
    /// of the rest to wait on `addr2` (a thread keeps its deadline and mask). Returns the number
    /// of threads woken and the number moved.
    pub(crate) fn futex_requeue(
        &mut self,
        addr: u64,
        nwake: u64,
        addr2: u64,
        nrequeue: u64,
    ) -> (u64, u64) {
        let woken = self.futex_wake(addr, nwake, u32::MAX) as u64;
        let pid = self.p.pid;
        let shared = addr & mm::SHARED_FUTEX != 0;
        let mut moved = 0;
        for t in self.threads.iter_mut() {
            if moved >= nrequeue {
                break;
            }
            if t.pid != pid && !shared {
                continue;
            }
            if let Some((Wait::Futex { addr: a, .. }, _)) = &mut t.wait {
                if *a == addr {
                    *a = addr2;
                    moved += 1;
                }
            }
        }
        (woken, moved)
    }

    /// Takes the registers of the thread on the CPU back into the process, so that every thread
    /// is in the process's state and none in the CPU's.
    pub fn park<H: Host>(&mut self, cpu: &Cpu<H>) {
        if let Some(i) = self.loaded.take() {
            self.threads[i].ctx = Ctx::save(cpu);
        }
    }

    // ------------------------------------------------------------ snapshots

    /// The processes' state for a snapshot (the machine's own snapshot holds its memory and
    /// filesystem): every process (its address space, descriptors, signals) and thread, its
    /// waits as time left, the descriptions the descriptors refer to, the terminal, the clock.
    pub fn save<H: Host>(&mut self, cpu: &Cpu<H>) -> Vec<u8> {
        self.park(cpu);
        self.compact();
        let now = Instant::now();
        let mut w = W(MAGIC.to_vec());
        w.u(self.ram_size);
        self.frames.borrow_mut().save(&mut w);
        w.u(self.asids.get().0);
        w.u(self.filled.get());
        w.u(self.next_id as u64);
        w.u(self.now_ns(1));
        w.u(self.cur as u64);
        self.files.save(&mut w);
        self.tty.save(&mut w);
        // The address spaces, one each whoever shares it (an ended process has none)
        let mut spaces: Vec<&mm::SharedMm> = Vec::new();
        for p in self.proc_list().filter(|p| p.zombie.is_none()) {
            if !spaces.iter().any(|m| Rc::ptr_eq(m, &p.mm)) {
                spaces.push(&p.mm);
            }
        }
        w.u(spaces.len() as u64);
        for m in &spaces {
            m.borrow().save(&mut w);
        }
        // The processes (an ended one that nobody waits for is gone)
        let kept: Vec<&Proc> = self.proc_list().filter(|p| !p.reap).collect();
        w.u(kept.len() as u64);
        for p in kept {
            w.u(p.pid as u64);
            w.u(p.ppid as u64);
            w.u(p.pgid as u64);
            w.u(p.sid as u64);
            w.u(p.exit_signal as u64);
            w.u(p.zombie.is_some() as u64);
            w.u(p.zombie.unwrap_or(0) as u32 as u64);
            w.u(spaces
                .iter()
                .position(|m| Rc::ptr_eq(m, &p.mm))
                .map_or(u64::MAX, |i| i as u64));
            Files::save_fds(
                if p.pid == self.p.pid { &self.files.fds } else { &p.fds },
                &mut w,
            );
            w.u(p.cwd);
            w.u(p.umask as u64);
            w.b(&p.exe);
            p.sig.save(&mut w);
            p.creds.save(&mut w);
            match p.alarm {
                Some(a) => {
                    w.u(a.at.saturating_duration_since(now).as_nanos() as u64);
                    w.u(a.every.as_nanos() as u64);
                },
                None => w.u(u64::MAX),
            }
        }
        w.u(self.threads.len() as u64);
        for t in &self.threads {
            w.u(t.tid as u64);
            w.u(t.pid as u64);
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
                Some((Wait::Vfork(child), _)) => {
                    w.u(5);
                    w.u(*child as u64);
                },
            }
            w.u(left(t.wait.as_ref().and_then(|(_, d)| *d)));
            w.u(left(t.resume_deadline));
            t.sig.save(&mut w);
            t.sched.save(&mut w);
        }
        self.shm.save(&mut w);
        let pages = self.file_pages.borrow();
        w.u(pages.len() as u64);
        for (&(ino, off), &pa) in pages.iter() {
            w.u(ino);
            w.u(off);
            w.u(pa);
        }
        drop(pages);
        self.files.net.save_rights(&mut w);
        w.0
    }

    /// The process from its `save`, on a CPU the machine's snapshot was restored to (and the
    /// machine, for its filesystem).
    pub fn restore<H: Host>(
        cpu: &mut Cpu<H>,
        board: Option<Arc<Board>>,
        data: &[u8],
    ) -> Result<Process, String> {
        // Which format it is (MAGIC says what each adds): 5, 6 or 7
        let Some((ver, rest)) = MAGICS
            .iter()
            .enumerate()
            .find_map(|(i, m)| data.strip_prefix(&m[..]).map(|rest| (5 + i, rest)))
        else {
            return Err("not a process state".into());
        };
        let v6 = ver >= 6;
        let mut r = R(rest);
        let ram = cpu.mem.alias();
        let ram_size = r.u()?;
        if ram_size != ram.len() as u64 {
            return Err(format!(
                "the process had {ram_size} bytes of RAM, the machine {}",
                ram.len()
            ));
        }
        let machine = cpu.machine.clone();
        let purge = Box::new(move |page: u64| {
            let flag = &machine.code_pages[page as usize];
            if flag.load(Ordering::Relaxed) != 0 {
                flag.store(0, Ordering::Relaxed);
                machine.invalidate_code(page as u32, usize::MAX);
            }
        });
        let mut frames = mm::Frames::new(ram.alias(), purge);
        frames.load(&mut r)?;
        let _asid = r.u()?;
        let filled = r.u()?;
        let next_id = r.u()? as i64;
        let mono_base = r.u()?;
        let cur = r.u()? as usize;
        let mut files = Files::load(&mut r)?;
        let tty = Tty::load(&mut r)?;
        let mut spaces = Vec::new();
        for _ in 0..r.u()? {
            spaces.push(Rc::new(RefCell::new(mm::Mm::load(&mut r, ram_size)?)));
        }
        let mut all = BTreeMap::new();
        for _ in 0..r.u()? {
            let (pid, ppid, pgid, sid) =
                (r.u()? as i64, r.u()? as i64, r.u()? as i64, r.u()? as i64);
            let exit_signal = r.u()? as u32;
            let (ended, status) = (r.u()? != 0, r.u()? as u32 as i32);
            let space = r.u()?;
            let mm = match (ended, spaces.get(space as usize)) {
                (false, Some(m)) => Rc::clone(m),
                (true, _) => Rc::new(RefCell::new(mm::Mm::husk())),
                (false, None) => return Err("a process has no address space".into()),
            };
            let fds = Files::load_fds(&mut r)?;
            let (cwd, umask) = (r.u()?, r.u()? as u32);
            let exe = r.b()?.to_vec();
            let sig = Signals::load(&mut r)?;
            // Before ARMLUSR6 every process was root
            let creds = if v6 { Creds::load(&mut r)? } else { Creds::default() };
            let alarm = match v6.then(|| r.u()).transpose()? {
                Some(left) if left != u64::MAX => Some(itimer::Alarm {
                    at: Instant::now() + Duration::from_nanos(left),
                    every: Duration::from_nanos(r.u()?),
                }),
                _ => None,
            };
            all.insert(
                pid,
                Proc {
                    pid,
                    ppid,
                    pgid,
                    sid,
                    mm,
                    fds,
                    exit_signal,
                    zombie: ended.then_some(status),
                    reap: false,
                    cwd,
                    umask,
                    exe,
                    sig,
                    creds,
                    alarm,
                },
            );
        }
        let now = Instant::now();
        let at = |left: u64| (left != u64::MAX).then(|| now + Duration::from_nanos(left));
        let mut threads = Vec::new();
        for _ in 0..r.u()? {
            let (tid, pid, clear_tid, exited) = (r.u()? as i64, r.u()? as i64, r.u()?, r.u()? != 0);
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
                5 => Some(Wait::Vfork(r.u()? as i64)),
                t => return Err(format!("unknown wait {t}")),
            };
            let deadline = at(r.u()?);
            let resume_deadline = at(r.u()?);
            if !all.contains_key(&pid) {
                return Err(format!("a thread of process {pid}, which is not there"));
            }
            threads.push(Thread {
                tid,
                pid,
                ctx,
                clear_tid,
                wait: wait.map(|w| (w, deadline)),
                resume_deadline,
                exited,
                sig: ThreadSig::load(&mut r)?,
                sched: if ver >= 7 { sched::Sched::load(&mut r)? } else { sched::Sched::default() },
            });
        }
        // Before ARMLUSR6 there was no System V shared memory
        let shm = if v6 { shm::Shm::load(&mut r)? } else { shm::Shm::default() };
        let mut file_pages = BTreeMap::new();
        if v6 {
            for _ in 0..r.u()? {
                let (ino, off, pa) = (r.u()?, r.u()?, r.u()?);
                file_pages.insert((ino, off), pa);
            }
            files.net.load_rights(&mut r)?;
        }
        // The first process is the one that runs
        let mut first = all.remove(&INIT).ok_or("no first process")?;
        files.fds = std::mem::take(&mut first.fds);
        setup_cpu(cpu);
        let p = Process {
            ram,
            ram_size,
            board,
            frames: RefCell::new(frames),
            asids: Cell::new((1, 0)),
            need: RefCell::new(Vec::new()),
            filled: Cell::new(filled),
            file_pages: RefCell::new(file_pages),
            p: first,
            procs: all,
            threads,
            next_id,
            sig_dirty: true,
            files,
            tty,
            start: now,
            mono_base,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            // Whatever waited on blobs asks for them again: the host that restores may not have them
            poked: true,
            epoch: 0,
            cur,
            loaded: None,
            resume_deadline: None,
            trace: false,
            stats: Stats::default(),
            shm,
        };
        p.net_attach();
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
        p.sync_mmu(cpu);
        Ok(p)
    }
}

/// /dev/shm, where shm_open makes POSIX shared memory (glibc's and musl's): on Linux a tmpfs, here
/// a directory of the machine's filesystem anyone may write in, made if the image has none
/// (Supabase's Postgres keeps its dynamic shared memory there).
fn dev_shm(fs: &mut Fs) {
    use armless::dev::fs::S_IFDIR;
    let dev = match fs.lookup(ROOT, b"dev") {
        Ok(d) => d,
        Err(_) => match fs.create(ROOT, b"dev", S_IFDIR | 0o755, 0, 0, 0) {
            Ok(d) => d,
            Err(_) => return,
        },
    };
    if fs.lookup(dev, b"shm").is_err() {
        let _ = fs.create(dev, b"shm", S_IFDIR | 0o1777, 0, 0, 0);
    }
}

/// A whole file of the machine's filesystem, for exec (its blobs asked for when they aren't there).
pub(crate) fn read_all(board: &Board, ino: Ino) -> Result<Vec<u8>, ExecError> {
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

/// The process state's format: ARMLUSR7 (0.16) has each thread's scheduling (sched.rs); ARMLUSR6
/// (0.15) has none of that; ARMLUSR5 (0.14) has neither the ids, timers, shared memory, shared
/// file pages nor descriptors in flight of 0.15. Each still loads.
const MAGIC: &[u8; 8] = b"ARMLUSR7";
const MAGICS: [&[u8; 8]; 3] = [b"ARMLUSR5", b"ARMLUSR6", MAGIC];
