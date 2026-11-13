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

/// `n` flags, all 0, allocated zeroed: a table sized by guest memory costs nothing until a flag in one of its
/// pages is written.
#[allow(unsafe_code)]
fn zeroed_flags(n: usize) -> Vec<AtomicU8> {
    let mut v = std::mem::ManuallyDrop::new(vec![0u8; n]);
    // SAFETY: `AtomicU8` has the same size, alignment and bit validity as `u8` (as std documents), so the
    // buffer, its length and its capacity are valid for a `Vec<AtomicU8>`, which now owns the allocation.
    unsafe { Vec::from_raw_parts(v.as_mut_ptr() as *mut AtomicU8, v.len(), v.capacity()) }
}

pub const MAX_CPUS: usize = 8;

/// A minimal spinlock: critical sections here are a few hundred instructions.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through a guard, and `locked` lets one guard exist at a time
// (acquire on lock, release on unlock), as for a Mutex.
#[allow(unsafe_code)]
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: owning the lock is owning the value.
#[allow(unsafe_code)]
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
        let mut contended = false;
        let mut spins = 0u64;
        let mut yields = 0u64;
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            contended = true;
            let mut n = 0u32;
            while self.locked.load(Ordering::Relaxed) {
                n += 1;
                spins += 1;
                if n < 128 {
                    std::hint::spin_loop();
                } else if pause_thread() {
                    // The holder's thread may have been descheduled: let it run.
                    yields += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
        }
        if contended {
            LOCK_WAITS.record(spins, yields, 0);
        }
        SpinGuard { lock: self }
    }
}

// ------------------------------------------------------------------ waits

/// The kinds of wait one CPU does for another (`Machine::waits`); a `SpinLock` held by another
/// thread is counted in `LOCK_WAITS`.
pub const WAIT_TLB: usize = 0;
pub const WAIT_WFE: usize = 1;
pub const WAIT_KINDS: usize = 2;

/// What one kind of wait cost: how often a CPU had to wait, how long it spun and slept, and for
/// how long in all. Only waits that didn't end at once are counted.
pub struct WaitStats {
    pub waits: AtomicU64,
    /// Polls of what it waited for.
    pub spins: AtomicU64,
    /// Times it gave up its host thread so the CPU it waited for could run.
    pub yields: AtomicU64,
    /// Nanoseconds waited, and the longest wait (where the waiter has a clock: not a `SpinLock`).
    pub ns: AtomicU64,
    pub max_ns: AtomicU64,
}

impl WaitStats {
    pub const fn new() -> WaitStats {
        WaitStats {
            waits: AtomicU64::new(0),
            spins: AtomicU64::new(0),
            yields: AtomicU64::new(0),
            ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// One wait: `spins` polls, `yields` times the thread given up, `ns` long.
    pub fn record(&self, spins: u64, yields: u64, ns: u64) {
        self.waits.fetch_add(1, Ordering::Relaxed);
        self.spins.fetch_add(spins, Ordering::Relaxed);
        self.yields.fetch_add(yields, Ordering::Relaxed);
        self.ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    /// As JSON: {"waits":…,"spins":…,"yields":…,"ms":…,"max_ms":…}.
    pub fn json(&self) -> String {
        let ms = |a: &AtomicU64| a.load(Ordering::Relaxed) as f64 / 1e6;
        format!(
            "{{\"waits\":{},\"spins\":{},\"yields\":{},\"ms\":{:.3},\"max_ms\":{:.3}}}",
            self.waits.load(Ordering::Relaxed),
            self.spins.load(Ordering::Relaxed),
            self.yields.load(Ordering::Relaxed),
            ms(&self.ns),
            ms(&self.max_ns),
        )
    }
}

impl Default for WaitStats {
    fn default() -> Self {
        WaitStats::new()
    }
}

/// Waits for a `SpinLock` that another thread held. (Process-wide: a lock knows no machine.)
pub static LOCK_WAITS: WaitStats = WaitStats::new();

/// How long a CPU spins on a wait before it gives up its host thread now and then (`POLICY_ADAPTIVE`).
pub const SPIN_NS: u64 = 50_000;
/// A TLB wait this long means the CPU waited for had no host core for a while… (Only a TLB wait
/// says so: a CPU waits there for others to run, while a guest may wait in WFE for seconds on
/// purpose, polling for work.)
pub const SLOW_WAIT_NS: u64 = 2_000_000;
/// …and after this many such waits the machine counts as oversubscribed, whatever the host says.
pub const SLOW_WAITS_TO_YIELD: u32 = 3;

/// How waiting CPUs behave (`Machine::policy`): spin for SPIN_NS, then give up the host thread
/// now and then; learn from slow waits that the host has fewer cores free than it says.
pub const POLICY_ADAPTIVE: u32 = 0;
/// Spin a set number of times, more when the host reports more cores than vCPUs (armless 0.12).
#[cfg_attr(not(any(feature = "wasm-api", test)), allow(dead_code))]
pub const POLICY_SPIN_COUNT: u32 = 1;

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
std::thread_local! {
    /// This thread may block (a vCPU worker): a browser's page thread may not.
    static MAY_BLOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This thread is a vCPU's (a Web Worker): it may sleep in `pause_thread`.
#[cfg_attr(not(feature = "wasm-api"), allow(dead_code))]
pub fn set_may_block() {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    MAY_BLOCK.with(|b| b.set(true));
}

/// Give up the host thread for a moment, so another vCPU's thread can run: true if it did. A
/// native thread yields; a vCPU worker sleeps ~20 µs; a browser page's thread can't, so it doesn't.
pub fn pause_thread() -> bool {
    #[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
    {
        if !MAY_BLOCK.with(|b| b.get()) {
            return false;
        }
        static NEVER: AtomicU32 = AtomicU32::new(0);
        // SAFETY: waits on a static atomic nobody notifies, for at most 20 µs.
        #[allow(unsafe_code)]
        unsafe {
            core::arch::wasm32::memory_atomic_wait32(NEVER.as_ptr() as *mut i32, 0, 20_000);
        }
        true
    }
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    {
        false
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::yield_now();
        true
    }
}

pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;
    #[allow(unsafe_code)]
    fn deref(&self) -> &T {
        // SAFETY: this guard holds the lock, so nothing else reaches the value
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    #[allow(unsafe_code)]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as in `deref`, and `&mut self` makes this the only reference through the guard
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
    /// The CPU's hardware vCPU plus one, when it runs in hardware (`cpu::hv`): a kick makes it leave the
    /// guest. 0: none.
    pub hv_vcpu: AtomicU64,
}

/// Instruction-cache invalidations kept for CPUs that have not applied them yet. A CPU further
/// behind can't know which pages changed and drops all its compiled code, so this is sized for
/// guests whose own JIT writes code on one CPU while another runs (a JVM invalidated over 1800
/// pages ahead of its other CPU; 256 meant a full flush every few seconds).
pub const CODE_RING: usize = 16384;
pub const TLB_RING: usize = 256;
pub const TLB_ALL: u64 = u64::MAX;
pub const CODE_ALL: u32 = u32::MAX;

pub struct CodeInval {
    /// Invalidations so far; entry `n` lives at `ring[n % CODE_RING]`.
    pub gen: u64,
    pub ring: Box<[u32]>,
}

impl Default for CodeInval {
    fn default() -> Self {
        CodeInval {
            gen: 0,
            ring: vec![0; CODE_RING].into_boxed_slice(),
        }
    }
}

/// Wakes an idle CPU's host thread after its `wake` counter changed.
pub type Waker = Box<dyn Fn(usize) + Send + Sync>;

pub struct Machine {
    pub ncpus: usize,
    /// Guest RAM; CPUs hold aliases of it.
    pub mem: Ram,
    pub gic: SpinLock<Gic>,
    /// The guest has a GICv3 that the host's KVM keeps (`cpu::kvm`), not `gic`: set before a boot,
    /// which describes it in the device tree.
    pub gic_v3: AtomicBool,
    /// Where device lines go instead of `gic` (the in-kernel GICv3 of `cpu::kvm`), once set.
    pub irq_sink: std::sync::OnceLock<Box<dyn Fn(u32, bool) + Send + Sync>>,
    /// The guest has state: a CPU ran, or a snapshot was restored (`Board::seed` tells it of a new
    /// generation only then).
    pub started: AtomicBool,
    /// A snapshot was restored into the machine (its guest booted elsewhere: `cpu::hw` checks it can
    /// go on in hardware).
    pub restored: AtomicBool,
    /// The feature ID registers (`sr` key, value) a guest booted in hardware was shown, where they
    /// aren't armless's own (a hypervisor that can't hide all of the host's, as KVM on Linux 6.8
    /// with FP16): empty for a guest that saw armless's. A snapshot carries them; a guest that saw
    /// more than armless emulates goes on only in hardware.
    pub features: SpinLock<Vec<(u32, u64)>>,
    /// The CPUs may run in hardware (`Cpu::enable_hypervisor`): a snapshot whose guest saw
    /// features armless doesn't emulate can be restored.
    pub hw_backend: AtomicBool,
    /// The generic timer's frequency, which the guest reads in CNTFRQ_EL0 at boot and keeps: a
    /// snapshot carries it, and a machine in hardware has the host counter's.
    pub timer_freq: AtomicU64,
    pub rtc: SpinLock<Rtc>,
    pub cpus: [PerCpu; MAX_CPUS],
    /// Generic timer: the counter was `timer_cnt_base` at host time `timer_ns_base`.
    pub timer_cnt_base: AtomicU64,
    pub timer_ns_base: AtomicU64,
    /// Bumped by every broadcast TLB invalidation; what was invalidated is in
    /// `tlb_ring` (VA page numbers, or TLB_ALL).
    pub tlb_gen: AtomicU64,
    pub tlb_ring: SpinLock<[u64; TLB_RING]>,
    pub stop: AtomicU32,
    /// Serialises STXP/LDXP-paired updates (no 128-bit atomics in wasm).
    pub pair_lock: SpinLock<()>,
    /// Instruction-cache invalidations (IC IVAU / IC IALLU), for other CPUs'
    /// JIT code: a ring of physical page numbers (CODE_ALL = everything).
    pub code_inval: SpinLock<CodeInval>,
    /// Bumped by SEV: wakes every CPU waiting in WFE.
    pub sev: AtomicU32,
    /// Host cores available to the vCPU threads (0: unknown). With more vCPUs
    /// than cores, waits give up the host thread instead of spinning.
    pub host_cores: AtomicU32,
    /// Statistics: broadcast TLBIs, DSB waits that had to spin, IC
    /// invalidations, kicks.
    pub stats: [AtomicU64; 4],
    /// What the CPUs' waits for one another cost (WAIT_TLB, WAIT_WFE).
    pub waits: [WaitStats; WAIT_KINDS],
    /// How waiting CPUs behave: POLICY_ADAPTIVE or POLICY_SPIN_COUNT.
    pub policy: AtomicU32,
    /// Waits of SLOW_WAIT_NS or more: a CPU that should have been running wasn't, so the host
    /// has fewer cores free than it says (a phone's browser, say).
    pub slow_waits: AtomicU32,
    /// Of the IC invalidations, those of a device's DMA write (not a CPU's IC IVAU).
    pub dma_code_invals: AtomicU64,
    /// One flag per RAM page: some CPU's JIT holds code translated from it
    /// (device DMA into such a page invalidates that code).
    pub code_pages: Vec<AtomicU8>,
    waker: SpinLock<Option<Waker>>,
}

impl Machine {
    /// Counter ticks in `ns` nanoseconds, at `timer_freq`.
    pub fn ticks_in_ns(&self, ns: u64) -> u64 {
        (ns as u128 * self.timer_freq.load(Ordering::Relaxed) as u128 / 1_000_000_000) as u64
    }

    /// Nanoseconds until `ticks` counter ticks have passed (rounded up), at `timer_freq`.
    pub fn ns_for_ticks(&self, ticks: u64) -> u64 {
        let f = self.timer_freq.load(Ordering::Relaxed).max(1) as u128;
        (ticks as u128 * 1_000_000_000).div_ceil(f).min(u64::MAX as u128) as u64
    }

    pub fn new(memory_size: usize, ncpus: usize) -> Machine {
        Machine::with_ram(Ram::zeroed(memory_size), ncpus)
    }

    /// A machine on `mem` (`Ram::from_image`, say) rather than on zeroed RAM.
    pub fn with_ram(mem: Ram, ncpus: usize) -> Machine {
        assert!((1..=MAX_CPUS).contains(&ncpus), "1..={MAX_CPUS} CPUs");
        let memory_size = mem.len();
        let m = Machine {
            ncpus,
            mem,
            gic: SpinLock::new(Gic::new(ncpus)),
            gic_v3: AtomicBool::new(false),
            irq_sink: std::sync::OnceLock::new(),
            started: AtomicBool::new(false),
            restored: AtomicBool::new(false),
            features: SpinLock::new(Vec::new()),
            hw_backend: AtomicBool::new(false),
            timer_freq: AtomicU64::new(crate::board::TIMER_FREQ_HZ),
            rtc: SpinLock::new(Rtc::default()),
            cpus: Default::default(),
            timer_cnt_base: AtomicU64::new(0),
            timer_ns_base: AtomicU64::new(0),
            tlb_gen: AtomicU64::new(0),
            tlb_ring: SpinLock::new([TLB_ALL; TLB_RING]),
            stop: AtomicU32::new(STOP_NONE),
            pair_lock: SpinLock::new(()),
            code_inval: SpinLock::new(CodeInval::default()),
            stats: Default::default(),
            waits: Default::default(),
            policy: AtomicU32::new(POLICY_ADAPTIVE),
            slow_waits: AtomicU32::new(0),
            dma_code_invals: AtomicU64::new(0),
            sev: AtomicU32::new(0),
            host_cores: AtomicU32::new(0),
            code_pages: zeroed_flags(memory_size.div_ceil(4096)),
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
        self.stats[3].fetch_add(1, Ordering::Relaxed);
        let c = &self.cpus[cpu];
        c.attn.store(1, Ordering::SeqCst);
        c.wake.fetch_add(1, Ordering::SeqCst);
        #[cfg(armless_hw)]
        crate::cpu::hv_exit(c.hv_vcpu.load(Ordering::SeqCst));
        if let Some(w) = self.waker.lock().as_ref() {
            w(cpu);
        }
    }

    /// Record an instruction-cache invalidation for every CPU's JIT and tell
    /// the other CPUs (`page` = physical page number, or CODE_ALL).
    pub fn invalidate_code(&self, page: u32, from: usize) {
        self.stats[2].fetch_add(1, Ordering::Relaxed);
        if from >= self.ncpus {
            self.dma_code_invals.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut c = self.code_inval.lock();
            // Every invalidation is recorded, even of the page recorded last: CPUs that applied
            // that one may have compiled or decoded the page again since. (IC IVAU comes once
            // per page, the IminLine CTR_EL0 reports with several CPUs, and a DMA write only for
            // a page flagged as holding code since the last one.)
            let n = c.gen as usize % CODE_RING;
            c.ring[n] = page;
            c.gen += 1;
        }
        self.poke_others(from);
    }

    /// Ask the running CPUs other than `except` to apply broadcast TLB or
    /// instruction-cache invalidations soon. Idle CPUs are not woken: they
    /// hold no translations in flight and apply them when they run again.
    pub fn poke_others(&self, except: usize) {
        for i in 0..self.ncpus {
            if i != except && self.cpus[i].run_state.load(Ordering::SeqCst) == RUN_RUNNING {
                self.cpus[i].attn.store(1, Ordering::SeqCst);
            }
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

    /// Not more host cores than vCPUs (one is needed for the devices / main
    /// thread): waiting CPUs should yield the host thread, not spin. With the
    /// adaptive policy, also once waits have shown the CPUs don't all run at
    /// once, whatever the host reported (SLOW_WAITS_TO_YIELD).
    pub fn oversubscribed(&self) -> bool {
        let cores = self.host_cores.load(Ordering::Relaxed);
        cores == 0
            || self.ncpus as u32 >= cores
            || self.adaptive() && self.slow_waits.load(Ordering::Relaxed) >= SLOW_WAITS_TO_YIELD
    }

    pub fn adaptive(&self) -> bool {
        self.policy.load(Ordering::Relaxed) == POLICY_ADAPTIVE
    }

    /// A wait of `kind` ended: count it, and note a TLB wait that shows a CPU went without a host core.
    pub fn waited(&self, kind: usize, spins: u64, yields: u64, ns: u64) {
        self.waits[kind].record(spins, yields, ns);
        if kind == WAIT_TLB && ns >= SLOW_WAIT_NS {
            self.slow_waits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The CPUs and their waits, as JSON (for `armless_mem_report`).
    pub fn report_json(&self, insns: &[u64]) -> String {
        let s = |i: usize| self.stats[i].load(Ordering::Relaxed);
        format!(
            concat!(
                "{{\"cpus\":{},\"host_cores\":{},\"policy\":\"{}\",\"oversubscribed\":{},\"slow_waits\":{},",
                "\"tlbi\":{},\"ic_invals\":{},\"kicks\":{},\"insns\":{:?},",
                "\"waits\":{{\"tlb\":{},\"wfe\":{},\"lock\":{}}}}}"
            ),
            self.ncpus,
            self.host_cores.load(Ordering::Relaxed),
            if self.adaptive() { "adaptive" } else { "spin-count" },
            self.oversubscribed(),
            self.slow_waits.load(Ordering::Relaxed),
            s(0),
            s(2),
            s(3),
            insns,
            self.waits[WAIT_TLB].json(),
            self.waits[WAIT_WFE].json(),
            LOCK_WAITS.json(),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Slow waits teach an adaptive machine that its CPUs don't all run at once, whatever the
    /// host reported; the spin-count policy goes by the host's word alone.
    #[test]
    fn slow_waits_make_an_adaptive_machine_oversubscribed() {
        let m = Machine::new(1 << 20, 2);
        m.host_cores.store(8, Ordering::Relaxed);
        assert!(!m.oversubscribed());
        for _ in 0..SLOW_WAITS_TO_YIELD {
            m.waited(WAIT_TLB, 10, 0, SLOW_WAIT_NS - 1);
        }
        assert!(!m.oversubscribed(), "short waits teach nothing");
        for _ in 0..SLOW_WAITS_TO_YIELD {
            m.waited(WAIT_WFE, 10, 1, 10 * SLOW_WAIT_NS);
        }
        assert!(!m.oversubscribed(), "a guest may wait in WFE for long on purpose");
        for _ in 0..SLOW_WAITS_TO_YIELD {
            m.waited(WAIT_TLB, 10, 1, SLOW_WAIT_NS);
        }
        assert!(m.oversubscribed());
        m.policy.store(POLICY_SPIN_COUNT, Ordering::Relaxed);
        assert!(!m.oversubscribed());
        m.host_cores.store(2, Ordering::Relaxed);
        assert!(m.oversubscribed(), "two vCPUs on two cores");
    }

    /// The report has each kind of wait, what was learned, and the instructions of each CPU.
    #[test]
    fn the_report_says_what_the_waits_cost() {
        let m = Machine::new(1 << 20, 2);
        m.host_cores.store(4, Ordering::Relaxed);
        m.waited(WAIT_TLB, 100, 2, 1_500_000);
        m.waited(WAIT_TLB, 50, 0, 500_000);
        let r = m.report_json(&[7, 9]);
        assert!(r.starts_with("{\"cpus\":2,\"host_cores\":4,\"policy\":\"adaptive\",\"oversubscribed\":false,\"slow_waits\":0,"), "{r}");
        assert!(r.contains("\"insns\":[7, 9]"), "{r}");
        assert!(r.contains("\"tlb\":{\"waits\":2,\"spins\":150,\"yields\":2,\"ms\":2.000,\"max_ms\":1.500}"), "{r}");
        assert!(r.contains("\"wfe\":{\"waits\":0,"), "{r}");
        assert!(r.contains("\"lock\":{\"waits\":"), "{r}");
        m.policy.store(POLICY_SPIN_COUNT, Ordering::Relaxed);
        assert!(m.report_json(&[0, 0]).contains("\"policy\":\"spin-count\""));
    }

    /// A lock another thread holds is waited for, and the wait counted.
    #[test]
    fn a_contended_spinlock_is_counted() {
        let lock = Arc::new(SpinLock::new(0u32));
        let before = LOCK_WAITS.waits.load(Ordering::Relaxed);
        let held = lock.lock();
        let other = lock.clone();
        let waiter = std::thread::spawn(move || *other.lock() += 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held);
        waiter.join().unwrap();
        assert_eq!(*lock.lock(), 1);
        assert!(LOCK_WAITS.waits.load(Ordering::Relaxed) > before);
        assert!(LOCK_WAITS.yields.load(Ordering::Relaxed) > 0, "a native thread yields after 128 spins");
    }
}
