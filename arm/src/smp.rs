//! State shared by all vCPUs of a machine, and the primitives they use to
//! signal each other.
//!
//! A machine always has at least one CPU; with one CPU nothing here is ever
//! contended. With several, each vCPU runs on its own host thread (native
//! threads, or Web Workers sharing wasm memory) and they meet here:
//!
//! - the GIC distributor and the RTC, behind spinlocks;
//! - per-CPU *attention* bytes: set when something may need the CPU's notice
//!   (an interrupt, a TLB shootdown, power changes). The run loop and
//!   JIT-compiled code poll the CPU's own byte;
//! - per-CPU wake counters, which an idle host thread waits on;
//! - PSCI power state for CPU_ON/CPU_OFF/AFFINITY_INFO;
//! - a TLB-invalidation generation for broadcast TLBI (see `Cpu::tlb_sync`);
//! - the generic timer epoch, so every CPU reads the same counter.

use std::cell::UnsafeCell;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::gic::Gic;
use crate::ram::Ram;
use crate::rtc::Rtc;

pub const MAX_CPUS: usize = 8;

/// A minimal spinlock: critical sections here are a few hundred instructions.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

unsafe impl<T: Send> Sync for SpinLock<T> {}
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub fn new(value: T) -> SpinLock<T> {
        SpinLock {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    #[inline]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            let mut spins = 0u32;
            while self.locked.load(Ordering::Relaxed) {
                spins += 1;
                if spins < 128 {
                    std::hint::spin_loop();
                } else {
                    // The holder's thread may have been descheduled.
                    #[cfg(not(target_arch = "wasm32"))]
                    std::thread::yield_now();
                }
            }
        }
        SpinGuard { lock: self }
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// PSCI power states (the values AFFINITY_INFO returns).
pub const POWER_ON: u32 = 0;
pub const POWER_OFF: u32 = 1;
pub const POWER_ON_PENDING: u32 = 2;

/// What a CPU's host thread is doing, for TLB shootdown waits.
pub const RUN_RUNNING: u32 = 0;
pub const RUN_IDLE: u32 = 1;

/// Machine-wide stop requests (any CPU may raise one; all CPUs stop).
pub const STOP_NONE: u32 = 0;
pub const STOP_POWER_OFF: u32 = 1;
pub const STOP_RESET: u32 = 2;

#[derive(Default)]
pub struct PerCpu {
    /// Non-zero: re-examine interrupts and other shared state before the next
    /// instruction. Polled by the run loop and by JIT-compiled code.
    pub attn: AtomicU8,
    /// Bumped on every kick; an idle host thread waits for it to change.
    pub wake: AtomicU32,
    pub power: AtomicU32,
    pub entry: AtomicU64,
    pub context: AtomicU64,
    /// Last TLB-invalidation generation this CPU has applied.
    pub tlb_seen: AtomicU64,
    pub run_state: AtomicU32,
}

/// Wakes an idle CPU's host thread after its `wake` counter changed.
pub type Waker = Box<dyn Fn(usize) + Send + Sync>;

pub struct Machine {
    pub ncpus: usize,
    /// Guest RAM; CPUs hold aliases of it.
    pub mem: Ram,
    pub gic: SpinLock<Gic>,
    pub rtc: SpinLock<Rtc>,
    pub cpus: [PerCpu; MAX_CPUS],
    /// Generic timer: the counter was `timer_cnt_base` at host time `timer_ns_base`.
    pub timer_cnt_base: AtomicU64,
    pub timer_ns_base: AtomicU64,
    /// Bumped by every broadcast TLB invalidation.
    pub tlb_gen: AtomicU64,
    pub stop: AtomicU32,
    /// Serialises STXP/LDXP-paired updates (no 128-bit atomics in wasm).
    pub pair_lock: SpinLock<()>,
    waker: SpinLock<Option<Waker>>,
}

impl Machine {
    pub fn new(memory_size: usize, ncpus: usize) -> Machine {
        assert!((1..=MAX_CPUS).contains(&ncpus), "1..={MAX_CPUS} CPUs");
        let m = Machine {
            ncpus,
            mem: Ram::zeroed(memory_size),
            gic: SpinLock::new(Gic::new(ncpus)),
            rtc: SpinLock::new(Rtc::default()),
            cpus: Default::default(),
            timer_cnt_base: AtomicU64::new(0),
            timer_ns_base: AtomicU64::new(0),
            tlb_gen: AtomicU64::new(0),
            stop: AtomicU32::new(STOP_NONE),
            pair_lock: SpinLock::new(()),
            waker: SpinLock::new(None),
        };
        // CPU 0 starts running; the others wait for PSCI CPU_ON.
        for (i, c) in m.cpus.iter().enumerate() {
            c.power
                .store(if i == 0 { POWER_ON } else { POWER_OFF }, Ordering::Relaxed);
            c.attn.store(1, Ordering::Relaxed);
        }
        m
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock() = Some(waker);
    }

    /// Ask CPU `cpu` to look at shared state, waking it if it is idle.
    pub fn kick(&self, cpu: usize) {
        let c = &self.cpus[cpu];
        c.attn.store(1, Ordering::SeqCst);
        c.wake.fetch_add(1, Ordering::SeqCst);
        if let Some(w) = self.waker.lock().as_ref() {
            w(cpu);
        }
    }

    /// Kick every CPU in `mask` except `except`.
    pub fn kick_mask(&self, mask: u32, except: usize) {
        for i in 0..self.ncpus {
            if mask & (1 << i) != 0 && i != except {
                self.kick(i);
            }
        }
    }

    pub fn all_cpus_mask(&self) -> u32 {
        (1u32 << self.ncpus) - 1
    }

    /// Request a machine-wide stop (power off or reset) and tell every CPU.
    pub fn request_stop(&self, kind: u32) {
        let _ = self
            .stop
            .compare_exchange(STOP_NONE, kind, Ordering::SeqCst, Ordering::SeqCst);
        for i in 0..self.ncpus {
            self.kick(i);
        }
    }
}
