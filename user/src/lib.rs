//! User mode for armless: one static aarch64 Linux ELF on an armless CPU with no guest kernel.
//! The CPU runs at EL0 with the MMU off and no vector table, so every exception the program
//! takes stops the CPU (`StopReason::UnhandledException`) and is answered here: syscalls by a
//! small Linux personality, faults by stopping the process.
//!
//! An experiment, to measure what a kernel-less machine costs and saves. Left out on purpose
//! for now: page tables (VA = PA, so no PROT_NONE, no nil-pointer faults, no fork), file
//! systems (every open is ENOENT), sockets, signal delivery (handlers are recorded, signals
//! dropped), and more than one vCPU (guest threads take turns on one CPU).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use armless::cpu::{Cpu, Exit, StopReason};
use armless::{Host, Ram};

const PAGE: u64 = 4096;
/// Instructions a guest thread runs before the next one gets the CPU.
const SLICE: u64 = 1 << 20;
const NCPUS_REPORTED: u64 = 4;
const STACK_SIZE: u64 = 8 << 20;
const BRK_SIZE: u64 = 64 << 20;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const ENODEV: i64 = 19;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const ENOSYS: i64 = 38;
const ETIMEDOUT: i64 = 110;

// ---------------------------------------------------------------- guest threads

#[derive(Clone)]
struct Ctx {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    nzcv: [bool; 4],
    v: [u128; 32],
    fpcr: u64,
    fpsr: u64,
    tpidr: u64,
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

#[derive(Clone, Copy, Debug)]
enum Wait {
    Futex { addr: u64, mask: u32 },
    Sleep,
    Epoll { ep: i32, out: u64, max: u32 },
    Forever,
    Stdin { buf: u64, len: u64 },
}

struct Thread {
    tid: i64,
    ctx: Ctx,
    clear_tid: u64,
    wait: Option<(Wait, Option<Instant>)>,
    exited: bool,
}

enum Fd {
    Stdin,
    Out(i32),
    Event { count: u64, nonblock: bool },
    Epoll { watch: BTreeMap<i32, (u32, u64)> },
}

/// What a syscall does to the thread that made it.
enum Ret {
    Val(i64),
    Block(Wait, Option<Instant>),
    Yield,
    ThreadExit,
    GroupExit(i32),
}
/// A program's process: its threads, memory map, descriptors and clock. A snapshot carries it
/// (`save`, `restore`) next to the machine's own state, which holds its memory and the CPU.
pub struct Process {
    ram: Ram,
    ram_size: u64,
    brk_start: u64,
    brk: u64,
    mmap_floor: u64,
    mmap_top: u64,
    /// Free ranges of the mmap area, start -> end, coalesced. Freed pages are zeroed, so
    /// anything handed out from here reads as zero.
    free: BTreeMap<u64, u64>,
    threads: Vec<Thread>,
    next_tid: i64,
    fds: BTreeMap<i32, Fd>,
    sigactions: BTreeMap<u64, [u8; 32]>,
    /// The monotonic clock reads `mono_base` ns at `start`, and goes on from there across a
    /// snapshot and its restores.
    start: Instant,
    mono_base: u64,
    stdin: VecDeque<u8>,
    stdin_closed: bool,
    output: Vec<u8>,
    exit_code: Option<i32>,
    fault: Option<String>,
    /// The thread whose turn it is, and the one whose registers are on the CPU.
    cur: usize,
    loaded: Option<usize>,
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

fn page_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

impl Process {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        addr.checked_add(buf.len() as u64)
            .is_some_and(|end| end <= self.ram_size)
            && self.ram.read(addr as usize, buf)
    }

    fn write(&self, addr: u64, data: &[u8]) -> bool {
        addr.checked_add(data.len() as u64)
            .is_some_and(|end| end <= self.ram_size)
            && self.ram.write(addr as usize, data)
    }

    fn u32_at(&self, addr: u64) -> Option<u32> {
        let mut b = [0; 4];
        self.read(addr, &mut b).then(|| u32::from_le_bytes(b))
    }

    fn u64_at(&self, addr: u64) -> Option<u64> {
        let mut b = [0; 8];
        self.read(addr, &mut b).then(|| u64::from_le_bytes(b))
    }

    fn put_u64(&self, addr: u64, v: u64) -> bool {
        self.write(addr, &v.to_le_bytes())
    }

    fn cstr(&self, mut addr: u64) -> String {
        let mut s = Vec::new();
        let mut b = [0u8];
        while s.len() < 4096 && self.read(addr, &mut b) && b[0] != 0 {
            s.push(b[0]);
            addr += 1;
        }
        String::from_utf8_lossy(&s).into_owned()
    }

    /// Zero a range the program gave back, and the host memory under it with it.
    fn zero_range(&self, addr: u64, len: u64) {
        let start = addr & !(PAGE - 1);
        let end = page_up(addr + len).min(self.ram_size);
        if start < end {
            self.ram.discard(start as usize, (end - start) as usize);
        }
    }

    /// Highest free range of `len` bytes in the mmap area.
    fn alloc(&mut self, len: u64) -> Option<u64> {
        let (&s, &e) = self.free.iter().rev().find(|(&s, &e)| e - s >= len)?;
        let at = e - len;
        self.free.remove(&s);
        if at > s {
            self.free.insert(s, at);
        }
        Some(at)
    }

    /// Take `[a, b)` out of the free list (MAP_FIXED over it).
    fn carve(&mut self, a: u64, b: u64) {
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

    /// Give `[a, b)` back to the free list, zeroed, merged with its neighbours.
    fn release(&mut self, a: u64, b: u64) {
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

    fn now_ns(&self, clock: u64) -> u64 {
        match clock {
            // CLOCK_REALTIME, CLOCK_REALTIME_COARSE
            0 | 5 => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64),
            // Monotonic: from a second before the program started, on across snapshots
            _ => self.mono_base + self.start.elapsed().as_nanos() as u64,
        }
    }

    fn timespec(&self, addr: u64) -> Option<Duration> {
        let s = self.u64_at(addr)?;
        let ns = self.u64_at(addr + 8)?;
        Some(Duration::new(s, ns as u32))
    }

    /// The host instant at which `clock` reads `abs`.
    fn deadline_at(&self, clock: u64, abs: Duration) -> Instant {
        let now = self.now_ns(clock);
        let target = abs.as_nanos() as u64;
        Instant::now() + Duration::from_nanos(target.saturating_sub(now))
    }

    fn stdout(&mut self, _fd: i32, data: &[u8]) {
        self.output.extend_from_slice(data);
    }

    /// Complete the reads of stdin that wait, now that there is input (or its end).
    fn wake_stdin(&mut self) {
        for i in 0..self.threads.len() {
            let Some((Wait::Stdin { buf, len }, _)) = self.threads[i].wait else {
                continue;
            };
            if self.stdin.is_empty() && !self.stdin_closed {
                return;
            }
            let n = (len as usize).min(self.stdin.len());
            let data: Vec<u8> = self.stdin.drain(..n).collect();
            let r = if self.write(buf, &data) { n as i64 } else { -EFAULT };
            self.threads[i].ctx.x[0] = r as u64;
            self.threads[i].wait = None;
        }
    }

    // ------------------------------------------------------------ futex and epoll

    fn futex_wake(&mut self, addr: u64, n: u64, mask: u32) -> i64 {
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

    fn epoll_ready(&self, ep: i32) -> Vec<(u32, u64)> {
        let Some(Fd::Epoll { watch }) = self.fds.get(&ep) else {
            return Vec::new();
        };
        let mut ready = Vec::new();
        for (fd, &(events, data)) in watch {
            let mut got = 0;
            match self.fds.get(fd) {
                Some(Fd::Event { count, .. }) => {
                    if *count > 0 {
                        got |= events & 1; // EPOLLIN
                    }
                    got |= events & 4; // EPOLLOUT
                },
                Some(Fd::Out(_)) => got |= events & 4,
                _ => {},
            }
            if got != 0 {
                ready.push((got, data));
            }
        }
        ready
    }

    fn epoll_deliver(&self, out: u64, max: u32, ready: &[(u32, u64)]) -> i64 {
        let n = ready.len().min(max as usize);
        for (i, &(ev, data)) in ready[..n].iter().enumerate() {
            // struct epoll_event on arm64: u32 events, 4 bytes of padding, u64 data
            let at = out + 16 * i as u64;
            self.write(at, &ev.to_le_bytes());
            self.put_u64(at + 8, data);
        }
        n as i64
    }

    /// After an fd became readable: complete the epoll waits it satisfies.
    fn wake_epoll_waiters(&mut self) {
        for i in 0..self.threads.len() {
            if let Some((Wait::Epoll { ep, out, max }, _)) = self.threads[i].wait {
                let ready = self.epoll_ready(ep);
                if !ready.is_empty() {
                    let n = self.epoll_deliver(out, max, &ready);
                    self.threads[i].ctx.x[0] = n as u64;
                    self.threads[i].wait = None;
                }
            }
        }
    }

    fn new_fd(&mut self, fd: Fd) -> i64 {
        let n = (3..).find(|n| !self.fds.contains_key(n)).unwrap();
        self.fds.insert(n, fd);
        n as i64
    }

    // ------------------------------------------------------------ syscalls

    fn syscall<H: Host>(&mut self, cpu: &mut Cpu<H>, cur: usize) -> Ret {
        let nr = cpu.x[8];
        let a = [cpu.x[0], cpu.x[1], cpu.x[2], cpu.x[3], cpu.x[4], cpu.x[5]];
        *self.stats.syscalls.entry(nr).or_default() += 1;
        let tid = self.threads[cur].tid;
        let v = |r: i64| Ret::Val(r);
        match nr {
            // read: stdin waits for input
            63 if matches!(self.fds.get(&(a[0] as i32)), Some(Fd::Stdin)) => {
                if self.stdin.is_empty() {
                    return if self.stdin_closed {
                        v(0)
                    } else {
                        Ret::Block(
                            Wait::Stdin {
                                buf: a[1],
                                len: a[2],
                            },
                            None,
                        )
                    };
                }
                let n = (a[2] as usize).min(self.stdin.len());
                let data: Vec<u8> = self.stdin.drain(..n).collect();
                v(if self.write(a[1], &data) { n as i64 } else { -EFAULT })
            },
            63 => match self.fds.get_mut(&(a[0] as i32)) {
                Some(Fd::Event { count, nonblock }) => {
                    if a[2] < 8 {
                        return v(-EINVAL);
                    }
                    if *count == 0 {
                        return if *nonblock {
                            v(-EAGAIN)
                        } else {
                            Ret::Block(Wait::Forever, None)
                        };
                    }
                    let c = std::mem::take(count);
                    v(if self.put_u64(a[1], c) { 8 } else { -EFAULT })
                },
                _ => v(-EBADF),
            },
            // write
            64 => {
                let fd = a[0] as i32;
                let mut buf = vec![0; a[2] as usize];
                if !self.read(a[1], &mut buf) {
                    return v(-EFAULT);
                }
                match self.fds.get_mut(&fd) {
                    Some(Fd::Out(_)) => {
                        self.stdout(fd, &buf);
                        v(buf.len() as i64)
                    },
                    Some(Fd::Event { count, .. }) if buf.len() >= 8 => {
                        *count += u64::from_le_bytes(buf[..8].try_into().unwrap());
                        self.wake_epoll_waiters();
                        v(8)
                    },
                    _ => v(-EBADF),
                }
            },
            // writev
            66 => {
                let fd = a[0] as i32;
                if !matches!(self.fds.get(&fd), Some(Fd::Out(_))) {
                    return v(-EBADF);
                }
                let mut all = Vec::new();
                for i in 0..a[2] {
                    let (Some(base), Some(len)) =
                        (self.u64_at(a[1] + 16 * i), self.u64_at(a[1] + 16 * i + 8))
                    else {
                        return v(-EFAULT);
                    };
                    let mut b = vec![0; len as usize];
                    if !self.read(base, &mut b) {
                        return v(-EFAULT);
                    }
                    all.extend_from_slice(&b);
                }
                self.stdout(fd, &all);
                v(all.len() as i64)
            },
            // close
            57 => v(if self.fds.remove(&(a[0] as i32)).is_some() { 0 } else { -EBADF }),
            // openat
            56 => {
                let path = self.cstr(a[1]);
                self.stats.opens.push(path);
                v(-ENOENT)
            },
            // faccessat, readlinkat, newfstatat, statx, faccessat2
            48 | 78 | 79 | 291 | 439 => v(-ENOENT),
            // fstat: the standard fds are character devices
            80 => {
                if !matches!(self.fds.get(&(a[0] as i32)), Some(Fd::Stdin | Fd::Out(_))) {
                    return v(-EBADF);
                }
                let mut st = [0u8; 128];
                st[16..20].copy_from_slice(&0o20620u32.to_le_bytes());
                v(if self.write(a[1], &st) { 0 } else { -EFAULT })
            },
            // lseek
            62 => v(-ESPIPE),
            // ioctl
            29 => v(-ENOTTY),
            // fcntl
            25 => match (self.fds.get(&(a[0] as i32)), a[1]) {
                (None, _) => v(-EBADF),
                (Some(Fd::Event { nonblock: true, .. }), 3) => v(0o4002),
                (Some(_), 3) => v(2),
                (Some(_), 1 | 2 | 4) => v(0),
                _ => v(-EINVAL),
            },
            // eventfd2
            19 => v(self.new_fd(Fd::Event {
                count: a[0],
                nonblock: a[1] & 0o4000 != 0,
            })),
            // epoll_create1
            20 => v(self.new_fd(Fd::Epoll {
                watch: BTreeMap::new(),
            })),
            // epoll_ctl
            21 => {
                let (ev, data) = (
                    self.u32_at(a[3]).unwrap_or(0),
                    self.u64_at(a[3] + 8).unwrap_or(0),
                );
                let Some(Fd::Epoll { watch }) = self.fds.get_mut(&(a[0] as i32)) else {
                    return v(-EBADF);
                };
                match a[1] {
                    1 | 3 => {
                        watch.insert(a[2] as i32, (ev, data));
                    },
                    2 => {
                        watch.remove(&(a[2] as i32));
                    },
                    _ => return v(-EINVAL),
                }
                v(0)
            },
            // epoll_pwait
            22 => {
                let ep = a[0] as i32;
                let ready = self.epoll_ready(ep);
                if !ready.is_empty() {
                    return v(self.epoll_deliver(a[1], a[2] as u32, &ready));
                }
                let timeout = a[3] as i32;
                if timeout == 0 {
                    return v(0);
                }
                let deadline =
                    (timeout > 0).then(|| Instant::now() + Duration::from_millis(timeout as u64));
                Ret::Block(
                    Wait::Epoll {
                        ep,
                        out: a[1],
                        max: a[2] as u32,
                    },
                    deadline,
                )
            },
            // pipe2 and sockets: not in this experiment
            59 | 198 => v(-ENOSYS),

            // ---------------------------------------------------- memory
            // brk
            214 => {
                if a[0] >= self.brk_start && a[0] <= self.brk_start + BRK_SIZE {
                    if a[0] < self.brk {
                        self.zero_range(a[0], self.brk - a[0]);
                    }
                    self.brk = a[0];
                }
                v(self.brk as i64)
            },
            // mmap
            222 => {
                let len = page_up(a[1]);
                let flags = a[3];
                if len == 0 {
                    return v(-EINVAL);
                }
                if flags & 0x20 == 0 {
                    return v(-ENODEV); // file mappings: not in this experiment
                }
                if flags & (0x10 | 0x10_0000) != 0 {
                    // MAP_FIXED, MAP_FIXED_NOREPLACE: VA is PA, so anywhere in RAM
                    if !a[0].is_multiple_of(PAGE) || a[0] + len > self.ram_size {
                        return v(-ENOMEM);
                    }
                    self.zero_range(a[0], len);
                    self.carve(a[0], a[0] + len);
                    return v(a[0] as i64);
                }
                // Hints are ignored: the highest free range that fits
                match self.alloc(len) {
                    Some(at) => v(at as i64),
                    None => v(-ENOMEM),
                }
            },
            // munmap
            215 => {
                self.release(a[0] & !(PAGE - 1), page_up(a[0] + a[1]));
                v(0)
            },
            // mprotect, msync, mlock: no page tables to change
            226..=228 => v(0),
            // madvise: DONTNEED and FREE give zero pages back
            233 => {
                if a[2] == 4 || a[2] == 8 {
                    self.zero_range(a[0], page_up(a[1]));
                }
                v(0)
            },
            // mremap
            216 => v(-ENOMEM),

            // ---------------------------------------------------- threads
            // clone
            220 => {
                let flags = a[0];
                const CLONE_VM: u64 = 0x100;
                const CLONE_THREAD: u64 = 0x10000;
                if flags & (CLONE_VM | CLONE_THREAD) != CLONE_VM | CLONE_THREAD {
                    return v(-ENOSYS); // fork: needs page tables
                }
                let new_tid = self.next_tid;
                self.next_tid += 1;
                let mut ctx = Ctx::save(cpu);
                ctx.x[0] = 0;
                if a[1] != 0 {
                    ctx.sp = a[1];
                }
                if flags & 0x80000 != 0 {
                    ctx.tpidr = a[3]; // CLONE_SETTLS
                }
                if flags & 0x10_0000 != 0 {
                    self.write(a[2], &(new_tid as u32).to_le_bytes()); // CLONE_PARENT_SETTID
                }
                if flags & 0x100_0000 != 0 {
                    self.write(a[4], &(new_tid as u32).to_le_bytes()); // CLONE_CHILD_SETTID
                }
                let clear_tid = if flags & 0x20_0000 != 0 { a[4] } else { 0 };
                self.threads.push(Thread {
                    tid: new_tid,
                    ctx,
                    clear_tid,
                    wait: None,
                    exited: false,
                });
                v(new_tid)
            },
            // clone3: callers fall back to clone
            435 => v(-ENOSYS),
            // futex
            98 => {
                let (addr, op, val) = (a[0], a[1] as u32, a[2]);
                let realtime = op & 256 != 0;
                match op & 0x7f {
                    0 | 9 => {
                        let Some(cur_val) = self.u32_at(addr) else {
                            return v(-EFAULT);
                        };
                        if cur_val != val as u32 {
                            return v(-EAGAIN);
                        }
                        let mask = if op & 0x7f == 9 { a[5] as u32 } else { u32::MAX };
                        let deadline = if a[3] == 0 {
                            None
                        } else if op & 0x7f == 0 {
                            self.timespec(a[3]).map(|d| Instant::now() + d)
                        } else {
                            self.timespec(a[3])
                                .map(|t| self.deadline_at(if realtime { 0 } else { 1 }, t))
                        };
                        Ret::Block(Wait::Futex { addr, mask }, deadline)
                    },
                    1 => v(self.futex_wake(addr, val, u32::MAX)),
                    10 => v(self.futex_wake(addr, val, a[5] as u32)),
                    _ => v(-ENOSYS),
                }
            },
            // set_tid_address
            96 => {
                self.threads[cur].clear_tid = a[0];
                v(tid)
            },
            // set_robust_list, prctl, membarrier
            99 | 167 | 283 => v(0),
            // rseq: glibc and others fall back when it is missing
            293 => v(-ENOSYS),
            // sched_yield
            124 => Ret::Yield,
            // sched_getaffinity
            123 => {
                if a[1] < 8 {
                    return v(-EINVAL);
                }
                v(if self.put_u64(a[2], (1 << NCPUS_REPORTED) - 1) { 8 } else { -EFAULT })
            },
            // sched_setaffinity, setpriority and the like
            122 | 140 => v(0),
            // exit
            93 => Ret::ThreadExit,
            // exit_group
            94 => Ret::GroupExit(a[0] as i32),
            // gettid, getpid, getppid, getuid, geteuid, getgid, getegid
            178 => v(tid),
            172 => v(1000),
            173 => v(1),
            174..=177 => v(0),

            // ---------------------------------------------------- signals (recorded, never delivered)
            // rt_sigaction
            134 => {
                let sig = a[0];
                if a[2] != 0 {
                    let old = self.sigactions.get(&sig).copied().unwrap_or([0; 32]);
                    self.write(a[2], &old);
                }
                if a[1] != 0 {
                    let mut act = [0; 32];
                    self.read(a[1], &mut act);
                    self.sigactions.insert(sig, act);
                }
                v(0)
            },
            // rt_sigprocmask
            135 => {
                if a[2] != 0 {
                    self.put_u64(a[2], 0);
                }
                v(0)
            },
            // sigaltstack: report none set
            132 => {
                if a[1] != 0 {
                    let mut ss = [0u8; 24];
                    ss[8..12].copy_from_slice(&2u32.to_le_bytes()); // SS_DISABLE
                    self.write(a[1], &ss);
                }
                v(0)
            },
            // kill, tkill, tgkill: dropped (Go's SIGURG preemption among them)
            129..=131 => {
                self.stats.signals_dropped += 1;
                v(0)
            },
            // rt_sigreturn: no frame was ever built
            139 => v(-EPERM),

            // ---------------------------------------------------- time and the rest
            // clock_gettime
            113 => {
                let ns = self.now_ns(a[0]);
                let ok = self.put_u64(a[1], ns / 1_000_000_000)
                    && self.put_u64(a[1] + 8, ns % 1_000_000_000);
                v(if ok { 0 } else { -EFAULT })
            },
            // clock_getres
            114 => {
                if a[1] != 0 {
                    self.put_u64(a[1], 0);
                    self.put_u64(a[1] + 8, 1);
                }
                v(0)
            },
            // nanosleep
            101 => match self.timespec(a[0]) {
                Some(d) => Ret::Block(Wait::Sleep, Some(Instant::now() + d)),
                None => v(-EFAULT),
            },
            // clock_nanosleep
            115 => match self.timespec(a[2]) {
                Some(d) if a[1] & 1 != 0 => {
                    Ret::Block(Wait::Sleep, Some(self.deadline_at(a[0], d)))
                },
                Some(d) => Ret::Block(Wait::Sleep, Some(Instant::now() + d)),
                None => v(-EFAULT),
            },
            // getrandom
            278 => {
                let bytes = armless::native::random_bytes(a[1] as usize);
                v(if self.write(a[0], &bytes) { a[1] as i64 } else { -EFAULT })
            },
            // prlimit64
            261 => {
                if a[3] != 0 {
                    let (cur, max) = match a[1] {
                        3 => (STACK_SIZE, u64::MAX),
                        7 => (1024, 4096),
                        _ => (u64::MAX, u64::MAX),
                    };
                    self.put_u64(a[3], cur);
                    self.put_u64(a[3] + 8, max);
                }
                v(0)
            },
            // uname
            160 => {
                let mut u = [0u8; 65 * 6];
                for (i, s) in [
                    "Linux",
                    "armless",
                    "6.6.0-armless-user",
                    "#1",
                    "aarch64",
                    "",
                ]
                .iter()
                .enumerate()
                {
                    u[65 * i..65 * i + s.len()].copy_from_slice(s.as_bytes());
                }
                v(if self.write(a[0], &u) { 0 } else { -EFAULT })
            },
            _ => {
                *self.stats.unknown.entry(nr).or_default() += 1;
                v(-ENOSYS)
            },
        }
    }
}

/// What a run of the process ended with.
#[derive(Debug)]
pub enum Status {
    /// Its time ran out: run it again.
    Running,
    /// Every thread waits: until this instant, or (None) for input only.
    Idle(Option<Instant>),
    Exited(i32),
    Fault(String),
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
    /// Load a static ELF into the CPU's RAM and set up its first thread, as `execve` would.
    pub fn exec<H: Host>(
        cpu: &mut Cpu<H>,
        elf: &[u8],
        args: &[String],
        env: &[String],
    ) -> Result<Process, String> {
        let ram = cpu.mem.alias();
        let ram_size = ram.len() as u64;
        let loaded = load_elf(&ram, ram_size, elf)?;
        let stack_top = ram_size - PAGE;
        let brk_start = page_up(loaded.end);
        let mmap_top = stack_top - STACK_SIZE - PAGE;
        if brk_start + BRK_SIZE >= mmap_top {
            return Err("RAM too small for the program".into());
        }
        let mut p = Process {
            ram,
            ram_size,
            brk_start,
            brk: brk_start,
            mmap_floor: brk_start + BRK_SIZE,
            mmap_top,
            free: BTreeMap::from([(brk_start + BRK_SIZE, mmap_top)]),
            threads: Vec::new(),
            next_tid: 1001,
            fds: BTreeMap::from([(0, Fd::Stdin), (1, Fd::Out(1)), (2, Fd::Out(2))]),
            sigactions: BTreeMap::new(),
            start: Instant::now(),
            mono_base: 1_000_000_000,
            stdin: VecDeque::new(),
            stdin_closed: false,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            cur: 0,
            loaded: None,
            trace: false,
            stats: Stats::default(),
        };
        let sp = build_stack(&p, stack_top, args, env, &loaded);
        setup_cpu(cpu);
        let mut first = Ctx::save(cpu);
        first.x = [0; 31];
        first.v = [0; 32];
        (first.fpcr, first.fpsr, first.tpidr) = (0, 0, 0);
        first.sp = sp;
        first.pc = loaded.entry;
        p.threads.push(Thread {
            tid: 1000,
            ctx: first,
            clear_tid: 0,
            wait: None,
            exited: false,
        });
        Ok(p)
    }

    /// Bytes for the program's stdin.
    pub fn input(&mut self, data: &[u8]) {
        self.stdin.extend(data);
        self.wake_stdin();
    }

    /// The end of stdin: reads past what is left return 0.
    pub fn close_input(&mut self) {
        self.stdin_closed = true;
        self.wake_stdin();
    }

    /// What the program wrote to stdout and stderr since the last call.
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
            // Waits whose deadline passed
            let now = Instant::now();
            for t in self.threads.iter_mut() {
                if let Some((w, Some(d))) = t.wait {
                    if d <= now {
                        t.ctx.x[0] = match w {
                            Wait::Futex { .. } => (-ETIMEDOUT) as u64,
                            _ => 0,
                        };
                        t.wait = None;
                    }
                }
            }
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
                        .filter_map(|t| t.wait.and_then(|(_, d)| d))
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
                        let (nr, a0, a1, a2) = (cpu.x[8], cpu.x[0], cpu.x[1], cpu.x[2]);
                        let ret = self.syscall(cpu, cur);
                        if self.trace {
                            let r = match &ret {
                                Ret::Val(r) => format!("{r:#x}"),
                                Ret::Block(w, d) => format!(
                                    "block {w:?} {:?}",
                                    d.map(|d| d.saturating_duration_since(Instant::now()))
                                ),
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
                                if Instant::now() >= until {
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

    /// Takes the registers of the thread on the CPU back into the process, so that every thread
    /// is in the process's state and none in the CPU's.
    pub fn park<H: Host>(&mut self, cpu: &Cpu<H>) {
        if let Some(i) = self.loaded.take() {
            self.threads[i].ctx = Ctx::save(cpu);
        }
    }

    /// The process's state for a snapshot (the machine's own snapshot holds its memory): every
    /// thread, its waits as time left, the memory map, the descriptors, the clock.
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
            let (tag, a, b, cc) = match t.wait.map(|(w, _)| w) {
                None => (0, 0, 0, 0),
                Some(Wait::Futex { addr, mask }) => (1, addr, mask as u64, 0),
                Some(Wait::Sleep) => (2, 0, 0, 0),
                Some(Wait::Epoll { ep, out, max }) => (3, ep as u64, out, max as u64),
                Some(Wait::Forever) => (4, 0, 0, 0),
                Some(Wait::Stdin { buf, len }) => (5, buf, len, 0),
            };
            for v in [tag, a, b, cc] {
                w.u(v);
            }
            w.u(t.wait.and_then(|(_, d)| d).map_or(u64::MAX, |d| {
                d.saturating_duration_since(now).as_nanos() as u64
            }));
        }
        w.u(self.fds.len() as u64);
        for (&n, fd) in &self.fds {
            w.u(n as u64);
            match fd {
                Fd::Stdin => w.u(0),
                Fd::Out(o) => {
                    w.u(1);
                    w.u(*o as u64);
                },
                Fd::Event { count, nonblock } => {
                    w.u(2);
                    w.u(*count);
                    w.u(*nonblock as u64);
                },
                Fd::Epoll { watch } => {
                    w.u(3);
                    w.u(watch.len() as u64);
                    for (&f, &(ev, data)) in watch {
                        w.u(f as u64);
                        w.u(ev as u64);
                        w.u(data);
                    }
                },
            }
        }
        w.u(self.sigactions.len() as u64);
        for (&sig, act) in &self.sigactions {
            w.u(sig);
            w.b(act);
        }
        w.b(&self.stdin.iter().copied().collect::<Vec<u8>>());
        w.u(self.stdin_closed as u64);
        w.0
    }

    /// The process from its `save`, on a CPU the machine's snapshot was restored to.
    pub fn restore<H: Host>(cpu: &mut Cpu<H>, data: &[u8]) -> Result<Process, String> {
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
            let (tag, a, b, c) = (r.u()?, r.u()?, r.u()?, r.u()?);
            let left = r.u()?;
            let deadline = (left != u64::MAX).then(|| now + Duration::from_nanos(left));
            let wait = match tag {
                0 => None,
                1 => Some(Wait::Futex {
                    addr: a,
                    mask: b as u32,
                }),
                2 => Some(Wait::Sleep),
                3 => Some(Wait::Epoll {
                    ep: a as i32,
                    out: b,
                    max: c as u32,
                }),
                4 => Some(Wait::Forever),
                5 => Some(Wait::Stdin { buf: a, len: b }),
                t => return Err(format!("unknown wait {t}")),
            };
            threads.push(Thread {
                tid,
                ctx,
                clear_tid,
                wait: wait.map(|w| (w, deadline)),
                exited,
            });
        }
        let mut fds = BTreeMap::new();
        for _ in 0..r.u()? {
            let n = r.u()? as i32;
            let fd = match r.u()? {
                0 => Fd::Stdin,
                1 => Fd::Out(r.u()? as i32),
                2 => Fd::Event {
                    count: r.u()?,
                    nonblock: r.u()? != 0,
                },
                3 => {
                    let mut watch = BTreeMap::new();
                    for _ in 0..r.u()? {
                        watch.insert(r.u()? as i32, (r.u()? as u32, r.u()?));
                    }
                    Fd::Epoll { watch }
                },
                t => return Err(format!("unknown descriptor {t}")),
            };
            fds.insert(n, fd);
        }
        let mut sigactions = BTreeMap::new();
        for _ in 0..r.u()? {
            let sig = r.u()?;
            sigactions.insert(sig, r.b()?.try_into().map_err(|_| "bad sigaction")?);
        }
        let stdin = r.b()?.iter().copied().collect();
        let stdin_closed = r.u()? != 0;
        setup_cpu(cpu);
        Ok(Process {
            ram,
            ram_size,
            brk_start,
            brk,
            mmap_floor,
            mmap_top,
            free,
            threads,
            next_tid,
            fds,
            sigactions,
            start: now,
            mono_base,
            stdin,
            stdin_closed,
            output: Vec::new(),
            exit_code: None,
            fault: None,
            cur,
            loaded: None,
            trace: false,
            stats: Stats::default(),
        })
    }
}

const MAGIC: &[u8; 8] = b"ARMLUSR1";

struct W(Vec<u8>);

impl W {
    fn u(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }

    fn b(&mut self, d: &[u8]) {
        self.u(d.len() as u64);
        self.0.extend_from_slice(d);
    }
}

struct R<'a>(&'a [u8]);

impl<'a> R<'a> {
    fn u(&mut self) -> Result<u64, String> {
        let (v, rest) = self
            .0
            .split_first_chunk::<8>()
            .ok_or("truncated process state")?;
        self.0 = rest;
        Ok(u64::from_le_bytes(*v))
    }

    fn b(&mut self) -> Result<&'a [u8], String> {
        let n = self.u()? as usize;
        let (v, rest) = self
            .0
            .split_at_checked(n)
            .ok_or("truncated process state")?;
        self.0 = rest;
        Ok(v)
    }
}

// ---------------------------------------------------------------- loading

struct Loaded {
    entry: u64,
    phdr: u64,
    phnum: u64,
    end: u64,
}

fn load_elf(ram: &Ram, ram_size: u64, elf: &[u8]) -> Result<Loaded, String> {
    let u16_at = |o: usize| u16::from_le_bytes(elf[o..o + 2].try_into().unwrap());
    let u32_at = |o: usize| u32::from_le_bytes(elf[o..o + 4].try_into().unwrap());
    let u64_at = |o: usize| u64::from_le_bytes(elf[o..o + 8].try_into().unwrap());
    if elf.len() < 64 || &elf[..4] != b"\x7fELF" || elf[4] != 2 || elf[5] != 1 || u16_at(18) != 183
    {
        return Err("not a 64-bit little-endian aarch64 ELF".into());
    }
    let base = match u16_at(16) {
        2 => 0,
        3 => 0x10_0000, // static PIE: anywhere will do
        t => return Err(format!("ELF type {t} is not an executable")),
    };
    let (phoff, phentsize, phnum) = (
        u64_at(32) as usize,
        u16_at(54) as usize,
        u16_at(56) as usize,
    );
    let mut phdr = 0;
    let mut end = 0;
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let (ty, off, vaddr, filesz, memsz) = (
            u32_at(p),
            u64_at(p + 8),
            u64_at(p + 16),
            u64_at(p + 32),
            u64_at(p + 40),
        );
        match ty {
            3 => return Err("dynamically linked (PT_INTERP): not in this experiment".into()),
            6 => phdr = base + vaddr,
            1 => {
                let at = base + vaddr;
                if at + memsz > ram_size {
                    return Err(format!("segment at {at:#x} is past RAM"));
                }
                let data = elf
                    .get(off as usize..(off + filesz) as usize)
                    .ok_or("truncated ELF")?;
                ram.write(at as usize, data);
                if phdr == 0 && off == 0 {
                    phdr = at + phoff as u64;
                }
                end = end.max(at + memsz);
            },
            _ => {},
        }
    }
    Ok(Loaded {
        entry: base + u64_at(24),
        phdr,
        phnum: phnum as u64,
        end,
    })
}

/// argc, argv, envp and auxv at the top of the stack, as Linux lays them out.
fn build_stack(p: &Process, top: u64, args: &[String], env: &[String], l: &Loaded) -> u64 {
    let mut sp = top;
    let mut put_str = |s: &str| {
        sp -= s.len() as u64 + 1;
        p.write(sp, s.as_bytes());
        p.write(sp + s.len() as u64, &[0]);
        sp
    };
    let argv: Vec<u64> = args.iter().map(|s| put_str(s)).collect();
    let envp: Vec<u64> = env.iter().map(|s| put_str(s)).collect();
    let platform = put_str("aarch64");
    sp -= 16;
    let random = sp;
    p.write(random, &armless::native::random_bytes(16));

    // FP | ASIMD | AES | PMULL | SHA1 | SHA2 | CRC32 | ATOMICS (from what armless implements)
    let hwcap = 0x1fb;
    let auxv: &[(u64, u64)] = &[
        (3, l.phdr),
        (4, 56),
        (5, l.phnum),
        (6, PAGE),
        (7, 0),
        (8, 0),
        (9, l.entry),
        (11, 0),
        (12, 0),
        (13, 0),
        (14, 0),
        (15, platform),
        (16, hwcap),
        (17, 100),
        (23, 0),
        (25, random),
        (26, 0),
        (31, argv[0]),
        (51, 5120),
        (0, 0),
    ];
    let words = 1 + argv.len() + 1 + envp.len() + 1 + 2 * auxv.len();
    sp &= !15;
    if words % 2 == 1 {
        sp -= 8;
    }
    sp -= 8 * words as u64;
    let mut at = sp;
    let mut push = |w: u64| {
        p.put_u64(at, w);
        at += 8;
    };
    push(argv.len() as u64);
    argv.iter().for_each(|&a| push(a));
    push(0);
    envp.iter().for_each(|&e| push(e));
    push(0);
    for &(k, val) in auxv {
        push(k);
        push(val);
    }
    sp
}
