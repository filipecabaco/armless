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

/// Creates a machine for the native runner.
pub fn new_vm(config: crate::vm::Config) -> Result<(Vm<NativePlatform>, Arc<Clock>), String> {
    let clock = Clock::new();
    let c = clock.clone();
    let vm = Vm::new(config, crate::platform::unix_secs, move |_| {
        NativePlatform::new(c.clone())
    })?;
    vm.machine().host_cores.store(
        std::thread::available_parallelism().map_or(1, |n| n.get()) as u32,
        Ordering::Relaxed,
    );
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

    /// Start (or restart) a thread per CPU.
    pub fn start(&mut self) {
        if !self.threads.is_empty() {
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

    /// Restore a snapshot (raw or zstd); the CPUs must not be running yet,
    /// or are paused for it.
    pub fn load_state(&self, data: &[u8]) -> Result<(), String> {
        let mut p = self.pause();
        let mut cpus = p.cpus();
        crate::vm::load_state(&self.board, &mut cpus, data)
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
        loader.finish(&self.board, &mut cpus)
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
                finish(s);
                machine.request_stop(kind);
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
