//! Running a [`Vm`] natively: one thread per CPU, idle CPUs sleeping until
//! their timer or an interrupt, and a pause that stops every CPU between
//! instruction slices (for snapshots).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::board::BoardHost;
use crate::cpu::{Cpu, Exit, StopReason};
use crate::platform::{Clock, NativePlatform};
use crate::smp;
use crate::vm::{Board, Vm};

pub type NativeCpu = Cpu<BoardHost<NativePlatform>>;

const SLICE: u64 = 100_000;

/// Why the machine stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Stopped {
    PowerOff,
    Reset,
    /// Halt instruction with an exit code (bare-metal tests).
    Halt(u64),
    /// Something the guest did that the machine cannot continue from.
    Fault(String),
}

struct Ctl {
    paused: AtomicBool,
    parked: Mutex<usize>,
    resume: Condvar,
    quit: AtomicBool,
    stopped: Mutex<Option<Stopped>>,
    stopped_cv: Condvar,
    sleepers: Vec<(Mutex<()>, Condvar)>,
    /// Skip ahead in time while the (only) CPU idles.
    skip_idle: bool,
}

pub struct Runner {
    pub board: Arc<Board>,
    pub clock: Arc<Clock>,
    cpus: Vec<Arc<Mutex<NativeCpu>>>,
    ctl: Arc<Ctl>,
    threads: Vec<JoinHandle<()>>,
}

/// Fresh randomness from the host's own generator, to seed a machine with (`Board::seed`).
pub fn os_entropy() -> [u8; 32] {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    let read = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if read.is_err() {
        // No /dev/urandom: the clock and this process, which at least differ between runs
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mix = t ^ (u128::from(std::process::id()) << 64);
        bytes[..16].copy_from_slice(&mix.to_le_bytes());
    }
    bytes
}

/// Creates a machine for the native runner, seeded with the host's randomness.
pub fn new_vm(config: crate::vm::Config) -> Result<(Vm<NativePlatform>, Arc<Clock>), String> {
    new_vm_on(config, None)
}

/// `new_vm`, on `ram` when there is one (`Ram::from_image`, say).
pub fn new_vm_on(
    config: crate::vm::Config,
    ram: Option<crate::ram::Ram>,
) -> Result<(Vm<NativePlatform>, Arc<Clock>), String> {
    let clock = Clock::new();
    let c = clock.clone();
    let platform = move |_| NativePlatform::new(c.clone());
    let vm = match ram {
        Some(ram) => Vm::with_ram(config, ram, crate::platform::unix_secs, platform)?,
        None => Vm::new(config, crate::platform::unix_secs, platform)?,
    };
    vm.board.seed(&os_entropy());
    vm.machine().host_cores.store(
        std::thread::available_parallelism().map_or(1, |n| n.get()) as u32,
        Ordering::Relaxed,
    );
    // ARMLESS_HV=1 runs the CPUs in hardware (Hypervisor.framework, KVM), where the process can;
    // where it can't, they run as ARMLESS_JIT says.
    #[cfg(armless_hw)]
    let vm = {
        let mut vm = vm;
        if std::env::var("ARMLESS_HV").is_ok_and(|v| v == "1") {
            // Which GIC the guest gets (the boot, after this, describes it)
            let all = crate::cpu::hypervisor_probe().and_then(|gic_v3| {
                vm.machine().gic_v3.store(gic_v3, Ordering::Relaxed);
                vm.cpus.iter_mut().try_for_each(|cpu| cpu.enable_hypervisor())
            });
            match all {
                Ok(()) => return Ok((vm, clock)),
                Err(e) => {
                    vm.machine().gic_v3.store(false, Ordering::Relaxed);
                    eprintln!("armless: {e}; not running in hardware");
                },
            }
        }
        vm
    };
    // ARMLESS_JIT=native compiles hot guest code to host code (where the host supports it).
    #[cfg(feature = "native-jit")]
    if std::env::var("ARMLESS_JIT").is_ok_and(|v| v == "native") {
        let mut vm = vm;
        for cpu in vm.cpus.iter_mut() {
            cpu.enable_native_jit();
        }
        return Ok((vm, clock));
    }
    Ok((vm, clock))
}

/// While held, every CPU is stopped.
pub struct Paused<'a> {
    runner: &'a Runner,
    guards: Vec<MutexGuard<'a, NativeCpu>>,
}

impl Paused<'_> {
    pub fn cpus(&mut self) -> Vec<&mut NativeCpu> {
        self.guards.iter_mut().map(|g| &mut **g).collect()
    }
}

impl Drop for Paused<'_> {
    fn drop(&mut self) {
        self.guards.clear();
        let ctl = &self.runner.ctl;
        let _g = ctl.parked.lock().unwrap();
        ctl.paused.store(false, Ordering::SeqCst);
        ctl.resume.notify_all();
    }
}

impl Runner {
    /// Take the machine's CPUs. `skip_idle` fast-forwards time while a
    /// single-CPU guest has nothing to do (tests).
    pub fn new(vm: Vm<NativePlatform>, clock: Arc<Clock>, skip_idle: bool) -> Runner {
        let n = vm.cpus.len();
        let ctl = Arc::new(Ctl {
            paused: AtomicBool::new(false),
            parked: Mutex::new(0),
            resume: Condvar::new(),
            quit: AtomicBool::new(false),
            stopped: Mutex::new(None),
            stopped_cv: Condvar::new(),
            sleepers: (0..n).map(|_| (Mutex::new(()), Condvar::new())).collect(),
            skip_idle: skip_idle && n == 1,
        });
        {
            let ctl = ctl.clone();
            vm.machine().set_waker(Box::new(move |cpu| {
                let (m, cv) = &ctl.sleepers[cpu];
                let _g = m.lock().unwrap();
                cv.notify_all();
            }));
        }
        Runner {
            board: vm.board.clone(),
            clock,
            cpus: vm
                .cpus
                .into_iter()
                .map(|c| Arc::new(Mutex::new(c)))
                .collect(),
            ctl,
            threads: Vec::new(),
        }
    }

    /// Start (or restart) a thread per CPU. The threads of a machine that stopped wait for it to
    /// be restored, and go on then: a stop from before, when the machine runs again, is not one.
    pub fn start(&mut self) {
        if !self.threads.is_empty() {
            self.restored();
            return;
        }
        self.ctl.quit.store(false, Ordering::SeqCst);
        *self.ctl.stopped.lock().unwrap() = None;
        for (i, cpu) in self.cpus.iter().enumerate() {
            let cpu = cpu.clone();
            let ctl = self.ctl.clone();
            let board = self.board.clone();
            let clock = self.clock.clone();
            self.threads.push(
                std::thread::Builder::new()
                    .name(format!("vcpu{i}"))
                    .spawn(move || vcpu_thread(i, cpu, ctl, board, clock))
                    .expect("spawn vCPU thread"),
            );
        }
    }

    /// Stop every CPU between instruction slices.
    pub fn pause(&self) -> Paused<'_> {
        self.ctl.paused.store(true, Ordering::SeqCst);
        for i in 0..self.cpus.len() {
            self.board.machine().kick(i);
        }
        let guards = self.cpus.iter().map(|c| c.lock().unwrap()).collect();
        Paused {
            runner: self,
            guards,
        }
    }

    pub fn save_state(&self) -> Vec<u8> {
        let mut p = self.pause();
        let mut cpus = p.cpus();
        crate::vm::save_state(&self.board, &mut cpus)
    }

    /// Snapshot for a machine of the first `n` CPUs; the others must be offline in the guest.
    pub fn save_state_cpus(&self, n: usize) -> Result<Vec<u8>, String> {
        let mut p = self.pause();
        let mut cpus = p.cpus();
        crate::vm::save_state_cpus(&self.board, &mut cpus, n)
    }

    /// The stop `wait` gives dropped, unless the machine is still stopped: a restore runs it
    /// again. (A CPU that stops says so to the machine before it says so here, so a stop that
    /// the machine doesn't have is one from before.)
    fn restored(&self) {
        let mut s = self.ctl.stopped.lock().unwrap();
        if self.board.machine().stop.load(Ordering::SeqCst) == smp::STOP_NONE {
            *s = None;
        }
    }

    /// Restore a snapshot (raw or zstd); the CPUs must not be running yet,
    /// or are paused for it. The machine is seeded afresh, as after every restore (`Board::seed`):
    /// otherwise each restore of a snapshot would draw the same "random" numbers.
    pub fn load_state(&self, data: &[u8]) -> Result<(), String> {
        let mut p = self.pause();
        let mut cpus = p.cpus();
        crate::vm::load_state(&self.board, &mut cpus, data)?;
        self.board.seed(&os_entropy());
        self.restored();
        Ok(())
    }

    /// Restore a snapshot streamed from `reader` (decompressed bytes).
    pub fn load_state_from(&self, reader: &mut dyn std::io::Read) -> Result<(), String> {
        let mut p = self.pause();
        let mut cpus = p.cpus();
        let mut loader = crate::vm::StateLoader::new(&self.board);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            loader.feed(&self.board, &buf[..n])?;
        }
        loader.finish(&self.board, &mut cpus)?;
        self.board.seed(&os_entropy());
        self.restored();
        Ok(())
    }

    /// Wait until the machine stops (or `timeout` passes).
    pub fn wait(&self, timeout: Option<Duration>) -> Option<Stopped> {
        let deadline = timeout.map(|t| Instant::now() + t);
        let mut s = self.ctl.stopped.lock().unwrap();
        while s.is_none() {
            match deadline {
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    s = self.ctl.stopped_cv.wait_timeout(s, left).unwrap().0;
                },
                None => s = self.ctl.stopped_cv.wait(s).unwrap(),
            }
        }
        s.clone()
    }

    pub fn stopped(&self) -> Option<Stopped> {
        self.ctl.stopped.lock().unwrap().clone()
    }

    /// Stop the CPU threads and wait for them.
    pub fn shutdown(&mut self) {
        self.ctl.quit.store(true, Ordering::SeqCst);
        {
            let _g = self.ctl.parked.lock().unwrap();
            self.ctl.resume.notify_all();
        }
        for i in 0..self.cpus.len() {
            self.board.machine().kick(i);
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }

    /// Instructions executed so far by every CPU.
    pub fn insn_count(&self) -> u64 {
        self.cpus.iter().map(|c| c.lock().unwrap().insn_count).sum()
    }

    /// The code every CPU has compiled (see `Cpu::jit_profile`), one entry per page.
    pub fn jit_profile(&self) -> Vec<crate::jit::profile::Page> {
        let p = self.pause();
        let mut pages = Vec::new();
        for c in &p.guards {
            crate::jit::profile::merge(&mut pages, &c.jit_profile(), usize::MAX);
        }
        // Several CPUs compiling the same page is still one run
        for page in &mut pages {
            page.runs = 1;
        }
        pages
    }

    /// Drop every CPU's compiled code, as a restore does: what is compiled from here on is what
    /// a restored machine would compile doing the same.
    pub fn jit_flush(&self) {
        let mut p = self.pause();
        for c in p.cpus() {
            c.jit.flush_all();
        }
    }

    /// Have every CPU compile the entry points of a profile now (see `Cpu::jit_prewarm`); the
    /// compilers work through them while the machine runs. Returns the modules queued.
    pub fn jit_prewarm(&self, pages: &[crate::jit::profile::Page]) -> usize {
        let mut p = self.pause();
        p.cpus().into_iter().map(|c| c.jit_prewarm(pages)).sum()
    }

    /// Run `f` on a CPU (e.g. to read registers); pauses that CPU.
    pub fn with_cpu<T>(&self, i: usize, f: impl FnOnce(&mut NativeCpu) -> T) -> T {
        let mut p = self.pause();
        f(&mut p.guards[i])
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn vcpu_thread(
    i: usize,
    cpu: Arc<Mutex<NativeCpu>>,
    ctl: Arc<Ctl>,
    board: Arc<Board>,
    clock: Arc<Clock>,
) {
    let machine = board.machine().clone();
    let me = &machine.cpus[i];
    let finish = |s: Stopped| {
        let mut st = ctl.stopped.lock().unwrap();
        if st.is_none() {
            *st = Some(s);
        }
        ctl.stopped_cv.notify_all();
    };
    loop {
        if ctl.quit.load(Ordering::SeqCst) {
            // A CPU waiting in `tlb_sync` for this one's acknowledgement would wait forever (and
            // `shutdown` for it): it waits only for CPUs that are running.
            me.run_state.store(smp::RUN_IDLE, Ordering::SeqCst);
            let c = cpu.lock().unwrap();
            if let Some(s) = c.jit.inval_stats.as_ref() {
                eprint!("{}", s.report(i));
                let (all, compiled) = (c.insn_count, c.jit.block_insns);
                eprintln!(
                    "jit-inval: cpu{i} instructions {:.0}M, compiled {:.0}M ({:.1}%), of them handed to the interpreter {:.1}M; {} block runs ({:.1} instructions each)",
                    all as f64 / 1e6,
                    compiled as f64 / 1e6,
                    100.0 * compiled as f64 / all.max(1) as f64,
                    c.jit.helper_interps as f64 / 1e6,
                    c.jit.block_runs,
                    compiled as f64 / c.jit.block_runs.max(1) as f64
                );
                let e = c.jit.exits;
                eprintln!(
                    "jit-inval: cpu{i} block runs right after a block run {:.1}%; ended done {}, redirected {}, exception {}, wfi {}",
                    100.0 * c.jit.block_to_block as f64 / c.jit.block_runs.max(1) as f64,
                    e[0], e[1], e[2], e[3]
                );
                if i == 0 {
                    eprintln!(
                        "jit-inval: machine IC invalidations recorded {}, of them DMA {}",
                        machine.stats[2].load(Ordering::Relaxed),
                        machine.dma_code_invals.load(Ordering::Relaxed)
                    );
                }
            }
            return;
        }
        if ctl.paused.load(Ordering::SeqCst) {
            // Others must not wait for this CPU's TLB acknowledgements.
            me.run_state.store(smp::RUN_IDLE, Ordering::SeqCst);
            let mut parked = ctl.parked.lock().unwrap();
            *parked += 1;
            while ctl.paused.load(Ordering::SeqCst) && !ctl.quit.load(Ordering::SeqCst) {
                parked = ctl.resume.wait(parked).unwrap();
            }
            *parked -= 1;
            continue;
        }
        let woken = me.wake.load(Ordering::SeqCst);
        let exit = {
            let mut c = cpu.lock().unwrap();
            c.run(SLICE)
        };
        match exit {
            Exit::Budget => {},
            Exit::Idle { wake_ns } => {
                if ctl.skip_idle && board.io.out.is_empty() {
                    if let Some(w) = wake_ns {
                        clock.advance_to(w);
                        continue;
                    }
                }
                let (m, cv) = &ctl.sleepers[i];
                let mut g = m.lock().unwrap();
                let wait = match wake_ns {
                    Some(w) => Duration::from_nanos(w.saturating_sub(clock.now_ns())),
                    None => Duration::from_millis(100),
                };
                let deadline = Instant::now() + wait;
                while me.wake.load(Ordering::SeqCst) == woken
                    && !ctl.paused.load(Ordering::SeqCst)
                    && !ctl.quit.load(Ordering::SeqCst)
                {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    g = cv.wait_timeout(g, left).unwrap().0;
                }
            },
            Exit::Stop(reason) => {
                let (s, kind) = match reason {
                    StopReason::PowerOff => (Stopped::PowerOff, smp::STOP_POWER_OFF),
                    StopReason::Reset => (Stopped::Reset, smp::STOP_RESET),
                    StopReason::Halt(c) => (Stopped::Halt(c as u64), smp::STOP_POWER_OFF),
                    other => {
                        let c = cpu.lock().unwrap();
                        (
                            Stopped::Fault(format!(
                                "cpu {i}: {other:x?} at pc={:#x} el={} esr={:#x} far={:#x}",
                                c.pc, c.el, c.sys.esr, c.sys.far
                            )),
                            smp::STOP_POWER_OFF,
                        )
                    },
                };
                // The machine first, for `restored` to tell a stop from one before a restore
                machine.request_stop(kind);
                finish(s);
                me.run_state.store(smp::RUN_IDLE, Ordering::SeqCst);
                // Stay parked until shut down (or restarted after a load).
                let mut parked = ctl.parked.lock().unwrap();
                while !ctl.quit.load(Ordering::SeqCst)
                    && machine.stop.load(Ordering::SeqCst) != smp::STOP_NONE
                {
                    parked = ctl
                        .resume
                        .wait_timeout(parked, Duration::from_millis(100))
                        .unwrap()
                        .0;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_restore_gets_fresh_randomness() {
        let config = crate::vm::Config {
            memory_size: 16 << 20,
            ..Default::default()
        };
        let (vm, clock) = new_vm(config).unwrap();
        let runner = Runner::new(vm, clock, false);
        let booted = runner.board.devices.lock().vmgenid;
        assert_ne!(booted, [0; 16], "a new machine is seeded");
        let state = runner.save_state();
        let mut ids = Vec::new();
        for _ in 0..2 {
            runner.load_state(&state).unwrap();
            ids.push(runner.board.devices.lock().vmgenid);
        }
        assert_ne!(ids[0], booted);
        assert_ne!(
            ids[0], ids[1],
            "each restore of the same snapshot has its own generation"
        );
    }

    #[test]
    fn a_restored_machine_has_not_stopped_until_it_stops_again() {
        let config = crate::vm::Config {
            memory_size: 16 << 20,
            ..Default::default()
        };
        let (vm, clock) = new_vm(config).unwrap();
        let mut runner = Runner::new(vm, clock, false);
        // HLT #1 at 0x1000, where the CPU starts; `b .` at 0x2000
        runner.with_cpu(0, |c| {
            c.mem.write(0x1000, &(0xd440_0000u32 | 1 << 5).to_le_bytes());
            c.mem.write(0x2000, &0x1400_0000u32.to_le_bytes());
            c.pc = 0x1000;
        });
        runner.start();
        let stop = runner.wait(Some(Duration::from_secs(5)));
        assert!(matches!(stop, Some(Stopped::Halt(1))), "{stop:?}");
        let state = runner.save_state();
        // Restored, it runs on (from the loop): no stop to wait for, the old one least of all
        runner.load_state(&state).unwrap();
        runner.with_cpu(0, |c| c.pc = 0x2000);
        runner.start();
        let stop = runner.wait(Some(Duration::from_millis(200)));
        assert!(stop.is_none(), "{stop:?}");
        assert!(runner.stopped().is_none());
    }
}
