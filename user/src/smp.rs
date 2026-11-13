//! A program on several CPUs. Each CPU runs on a host thread of its own (`Smp::serve`, and the
//! host's own thread for the CPU it drives with `Smp::run`), and runs the guest's threads at the
//! same time as the others: a thread's instructions run with no lock held. What a thread asks of
//! its kernel (a system call, a page it touches first, a fault) stops its CPU, which then takes
//! the process's lock and answers it with the same code as one CPU does (`Process::answer`), one
//! CPU at a time. Picking a CPU's next thread is under the lock too (`Process::schedule`).
//!
//! What one CPU does that others must see before it goes on:
//! - a change to an address space (unmapped, protected, copied on write, a fork): the
//!   translations of it every CPU holds go first (`Process::sync_mmu`, armless's broadcast TLB
//!   invalidation);
//! - a process that ends or execs: its threads on other CPUs stop there, and their CPUs leave the
//!   guest, before its memory is freed (`Process::take_off_cpus`);
//! - a signal for a thread another CPU runs: that CPU is stopped, and delivers it itself.
//!
//! A CPU with no thread to run waits on a condition variable that a CPU which makes one runnable
//! (or changes what waits look at) signals.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use armless::cpu::Cpu;
use armless::Host;

use crate::host::Instant;
use crate::{Process, Status, SLICE};

/// The longest an idle CPU waits before it looks again: what wakes a thread that nothing signals
/// (blobs arriving, the network) is looked at when a CPU schedules.
const IDLE_POLL: Duration = Duration::from_millis(5);

pub struct Smp<'a> {
    p: Mutex<&'a mut Process>,
    /// Signalled when there may be a thread for a CPU that has none
    work: Condvar,
    /// CPUs waiting on `work`
    idle: AtomicUsize,
    quit: AtomicBool,
    /// `pause`: the CPUs `serve` runs take their threads off and wait
    paused: AtomicBool,
    parked: Mutex<usize>,
    parked_cv: Condvar,
    /// Per CPU, ns: running guest code, waiting for the lock, holding it (`times`)
    times: Vec<[AtomicU64; 3]>,
}

impl<'a> Smp<'a> {
    pub fn new(p: &'a mut Process) -> Smp<'a> {
        let n = p.on_cpu.len();
        Smp {
            p: Mutex::new(p),
            work: Condvar::new(),
            idle: AtomicUsize::new(0),
            quit: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            parked: Mutex::new(0),
            parked_cv: Condvar::new(),
            times: (0..n).map(|_| Default::default()).collect(),
        }
    }

    /// Per CPU, the time (ns) it spent running guest code, waiting for the process's lock, and
    /// answering with it held: where a program's time goes on several CPUs.
    pub fn times(&self) -> Vec<[u64; 3]> {
        self.times
            .iter()
            .map(|t| [0, 1, 2].map(|k| t[k].load(Ordering::Relaxed)))
            .collect()
    }

    fn count(&self, i: usize, k: usize, since: std::time::Instant) -> std::time::Instant {
        let now = std::time::Instant::now();
        self.times[i][k].fetch_add((now - since).as_nanos() as u64, Ordering::Relaxed);
        now
    }

    /// The process, for the host (its input, its output, a signal, a snapshot). Threads it makes
    /// runnable are run once this is dropped: call `poke` after.
    pub fn lock(&self) -> MutexGuard<'_, &'a mut Process> {
        self.p.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// After the host changed something (input, a signal): CPUs with nothing to do look again.
    pub fn poke(&self) {
        let _g = self.lock();
        self.work.notify_all();
    }

    /// CPU `i`'s turn at the process: `f` answers for it. CPUs waiting for a thread are woken if
    /// it left one for them.
    fn answering<R>(&self, i: usize, f: impl FnOnce(&mut Process) -> R) -> R {
        let t = std::time::Instant::now();
        let mut g = self.lock();
        let t = self.count(i, 1, t);
        g.enter(i);
        let r = f(&mut g);
        g.leave(i);
        self.count(i, 2, t);
        let wake = self.idle.load(Ordering::SeqCst) > 0 && g.has_work();
        drop(g);
        if wake {
            self.work.notify_all();
        }
        r
    }

    /// Run CPU `i`'s share of the program for up to `budget` of wall time (as `Process::run` runs
    /// it all on one CPU). It is idle (`Status::Idle`) only when every CPU is: while others run,
    /// it waits for a thread to run until the budget is spent (`Status::Running`).
    pub fn run<H: Host>(&self, i: usize, cpu: &mut Cpu<H>, budget: Duration) -> Status {
        let until = Instant::now() + budget;
        cpu.left_guest();
        loop {
            if self.quit.load(Ordering::SeqCst) || self.paused.load(Ordering::SeqCst) && i != 0 {
                return Status::Running;
            }
            let t = std::time::Instant::now();
            let mut g = self.lock();
            let t = self.count(i, 1, t);
            g.enter(i);
            let sched = g.schedule(cpu, until);
            self.count(i, 2, t);
            let start = match sched {
                Ok(start) => start,
                Err(Status::Idle(wake)) => {
                    let busy = g.others_busy();
                    g.leave(i);
                    if !busy {
                        return Status::Idle(wake);
                    }
                    // Others run: what they do may give this CPU a thread
                    let now = Instant::now();
                    let to = wake.unwrap_or(until).min(until).min(now + IDLE_POLL);
                    self.idle.fetch_add(1, Ordering::SeqCst);
                    if !g.has_work() {
                        g = self
                            .work
                            .wait_timeout(g, to.saturating_duration_since(now))
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    self.idle.fetch_sub(1, Ordering::SeqCst);
                    drop(g);
                    continue;
                },
                Err(status) => {
                    g.leave(i);
                    return status;
                },
            };
            g.leave(i);
            let wake = self.idle.load(Ordering::SeqCst) > 0 && g.has_work();
            drop(g);
            if wake {
                self.work.notify_all();
            }
            loop {
                let used = cpu.insn_count - start;
                if used >= SLICE {
                    self.answering(i, |p| p.end_slice());
                    break;
                }
                let t = std::time::Instant::now();
                let exit = cpu.run(SLICE - used);
                cpu.left_guest();
                self.count(i, 0, t);
                if !self.answering(i, |p| p.answer(cpu, exit, until)) {
                    break;
                }
            }
        }
    }

    /// Run CPU `i` (not the host's, 0) until `stop`: on the host thread that calls this, which
    /// it keeps. (`cpu` is set up for the program here.)
    pub fn serve<H: Host>(&self, i: usize, cpu: &mut Cpu<H>) {
        crate::setup_cpu(cpu);
        while !self.quit.load(Ordering::SeqCst) {
            if self.paused.load(Ordering::SeqCst) {
                // Its thread goes back to the process, for a snapshot to have it
                self.answering(i, |p| p.park(cpu));
                let mut n = self.parked.lock().unwrap_or_else(|e| e.into_inner());
                *n += 1;
                self.parked_cv.notify_all();
                while self.paused.load(Ordering::SeqCst) && !self.quit.load(Ordering::SeqCst) {
                    n = self
                        .parked_cv
                        .wait_timeout(n, Duration::from_millis(50))
                        .unwrap_or_else(|e| e.into_inner())
                        .0;
                }
                *n -= 1;
                continue;
            }
            match self.run(i, cpu, Duration::from_millis(20)) {
                Status::Running => {},
                // Every CPU idles, or the program is over: wait to be told, or for a deadline
                status => {
                    let done = matches!(status, Status::Exited(_) | Status::Fault(_));
                    let g = self.lock();
                    let now = Instant::now();
                    let to = match status {
                        Status::Idle(Some(w)) => w.min(now + IDLE_POLL),
                        _ if done => now + Duration::from_millis(50),
                        _ => now + IDLE_POLL,
                    };
                    self.idle.fetch_add(1, Ordering::SeqCst);
                    if done || !g.has_work() {
                        drop(
                            self.work
                                .wait_timeout(g, to.saturating_duration_since(now))
                                .unwrap_or_else(|e| e.into_inner()),
                        );
                    }
                    self.idle.fetch_sub(1, Ordering::SeqCst);
                },
            }
        }
    }

    /// The CPUs `serve` runs (`n` of them) stop between slices, their threads back in the
    /// process: for a snapshot (`lock().save(cpu0)`), until `resume`.
    pub fn pause(&self, n: usize) {
        self.paused.store(true, Ordering::SeqCst);
        {
            let g = self.lock();
            for c in 1..g.on_cpu.len() {
                g.machine.preempt(c);
            }
        }
        self.work.notify_all();
        let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        while *parked < n {
            parked = self
                .parked_cv
                .wait_timeout(parked, Duration::from_millis(10))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        let _n = self.parked.lock().unwrap_or_else(|e| e.into_inner());
        self.parked_cv.notify_all();
    }

    /// The CPUs `serve` runs return.
    pub fn stop(&self) {
        self.quit.store(true, Ordering::SeqCst);
        {
            let g = self.lock();
            for c in 1..g.on_cpu.len() {
                g.machine.preempt(c);
            }
        }
        self.work.notify_all();
        self.parked_cv.notify_all();
    }
}

impl Process {
    /// `run` on several CPUs (`cpus`, all of the machine's): CPU 0 on this thread, the others on
    /// threads of their own for as long as this runs. Their threads are back in the process when
    /// it returns. (A host that runs the program for long keeps an `Smp` and its threads instead.)
    pub fn run_on<H: Host>(&mut self, cpus: &mut [Cpu<H>], budget: Duration) -> Status
    where
        Cpu<H>: Send,
    {
        let (first, others) = cpus.split_at_mut(1);
        if others.is_empty() {
            return self.run(&mut first[0], budget);
        }
        let n = others.len();
        let smp = Smp::new(self);
        std::thread::scope(|s| {
            for (i, c) in others.iter_mut().enumerate() {
                let smp = &smp;
                s.spawn(move || smp.serve(i + 1, c));
            }
            let status = smp.run(0, &mut first[0], budget);
            smp.pause(n);
            smp.stop();
            status
        })
    }

    /// CPU `i` answers (see `smp`): it is the one `cpu` and `cur` are of.
    pub(crate) fn enter(&mut self, i: usize) {
        self.cpu = i;
        self.cur = self.cpu_cur[i];
    }

    /// CPU `i` is done answering.
    pub(crate) fn leave(&mut self, i: usize) {
        self.cpu_cur[i] = self.cur;
        self.cpu = 0;
        self.cur = self.cpu_cur[0];
    }

    /// Whether a CPU other than the one answering has a thread on it
    pub(crate) fn others_busy(&self) -> bool {
        self.on_cpu
            .iter()
            .enumerate()
            .any(|(c, t)| c != self.cpu && t.is_some())
    }

    /// Whether a CPU with no thread may find one: a thread that can run and no CPU has, or
    /// something changed that parked calls or signals look at.
    pub(crate) fn has_work(&self) -> bool {
        self.poked
            || self.sig_dirty
            || self.exit_code.is_some()
            || self
                .threads
                .iter()
                .enumerate()
                .any(|(i, t)| !t.exited && t.wait.is_none() && !self.on_cpu.contains(&Some(i)))
    }
}
