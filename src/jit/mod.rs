//! Page JIT: the hot entry points of a physical code page are translated to
//! one WebAssembly module (see `gen`), compiled by the host (JavaScript
//! instantiates it asynchronously and puts it into the shared function table)
//! and then called from the run loop. Inside a module, direct branches
//! between its entries do not return to the run loop.
//!
//! Entries are keyed by physical address, exception level and SPSel; modules
//! by physical page, EL and SPSel. When another entry on a compiled page gets
//! hot, the page is recompiled with all of its entries and the new module
//! replaces the old one once it is ready. Any write to a physical page
//! holding translated code (by the guest, or DMA from the host via
//! `armless_jit_dirty`) drops that page's module. Pages with code are never
//! entered into the fast write TLB, so guest stores to them always reach the
//! slow path, which does the invalidation.

pub(crate) mod gen;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
pub mod native;
pub mod profile;
pub(crate) mod wasm;

use std::collections::HashMap;

use crate::board::Host;
use crate::cpu::{Cpu, Event, Exit, Res, StopReason};

/// Table index of the first JIT slot (Rust's own function pointers stay below).
pub const SLOT_BASE: u32 = 1024;
/// Number of compiled modules that can exist at once.
pub const SLOTS: u32 = 4096;
const MAX_BLOCK: usize = 96;
/// Entries per page module; more get no module of their own.
const MAX_ENTRIES: usize = 192;
/// Instructions a module may run before it returns to the run loop.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const CHAIN_BUDGET: u64 = 0x400;
/// Extra modules a page gets (see `PageInfo::extra`) before its own module is compiled again with
/// all their entries, so that they chain to one another again.
const CONSOLIDATE: usize = 3;
const LOOKUP_BITS: u32 = 14;
const LOOKUP: usize = 1 << LOOKUP_BITS;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotState {
    Free,
    Compiling,
    Ready,
    /// Invalidated or superseded while compiling: freed when the host reports it ready.
    Dead,
}

/// What `jit_compile` did with a hot entry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Compile {
    /// A compile was started.
    Started,
    /// Not now: a compile that has the entry, or one of its page, is in flight.
    Waiting,
    /// Not at all: the entry has a module, or the page has all the entries it may, or nothing can compile it.
    Declined,
}

/// Why a page's code was dropped (ARMLESS_JIT_INVAL_STATS).
#[derive(Clone, Copy)]
pub(crate) enum Inval {
    /// A store of this CPU to a word its compiled code covers (or to a page with decoded
    /// instructions only).
    Store = 0,
    /// An IC IVAU of this CPU (several CPUs).
    IcLocal = 1,
    /// Another CPU's IC IVAU, or a device's DMA write, from the shared ring.
    Remote = 2,
}

const INVAL_NAMES: [&str; 3] = [
    "store",
    "ic ivau (own)",
    "remote (other cpu / dma)",
];

/// Pages dropped by why, split into those with compiled code and those with decoded
/// instructions only, and how often each page with compiled code was dropped.
#[derive(Default)]
pub struct InvalStats {
    by: [[u64; 2]; 3],
    compiled_pages: HashMap<u32, u32>,
}

impl InvalStats {
    fn record(&mut self, page: usize, why: Inval, compiled: bool) {
        self.by[why as usize][compiled as usize] += 1;
        if compiled {
            *self.compiled_pages.entry(page as u32).or_default() += 1;
        }
    }

    /// One line per reason, then how concentrated the drops of compiled code are.
    pub fn report(&self, cpu: usize) -> String {
        let mut out = String::new();
        for (name, [decoded, compiled]) in INVAL_NAMES.iter().zip(self.by) {
            if decoded + compiled > 0 {
                out += &format!("jit-inval: cpu{cpu} {name:<26} compiled {compiled:>7}  decoded only {decoded:>7}\n");
            }
        }
        let mut counts: Vec<u32> = self.compiled_pages.values().copied().collect();
        counts.sort_unstable_by(|a, b| b.cmp(a));
        let total: u64 = counts.iter().map(|&c| c as u64).sum();
        let top10: u64 = counts.iter().take(10).map(|&c| c as u64).sum();
        let once = counts.iter().filter(|&&c| c == 1).count();
        out += &format!(
            "jit-inval: cpu{cpu} compiled drops {total} over {} pages ({once} dropped once); the 10 most dropped pages: {top10} ({:?})\n",
            counts.len(),
            &counts[..counts.len().min(10)]
        );
        out
    }
}

/// A page (with its EL and SPSel) that has, or is about to have, a module.
#[derive(Default)]
struct PageInfo {
    /// Slot of the module in use, and the entries it contains.
    slot: Option<u32>,
    entries: Vec<u16>,
    /// Slot being compiled, and the entries it will contain.
    compiling: Option<u32>,
    compiling_entries: Vec<u16>,
    /// Entries that got warm: included in the next compile of the page.
    warm: Vec<u16>,
    /// Natively: modules compiled besides the page's own (`slot`) for entries it lacks, so that an
    /// entry that gets hot is compiled at once rather than waiting for the page to be compiled
    /// again with it. Ready ones and ones being compiled, each with its entries.
    extra: Vec<(u32, Vec<u16>)>,
    extra_compiling: Vec<(u32, Vec<u16>)>,
}

impl PageInfo {
    /// The ready module that has entry `off`.
    fn slot_of(&self, off: u16) -> Option<u32> {
        if let Some(s) = self
            .slot
            .filter(|_| self.entries.binary_search(&off).is_ok())
        {
            return Some(s);
        }
        self.extra
            .iter()
            .find(|(_, e)| e.contains(&off))
            .map(|&(s, _)| s)
    }

    /// Whether a module of this page has, or will have, entry `off`.
    fn covers(&self, off: u16) -> bool {
        self.slot_of(off).is_some()
            || self.compiling_entries.contains(&off)
            || self.extra_compiling.iter().any(|(_, e)| e.contains(&off))
    }

    fn covered(&self) -> usize {
        self.entries.len()
            + self
                .extra
                .iter()
                .chain(&self.extra_compiling)
                .map(|(_, e)| e.len())
                .sum::<usize>()
    }
}

pub struct Jit {
    pub enabled: bool,
    /// Testing aid: return from `run` after every compile request so the host
    /// can finish compiling before the code executes for the first time.
    pub sync_compile: bool,
    /// Set when `run` returned early for `sync_compile`.
    pub(crate) yielded: bool,
    /// Executions of an entry before it is compiled.
    pub threshold: u32,
    /// Compiles are a profile's (`jit_prewarm`), so they wait for the ones code needs now.
    prewarming: bool,
    /// Whether each slot's module has run since it was compiled: a profile keeps only the pages
    /// whose code ran, not everything compiled (a profile's pages are compiled whether or not
    /// they run).
    slot_used: Vec<bool>,
    /// Whether each slot compiling is a profile's, still waiting behind code needed now.
    slot_later: Vec<bool>,
    lookup_key: Vec<u64>,
    lookup_slot: Vec<u32>,
    pages: HashMap<u64, PageInfo>,
    slot_page: Vec<u64>,
    slot_state: Vec<SlotState>,
    free: Vec<u32>,
    /// Physical page number -> page keys (one per EL/SPSel) with modules.
    page_keys: HashMap<u32, Vec<u64>>,
    /// One byte per physical page of RAM: non-zero if it holds compiled code (bit 0) or
    /// pre-decoded instructions (bit 1).
    pub code_page: Vec<u8>,
    /// The interpreter's pre-decoded pages, dropped with the page's compiled code.
    pub pre: crate::predecode::Cache,
    /// Physical page -> the instruction words its compiled blocks cover (1024 bits). A write to
    /// the page drops its compiled code only if it touches one of them.
    covered: HashMap<u32, Box<[u64; 16]>>,
    /// ARMLESS_JIT_DECODED_WRITES (experiment, 0: off): writes a page with decoded instructions
    /// only takes before it is dropped (each forgets the words written), and the count per page.
    decoded_writes_limit: u32,
    decoded_writes: HashMap<u32, u32>,
    /// Statistics
    pub compiled: u64,
    pub invalidated: u64,
    pub block_runs: u64,
    /// Block runs right after a block run (no instruction interpreted in between): the exits a
    /// jump from compiled code into the next module would keep out of the run loop.
    pub block_to_block: u64,
    /// Whether the last thing the run loop did was a block run.
    pub(crate) last_ran_block: bool,
    /// Block runs by how they ended: done, redirected by a helper, exception pending, WFI.
    pub exits: [u64; 4],
    /// Full flushes (slots exhausted, reset, configuration).
    pub flushes: u64,
    /// Pages dropped because they were written.
    pub page_invalidations: u64,
    /// Writes to pages with code that touched none of it, and kept it.
    pub code_writes_kept: u64,
    /// Instructions retired inside modules, and those of them interpreted.
    pub block_insns: u64,
    pub helper_interps: u64,
    /// Lanes of floating-point vector instructions compiled code computed through `fp3`.
    pub helper_fp_lanes: u64,
    /// ARMLESS_JIT_HELPER_STATS: instructions compiled code hands to the interpreter, by opcode
    /// (registers masked out) with how many and one of them: what the JIT should translate next.
    pub helper_hist: Option<HashMap<u32, (u64, u32)>>,
    /// ARMLESS_JIT_INVAL_STATS: why pages' code was dropped (`InvalStats::report`).
    pub inval_stats: Option<Box<InvalStats>>,
    /// The native JIT's compiled code (None: pages go to the host, i.e. the browser) and the
    /// address of each slot's function in it.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub(crate) native: Option<native::Native>,
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    native_fns: Vec<usize>,
    /// For the perf map (`native::perf_map`): the virtual page each slot was compiled for (the
    /// PC's when it got hot; u64::MAX for a profile's), and its entries.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    slot_va: Vec<(u64, u16)>,
}

#[inline(always)]
fn hash(key: u64) -> usize {
    (((key >> 2) ^ (key >> 13)) as usize) & (LOOKUP - 1)
}

/// Page key of an entry key.
#[inline(always)]
fn page_of(key: u64) -> u64 {
    key & !(0xfff << 2)
}

impl Jit {
    pub fn new(memory_size: usize) -> Jit {
        Jit {
            enabled: false,
            sync_compile: false,
            yielded: false,
            // ARMLESS_JIT_THRESHOLD: executions of an entry before it is compiled
            threshold: std::env::var("ARMLESS_JIT_THRESHOLD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024)
                .max(1),
            prewarming: false,
            slot_used: vec![false; SLOTS as usize],
            slot_later: vec![false; SLOTS as usize],
            lookup_key: vec![u64::MAX; LOOKUP],
            lookup_slot: vec![0; LOOKUP],
            pages: HashMap::new(),
            slot_page: vec![u64::MAX; SLOTS as usize],
            slot_state: vec![SlotState::Free; SLOTS as usize],
            free: (0..SLOTS).rev().collect(),
            page_keys: HashMap::new(),
            code_page: vec![0; memory_size.div_ceil(4096)],
            pre: crate::predecode::Cache::new(memory_size),
            covered: HashMap::new(),
            decoded_writes_limit: std::env::var("ARMLESS_JIT_DECODED_WRITES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            decoded_writes: HashMap::new(),
            compiled: 0,
            invalidated: 0,
            block_runs: 0,
            block_to_block: 0,
            last_ran_block: false,
            exits: [0; 4],
            flushes: 0,
            page_invalidations: 0,
            code_writes_kept: 0,
            block_insns: 0,
            helper_interps: 0,
            helper_fp_lanes: 0,
            helper_hist: std::env::var_os("ARMLESS_JIT_HELPER_STATS").map(|_| HashMap::new()),
            inval_stats: std::env::var_os("ARMLESS_JIT_INVAL_STATS").map(|_| Box::default()),
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            native: None,
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            native_fns: vec![0; SLOTS as usize],
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            slot_va: vec![(u64::MAX, 0); SLOTS as usize],
        }
    }

    #[inline(always)]
    pub fn key(pa: u64, el: u8, spsel: bool) -> u64 {
        (pa << 2) | ((el as u64) << 1) | spsel as u64
    }

    /// Whether a module is ready for the entry `key`.
    #[inline(always)]
    pub(crate) fn has_module(&self, key: u64) -> bool {
        self.lookup_key[hash(key)] == key
    }

    #[inline(always)]
    fn ready_slot(&self, key: u64) -> Option<u32> {
        let i = hash(key);
        if self.lookup_key[i] == key {
            Some(self.lookup_slot[i])
        } else {
            None
        }
    }

    fn free_slot(&mut self, slot: u32) {
        let s = slot as usize;
        match self.slot_state[s] {
            SlotState::Compiling => self.slot_state[s] = SlotState::Dead,
            SlotState::Ready => {
                self.slot_state[s] = SlotState::Free;
                self.slot_page[s] = u64::MAX;
                self.free.push(slot);
            },
            _ => {},
        }
    }

    fn unmap_entries(&mut self, page_key: u64, entries: &[u16]) {
        for &e in entries {
            let key = page_key | ((e as u64) << 2);
            let i = hash(key);
            if self.lookup_key[i] == key {
                self.lookup_key[i] = u64::MAX;
            }
        }
    }

    fn remove_page(&mut self, page_key: u64) {
        if let Some(info) = self.pages.remove(&page_key) {
            self.unmap_entries(page_key, &info.entries);
            if let Some(s) = info.slot {
                self.free_slot(s);
                self.invalidated += 1;
            }
            if let Some(s) = info.compiling {
                self.free_slot(s);
                self.invalidated += 1;
            }
            for (s, entries) in info.extra.iter().chain(&info.extra_compiling) {
                self.unmap_entries(page_key, entries);
                self.free_slot(*s);
                self.invalidated += 1;
            }
        }
    }

    /// Drop all modules of a physical page, recording why (ARMLESS_JIT_INVAL_STATS).
    pub(crate) fn invalidate_page_by(&mut self, page: usize, why: Inval) {
        if let (Some(s), Some(&flags)) = (self.inval_stats.as_mut(), self.code_page.get(page)) {
            if flags != 0 {
                s.record(page, why, flags & 1 != 0);
            }
        }
        self.invalidate_page(page);
    }

    /// Drop all modules of a physical page.
    pub fn invalidate_page(&mut self, page: usize) {
        if page >= self.code_page.len() || self.code_page[page] == 0 {
            return;
        }
        self.code_page[page] = 0;
        self.page_invalidations += 1;
        self.pre.drop_page(page);
        self.covered.remove(&(page as u32));
        self.decoded_writes.remove(&(page as u32));
        if let Some(keys) = self.page_keys.remove(&(page as u32)) {
            for k in keys {
                self.remove_page(k);
            }
        }
    }

    /// The guest wrote `size` bytes at physical address `pa`, on a page with code. Compiled code
    /// is dropped only if the write touches words its blocks were translated from (then the
    /// decoded instructions of the words written go too). A JVM, say, writes new code and data
    /// next to compiled code: dropping the whole page each time threw away code that was still
    /// right.
    pub(crate) fn code_written(&mut self, pa: u64, size: u32) {
        let page = (pa >> 12) as usize;
        let Some(&flags) = self.code_page.get(page) else {
            return;
        };
        if flags == 0 || size == 0 {
            return;
        }
        // Decoded instructions alone are cheap to decode again, and dropping them lets writes to
        // the page take the fast path again: only compiled code is worth keeping
        let first = ((pa & 0xfff) >> 2) as usize;
        let last = (((pa & 0xfff) + size as u64 - 1).min(0xfff) >> 2) as usize;
        if flags & 1 == 0 {
            // ARMLESS_JIT_DECODED_WRITES (experiment): the first N writes to such a page forget
            // only the words written, keeping the rest decoded and their counts towards compiling
            if self.decoded_writes_limit > 0 {
                let n = self.decoded_writes.entry(page as u32).or_default();
                if *n < self.decoded_writes_limit {
                    *n += 1;
                    self.pre.forget_words(page, first, last);
                    self.code_writes_kept += 1;
                    return;
                }
            }
            self.invalidate_page_by(page, Inval::Store);
            return;
        }
        let hit = match self.covered.get(&(page as u32)) {
            Some(c) => (first..=last).any(|w| c[w / 64] & (1 << (w % 64)) != 0),
            None => true, // compiled code with no record of what it covers: drop it
        };
        if hit {
            self.invalidate_page_by(page, Inval::Store);
        } else {
            self.pre.forget_words(page, first, last);
            self.code_writes_kept += 1;
        }
    }
    pub fn flush_all(&mut self) {
        self.flushes += 1;
        let keys: Vec<u64> = self.pages.keys().copied().collect();
        for k in keys {
            self.remove_page(k);
        }
        self.page_keys.clear();
        self.covered.clear();
        self.decoded_writes.clear();
        self.code_page.fill(0);
        self.pre.clear();
        self.lookup_key.fill(u64::MAX);
    }

    /// The host finished compiling the module for `table_index`.
    pub fn ready(&mut self, table_index: u32) {
        let Some(slot) = table_index.checked_sub(SLOT_BASE) else {
            return;
        };
        let s = slot as usize;
        if s >= SLOTS as usize {
            return;
        }
        match self.slot_state[s] {
            SlotState::Compiling => {
                let page_key = self.slot_page[s];
                self.slot_state[s] = SlotState::Ready;
                let Some(info) = self.pages.get_mut(&page_key) else {
                    return;
                };
                if let Some(i) = info.extra_compiling.iter().position(|&(s, _)| s == slot) {
                    let (slot, entries) = info.extra_compiling.swap_remove(i);
                    for &e in &entries {
                        let key = page_key | ((e as u64) << 2);
                        let i = hash(key);
                        self.lookup_key[i] = key;
                        self.lookup_slot[i] = slot;
                    }
                    info.extra.push((slot, entries));
                    return;
                }
                if info.compiling != Some(slot) {
                    return;
                }
                info.compiling = None;
                let entries = std::mem::take(&mut info.compiling_entries);
                let old = info.slot.replace(slot);
                let old_entries = std::mem::replace(&mut info.entries, entries.clone());
                // Extra modules whose entries all moved into this one: no lookup reaches them now
                // (the loop below maps their keys here).
                let (merged, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut info.extra)
                    .into_iter()
                    .partition(|(_, e)| e.iter().all(|x| entries.binary_search(x).is_ok()));
                info.extra = kept;
                self.unmap_entries(page_key, &old_entries);
                if let Some(old) = old {
                    self.free_slot(old);
                }
                for (s, _) in merged {
                    self.free_slot(s);
                }
                for e in entries {
                    let key = page_key | ((e as u64) << 2);
                    let i = hash(key);
                    self.lookup_key[i] = key;
                    self.lookup_slot[i] = slot;
                }
            },
            SlotState::Dead => {
                self.slot_state[s] = SlotState::Free;
                self.slot_page[s] = u64::MAX;
                self.free.push(slot);
            },
            _ => {},
        }
    }
}

/// Result of trying to run translated code at the current PC.
pub enum JitStep {
    /// Nothing ran; interpret the next instruction.
    NotCompiled,
    /// A block ran; `n` instructions retired.
    Ran(u64),
    /// A block ended in WFI.
    Wfi(u64),
    /// Stop the machine.
    Stop(StopReason),
}

impl<H: Host> Cpu<H> {
    /// Physical address of the current PC if its page is in the fetch cache.
    #[inline(always)]
    fn jit_pc_pa(&mut self) -> Option<u64> {
        // A misaligned PC is no block's entry: the interpreter takes its alignment fault
        if self.pc & 3 != 0 {
            return None;
        }
        let f = &self.fast[self.el as usize];
        if self.pc >> 12 != f.code_page {
            // Fill the fetch cache; a fault is left for the interpreter.
            self.fetch_slow_pub(self.pc).ok()?;
        }
        let f = &self.fast[self.el as usize];
        if self.pc >> 12 == f.code_page {
            Some(f.code_base as u64 + (self.pc & 0xfff))
        } else {
            None
        }
    }

    /// The count of executions of an entry at which its page is compiled (see `jit_step`).
    #[inline(always)]
    fn hot_threshold(&self) -> u32 {
        (self.jit.threshold.min(crate::predecode::COUNT_MAX) << (self.jit.flushes / 8).min(3))
            .min(crate::predecode::COUNT_MAX)
    }

    /// What `jit_step` does with an entry of the page at physical address `base`, for the loop of the
    /// interpreter, which counts the executions of the entries itself: the count at which `jit_step` warms an
    /// entry, the one at which it compiles it, and whether the page has modules (one of which may hold the entry).
    #[inline(always)]
    pub(crate) fn page_counts(&self, base: usize) -> (u32, u32, bool) {
        let hot = self.hot_threshold();
        (
            hot.div_ceil(4).max(1),
            hot,
            self.jit.code_page[base >> 12] & 1 != 0,
        )
    }

    /// Run translated code at the current PC if there is any.
    #[inline(always)]
    pub(crate) fn jit_step(&mut self, budget: u64) -> JitStep {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if self.jit.native.as_ref().is_some_and(|n| n.has_done()) {
            self.jit_take_compiled();
        }
        let Some(pa) = self.jit_pc_pa() else {
            self.jit.last_ran_block = false;
            return JitStep::NotCompiled;
        };
        let key = Jit::key(pa, self.el, self.spsel);
        // Only a page that was ever compiled (or is compiling) can have a module to run: the
        // lookup table is not touched for the cold ones.
        if self.jit.code_page[(pa >> 12) as usize] & 1 != 0 {
            if let Some(slot) = self.jit.ready_slot(key) {
                return self.jit_run(slot, budget);
            }
        }
        self.jit.last_ran_block = false;
        // Count this execution of the entry in its predecoded slot (`Cache::bump`): the line is
        // the one the interpreter reads next.
        let i = ((pa & 0xfff) >> 2) as usize;
        let mut c = self.jit.pre.bump(pa as usize & !0xfff, i);
        if c == 0 {
            // Not decoded yet: decode it now (the interpreter would next), and count it.
            let _ = self.fetch_decoded_slow(self.pc);
            c = self.jit.pre.bump(pa as usize & !0xfff, i);
        }
        // The counts are exact, so what a workload runs a few thousand times each, in more places than the
        // modules can hold, would be compiled, flushed and compiled again. Every 8 flushes the threshold
        // doubles (at most 8 times the configured one), so that the code that is run the most stays.
        let threshold = self.hot_threshold();
        if c >= threshold {
            match self.jit_compile(key) {
                Compile::Started => {
                    if self.jit.sync_compile && self.jit.enabled {
                        self.jit.yielded = true;
                    }
                },
                Compile::Waiting => {
                    // A compile that has the entry is in flight, or one of its page, which this one waits for:
                    // the loop of the interpreter, which exits to `jit_step` for an entry whose count is at
                    // the threshold, would exit for each execution of it from now on, to be refused again.
                    // Count it from just above the count that warms it, so that it is back here after the
                    // executions in between, to be compiled when it can be. (A module that gets ready is
                    // found by the loop itself, see `has_module`, whatever the count.)
                    let warm = threshold.div_ceil(4).max(1);
                    if warm + 1 < threshold {
                        self.jit.pre.set_count(pa as usize & !0xfff, i, warm + 1);
                    }
                },
                // Nothing to wait for. The count is in the predecoded slot, which every context of the
                // physical code shares, while a page has its entries per context: one that has all the
                // entries it may must not keep another from reaching the threshold, so the count stays.
                Compile::Declined => {},
            }
        } else if c == threshold.div_ceil(4) {
            self.jit_warm(key);
        }
        JitStep::NotCompiled
    }

    #[cold]
    fn jit_warm(&mut self, key: u64) {
        let off = ((key >> 2) & 0xfff) as u16;
        let info = self.jit.pages.entry(page_of(key)).or_default();
        if !info.warm.contains(&off) && info.warm.len() < MAX_ENTRIES {
            info.warm.push(off);
        }
    }

    /// Compile the page of the hot entry `key`, unless a compile including
    /// it is in flight. Says whether a compile was started, and if not whether
    /// the entry can be compiled later (`Waiting`).
    #[cold]
    fn jit_compile(&mut self, key: u64) -> Compile {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if self.jit.native.as_ref().is_some_and(|n| n.full()) {
            // Start a new module: no slot may point into the old one.
            self.jit.flush_all();
            if let Some(n) = self.jit.native.as_mut() {
                n.reset();
            }
        }
        let page_key = page_of(key);
        let off = ((key >> 2) & 0xfff) as u16;
        let additive = self.jit_additive();
        let info = self.jit.pages.entry(page_key).or_default();
        if let Some(slot) = info.slot_of(off) {
            // Compiled, but its lookup entry was evicted by a collision.
            let i = hash(key);
            self.jit.lookup_key[i] = key;
            self.jit.lookup_slot[i] = slot;
            return Compile::Declined;
        }
        // An entry that got hot on a page a profile queued is needed now: it gets a module of its
        // own at once rather than waiting behind the rest of the profile
        let queued_later = |s: &u32| self.jit.slot_later[*s as usize];
        let waits_for_later = !self.jit.prewarming
            && info.compiling.as_ref().is_some_and(queued_later)
            && info.compiling_entries.contains(&off)
            && !info.extra_compiling.iter().any(|(_, e)| e.contains(&off));
        if info.covers(off) && !waits_for_later {
            return Compile::Waiting;
        }
        let busy = info.compiling.is_some();
        if busy && !additive {
            if !info.warm.contains(&off) {
                info.warm.push(off);
            }
            return Compile::Waiting;
        }
        if info.covered() >= MAX_ENTRIES {
            // The page has as many compiled entries as it may: further ones stay interpreted.
            return Compile::Declined;
        }
        // A page with a module (or one on the way) gets another for this entry and the warm ones
        // no module has; else the page's module is (re)compiled with them all, and with the
        // entries of its extra modules once there are a few of those.
        let consolidate = additive && !busy && info.extra.len() >= CONSOLIDATE;
        let extra = additive && (info.slot.is_some() || busy) && !consolidate;
        let mut entries: Vec<u16> = if extra { Vec::new() } else { info.entries.clone() };
        if consolidate {
            for (_, e) in &info.extra {
                entries.extend_from_slice(e);
            }
            entries.truncate(MAX_ENTRIES - 1);
        }
        entries.push(off);
        for &w in &info.warm {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            if !(extra && info.covers(w)) {
                entries.push(w);
            }
        }
        entries.sort_unstable();
        entries.dedup();
        info.warm.clear();

        let Some(slot) = self.jit.free.pop().or_else(|| {
            self.jit.flush_all();
            self.jit.free.pop()
        }) else {
            return Compile::Waiting;
        };
        // flush_all may have removed the page info
        let page_pa = (page_key >> 2) as usize;
        let mut page = [0u8; 0x1000];
        if !self.mem.read(page_pa, &mut page) {
            self.jit.free.push(slot);
            return Compile::Declined;
        }
        let words: Vec<u32> = page
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| u32::from_le_bytes(c))
            .collect();
        let layout = self.jit_layout_for(page_key);
        let code = gen::translate_page_code(&layout, &words, &entries, MAX_BLOCK);

        let page = page_pa >> 12;
        // The words these blocks were translated from: writes to them drop the page's code
        let covered = self
            .jit
            .covered
            .entry(page as u32)
            .or_insert_with(|| Box::new([0; 16]));
        for &e in &entries {
            let start = e as usize / 4;
            for w in start..start + gen::block_insns(&words, start, MAX_BLOCK).len() {
                covered[w / 64] |= 1 << (w % 64);
            }
        }
        self.jit.code_page[page] |= 1;
        self.machine.code_pages[page].store(1, std::sync::atomic::Ordering::Relaxed);
        let keys = self.jit.page_keys.entry(page as u32).or_default();
        if !keys.contains(&page_key) {
            keys.push(page_key);
        }
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        {
            // The PC is the hot entry's, unless a profile is being compiled
            let va = if self.jit.prewarming { u64::MAX } else { self.pc & !0xfff };
            self.jit.slot_va[slot as usize] = (va, entries.len() as u16);
        }
        let info = self.jit.pages.entry(page_key).or_default();
        if extra {
            info.extra_compiling.push((slot, entries));
        } else {
            info.compiling = Some(slot);
            info.compiling_entries = entries;
        }
        self.jit.slot_page[slot as usize] = page_key;
        self.jit.slot_state[slot as usize] = SlotState::Compiling;
        self.jit.slot_used[slot as usize] = false;
        self.jit.slot_later[slot as usize] = self.jit.prewarming;
        self.jit.compiled += 1;
        // Stores to this page must take the slow path from now on.
        for f in self.fast.iter_mut() {
            f.forget_write_page(page_pa);
        }
        if !self.jit_compile_code(slot, &code, &layout) {
            // Nothing can compile it: give up on the JIT.
            self.jit.enabled = false;
            self.jit.flush_all();
            return Compile::Declined;
        }
        Compile::Started
    }

    /// Whether a page gets a module of its own for entries that get hot after it was compiled
    /// (natively, where small modules compile quickly) instead of being compiled again.
    fn jit_additive(&self) -> bool {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if self.jit.native.is_some() {
            return true;
        }
        false
    }

    /// Have the function of slot `slot` compiled, by the native JIT or the host; either reports it
    /// ready when it is.
    fn jit_compile_code(&mut self, slot: u32, code: &wasm::Code, layout: &gen::Layout) -> bool {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        let bases = self.jit_bases(layout.el as usize);
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if let Some(n) = self.jit.native.as_mut() {
            // On a thread of its own: `jit_take_compiled` marks the slot ready when it is. Until
            // then the slot must not keep a function from its previous page.
            self.jit.native_fns[slot as usize] = 0;
            // Loads of the general registers, SP, the instruction count and the flags reuse the
            // value the block last stored or loaded there, until the interpreter may change it.
            let wide = (0..31)
                .map(|r| layout.x + 8 * r)
                .chain([layout.sp, layout.sp_el0, layout.insn_count])
                .map(|addr| native::Cached { addr, byte: false });
            let flags = [layout.n, layout.z, layout.c, layout.v]
                .into_iter()
                .map(|addr| native::Cached { addr, byte: true });
            let cached = wide.chain(flags).collect();
            return if self.jit.prewarming {
                n.submit_later(slot, code.b.clone(), cached, bases)
            } else {
                n.submit(slot, code.b.clone(), cached, bases)
            };
        }
        let wasm = wasm::module(&gen::imports(), gen::LOCALS, code, layout.smp != 0);
        self.host.jit_compile(SLOT_BASE + slot, &wasm)
    }

    /// The regions native code for EL `el` reaches at fixed addresses (`native::Bases`): this CPU,
    /// guest RAM and that EL's fast TLB tables. `jit_run` passes their starts to every function.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    #[inline(always)]
    fn jit_bases(&self, el: usize) -> native::Bases {
        let [rt, rb, wt, wb] = self.fast[el].tables();
        native::Bases([
            (
                self as *const Self as u64,
                std::mem::size_of::<Self>() as u64,
            ),
            (self.mem.as_ptr() as u64, self.mem.len() as u64),
            rt,
            rb,
            wt,
            wb,
        ])
    }

    /// The native compiler's statistics: (functions compiled, nanoseconds spent compiling).
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn native_jit_stats(&self) -> (u64, u64) {
        self.jit.native.as_ref().map_or((0, 0), |n| n.stats())
    }

    /// Native compiles that failed (their blocks stay interpreted): zero unless `gen` makes
    /// something the native compiler does not translate.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn native_jit_failures(&self) -> u64 {
        self.jit.native.as_ref().map_or(0, |n| n.failures())
    }

    /// Bytes of code the native JIT holds for this CPU, stale functions included until its next
    /// reset.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn native_jit_code_bytes(&self) -> usize {
        self.jit.native.as_ref().map_or(0, |n| n.code_bytes())
    }

    /// Testing aid: wait for the native JIT to finish what it was given; the next block entry takes
    /// the results.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn wait_native_jit(&self) {
        if let Some(n) = &self.jit.native {
            n.wait_idle();
        }
    }

    /// Take the slots the native JIT finished compiling and mark them ready.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    #[cold]
    fn jit_take_compiled(&mut self) {
        let Some(n) = self.jit.native.as_mut() else {
            return;
        };
        let generation = n.generation;
        for done in n.take_done() {
            // A slot of an older generation was flushed before its module was freed: `ready`
            // only frees it.
            self.jit.native_fns[done.slot as usize] =
                if done.generation == generation { done.function } else { 0 };
            if done.generation == generation && done.function != 0 {
                self.jit_perf_map_line(done.slot, done.function, done.size);
            }
            self.jit.ready(SLOT_BASE + done.slot);
        }
    }

    /// Name the function compiled for `slot` in the perf map (ARMLESS_JIT_PERF_MAP), after the
    /// guest code it was translated from: its exception level, virtual page (the one it got hot
    /// at; the same physical page can be mapped at others) and physical page, as in
    /// `armless EL0 va 0x0000aaaab2c41000 pa 0x41a3f000 [cpu1, 12 entries]`.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    #[cold]
    #[allow(unsafe_code)]
    fn jit_perf_map_line(&self, slot: u32, function: usize, size: usize) {
        use std::io::Write;
        if let Some(dump) = native::code_dump() {
            let mut d = dump.lock().unwrap();
            // SAFETY: the function's code, just compiled and readable, `size` bytes from `function`
            let code = unsafe { std::slice::from_raw_parts(function as *const u8, size) };
            let offset = d.2;
            if d.0.write_all(code).is_ok() {
                let _ = writeln!(d.1, "{function:x} {size:x} {offset:x}");
                d.2 += size as u64;
            }
        }
        let Some(mut map) = native::perf_map() else {
            return;
        };
        let key = self.jit.slot_page[slot as usize];
        let (va, entries) = self.jit.slot_va[slot as usize];
        let el = (key >> 1) & 1;
        let pa = (key >> 2) & !0xfff;
        let va = if va == u64::MAX { "?".to_string() } else { format!("{va:#018x}") };
        // One write per line: the file is appended to by every CPU
        let line = format!(
            "{function:x} {size:x} armless EL{el} va {va} pa {pa:#x} [cpu{}, {entries} entries]\n",
            self.index
        );
        if let Err(e) = map.write_all(line.as_bytes()) {
            eprintln!("armless: perf map: {e}");
        }
    }

    /// The code this CPU has compiled and run: each page with its entry points and a
    /// hash of what the page holds. Pages are physical (with their EL and SPSel), so a profile
    /// stays meaningful across a save and restore; the hash tells `jit_prewarm` whether a page
    /// still holds the same code (a new process can land on other pages from run to run).
    pub fn jit_profile(&self) -> Vec<profile::Page> {
        let mut pages: Vec<profile::Page> = self
            .jit
            .pages
            .iter()
            .filter_map(|(&key, info)| {
                let used = |s: &u32| self.jit.slot_used[*s as usize];
                // The entries of the modules that ran
                let mut entries: Vec<u16> = info
                    .slot
                    .iter()
                    .filter(|s| used(s))
                    .flat_map(|_| &info.entries)
                    .chain(
                        info.extra
                            .iter()
                            .filter(|(s, _)| used(s))
                            .flat_map(|(_, e)| e),
                    )
                    .copied()
                    .collect();
                if entries.is_empty() {
                    return None;
                }
                entries.sort_unstable();
                entries.dedup();
                Some(profile::Page {
                    key,
                    hash: self.jit_page_hash(key)?,
                    runs: 1,
                    entries,
                })
            })
            .collect();
        pages.sort_unstable_by_key(|p| (p.key, p.hash));
        pages
    }

    fn jit_page_hash(&self, page_key: u64) -> Option<u64> {
        let mut page = [0u8; 0x1000];
        self.mem
            .read((page_key >> 2) as usize, &mut page)
            .then(|| profile::hash(&page))
    }

    /// Compile the pages of a profile now, in its order, instead of waiting for each entry to
    /// run `threshold` times. A page that holds something else now than when it was profiled
    /// is skipped. Code is translated from what memory holds and dropped when the guest writes
    /// the page, as always. Returns the modules queued.
    pub fn jit_prewarm(&mut self, pages: &[profile::Page]) -> usize {
        if !self.jit.enabled {
            return 0;
        }
        let mut queued = 0;
        for page in pages {
            if page.entries.is_empty() || self.jit_page_hash(page.key) != Some(page.hash) {
                continue;
            }
            let mut offs = page.entries.clone();
            offs.truncate(MAX_ENTRIES);
            // The rest go into the same module as warm entries
            self.jit.pages.entry(page.key).or_default().warm = offs[1..].to_vec();
            self.jit.prewarming = true;
            if self.jit_compile(page.key | ((offs[0] as u64) << 2)) == Compile::Started {
                queued += 1;
            }
            self.jit.prewarming = false;
        }
        queued
    }

    /// Turn the JIT on with the native compiler (false where the host has no Cranelift backend).
    /// One turned on with `enable_native_jit_here` compiles on a thread from now on, keeping what
    /// it compiled; one already compiling on a thread is left as it is.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn enable_native_jit(&mut self) -> bool {
        if let Some(n) = self.jit.native.as_mut() {
            if n.compiles_here() {
                self.jit.enabled = n.start_thread();
            }
            return self.jit.enabled;
        }
        self.jit.native = native::Native::new(gen::LOCALS, self.native_helpers());
        self.jit.enabled = self.jit.native.is_some();
        self.jit_entry = true;
        self.jit.enabled
    }

    /// Turn the JIT on with the native compiler working on this thread: a page compiles before
    /// the call that asked for it returns (`jit_prewarm`), and no thread starts. For a process
    /// that compiles a profile and then forks: every child starts with that code, sharing it, and
    /// `enable_native_jit` in the child moves compiling to a thread (and a module) of its own.
    /// False where the host has no Cranelift backend.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn enable_native_jit_here(&mut self) -> bool {
        self.jit.native = native::Native::new_here(gen::LOCALS, self.native_helpers());
        self.jit.enabled = self.jit.native.is_some();
        self.jit_entry = true;
        self.jit.enabled
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    fn native_helpers(&self) -> native::Helpers {
        native::Helpers {
            read: native_read::<H> as *const () as usize,
            write: native_write::<H> as *const () as usize,
            interp: native_interp::<H> as *const () as usize,
            fp3: native_fp3::<H> as *const () as usize,
        }
    }

    /// Layout for code of the page key (its EL and SPSel select the TLB and SP).
    pub(crate) fn jit_layout_for(&self, page_key: u64) -> gen::Layout {
        let el = ((page_key >> 1) & 1) as u8;
        let spsel = page_key & 1 != 0;
        self.jit_layout_el(el, spsel)
    }

    pub(crate) fn jit_layout_el(&self, el: u8, spsel: bool) -> gen::Layout {
        let a = |p: *const u8| p as usize as u64;
        let f = &self.fast[el as usize];
        gen::Layout {
            x: a(self.x.as_ptr() as *const u8),
            sp: if el == 0 || !spsel {
                a(std::ptr::addr_of!(self.sp_el0) as *const u8)
            } else {
                a(std::ptr::addr_of!(self.sp_el1) as *const u8)
            },
            pc: a(std::ptr::addr_of!(self.pc) as *const u8),
            n: a(std::ptr::addr_of!(self.n) as *const u8),
            z: a(std::ptr::addr_of!(self.z) as *const u8),
            c: a(std::ptr::addr_of!(self.c) as *const u8),
            v: a(std::ptr::addr_of!(self.v) as *const u8),
            insn_count: a(std::ptr::addr_of!(self.insn_count) as *const u8),
            pending: a(std::ptr::addr_of!(self.jit_pending_flag)),
            excl_armed: a(std::ptr::addr_of!(self.excl_armed)),
            limit: a(std::ptr::addr_of!(self.jit_limit) as *const u8),
            excl_addr: a(std::ptr::addr_of!(self.excl_addr) as *const u8),
            tpidr_el0: a(std::ptr::addr_of!(self.sys.tpidr_el0) as *const u8),
            tpidr_el1: a(std::ptr::addr_of!(self.sys.tpidr_el1) as *const u8),
            tpidrro_el0: a(std::ptr::addr_of!(self.sys.tpidrro_el0) as *const u8),
            sp_el0: a(std::ptr::addr_of!(self.sp_el0) as *const u8),
            daif: a(std::ptr::addr_of!(self.daif) as *const u8),
            vreg: a(self.vreg.as_ptr() as *const u8),
            cpacr: a(std::ptr::addr_of!(self.sys.cpacr) as *const u8),
            fpcr: a(std::ptr::addr_of!(self.sys.fpcr) as *const u8),
            fpsr: a(std::ptr::addr_of!(self.sys.fpsr) as *const u8),
            el: el as u64,
            spsel: spsel as u64,
            irq_dirty: a(self.attn_ptr()),
            // The threaded build's memory is shared, so blocks must import a shared memory even
            // with one CPU (the non-shared import type does not match it).
            smp: (cfg!(target_feature = "atomics") || self.machine.ncpus > 1) as u64,
            mem: a(self.mem.as_ptr()),
            read_tag: a(f.read_tag_ptr()),
            read_base: a(f.read_base_ptr()),
            write_tag: a(f.write_tag_ptr()),
            write_base: a(f.write_base_ptr()),
        }
    }

    #[cfg(not(any(target_arch = "wasm32", feature = "native-jit")))]
    fn jit_run(&mut self, _slot: u32, _budget: u64) -> JitStep {
        JitStep::NotCompiled
    }

    #[cfg(any(target_arch = "wasm32", feature = "native-jit"))]
    #[allow(unsafe_code)]
    fn jit_run(&mut self, slot: u32, budget: u64) -> JitStep {
        let before = self.insn_count;
        self.jit_limit = before.wrapping_add(budget.clamp(1, CHAIN_BUDGET));
        let pc = self.pc;
        // SAFETY: the table entry the host filled with this slot's compiled function, of this type
        #[cfg(target_arch = "wasm32")]
        let status = unsafe {
            let f: extern "C" fn(i64) -> i32 = std::mem::transmute((SLOT_BASE + slot) as usize);
            f(pc as i64)
        };
        #[cfg(not(target_arch = "wasm32"))]
        let status = {
            let f = self.jit.native_fns[slot as usize];
            if f == 0 {
                return JitStep::NotCompiled;
            }
            self.jit.slot_used[slot as usize] = true;
            // The regions the function was compiled with: its page's EL is this one (the slot was
            // found by a key with it)
            let b = self.jit_bases(self.el as usize).0;
            let previous = CURRENT.replace(self as *mut Self as *mut ());
            // SAFETY: `f` is the function compiled for this slot, in the module the JIT holds; the
            // block reaches this CPU through the addresses in its layout and the region starts it
            // is passed (the same ones), and its helpers through CURRENT, while nothing else uses
            // the CPU.
            let status = unsafe {
                type Block = extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i32;
                let f: Block = std::mem::transmute(f);
                f(
                    pc as i64,
                    b[0].0 as i64,
                    b[1].0 as i64,
                    b[2].0 as i64,
                    b[3].0 as i64,
                    b[4].0 as i64,
                    b[5].0 as i64,
                )
            };
            CURRENT.set(previous);
            status
        };
        self.jit.block_runs += 1;
        if self.jit.last_ran_block {
            self.jit.block_to_block += 1;
        }
        self.jit.last_ran_block = true;
        if let Some(e) = self.jit.exits.get_mut(status as usize) {
            *e += 1;
        }
        let ran = self.insn_count.wrapping_sub(before);
        self.jit.block_insns += ran;
        match status {
            gen::STATUS_DONE | gen::STATUS_REDIRECT => JitStep::Ran(ran),
            gen::STATUS_WFI => JitStep::Wfi(ran),
            _ => match self.jit_take_pending() {
                Ok(()) => JitStep::Ran(ran),
                Err(r) => JitStep::Stop(r),
            },
        }
    }

    /// Handle the exception a block left pending (cpu.pc = faulting instruction).
    #[cfg_attr(
        not(any(target_arch = "wasm32", feature = "native-jit")),
        allow(dead_code)
    )]
    fn jit_take_pending(&mut self) -> Result<(), StopReason> {
        self.jit_pending_flag = 0;
        match self.jit_pending.take() {
            Some(Event::Sync {
                ec,
                iss,
                far,
                ret_next,
            }) => {
                let pc = self.pc;
                let ret = if ret_next { pc.wrapping_add(4) } else { pc };
                if ec == crate::cpu::ec::UNKNOWN {
                    if let Ok(insn) = self.read_code_word(pc) {
                        self.last_undefined = Some((pc, insn));
                        if self.trap_undefined {
                            return Err(StopReason::Undefined { pc, insn });
                        }
                    }
                }
                self.sync_exception(ec, iss, far, ret)
            },
            Some(Event::Stop(r)) => Err(r),
            _ => Ok(()),
        }
    }

    #[cfg_attr(
        not(any(target_arch = "wasm32", feature = "native-jit")),
        allow(dead_code)
    )]
    fn read_code_word(&mut self, pc: u64) -> Res<u32> {
        self.read(pc, 4, false).map(|v| v as u32)
    }

    // ------------------------------------------------- helpers for blocks

    pub fn jit_helper_read(&mut self, va: u64, size: u32) -> u64 {
        match self.read(va, size, false) {
            Ok(v) => v,
            Err(e) => {
                self.jit_pending = Some(e);
                self.jit_pending_flag = 1;
                0
            },
        }
    }

    pub fn jit_helper_write(&mut self, va: u64, size: u32, value: u64) {
        if let Err(e) = self.write(va, size, value, false) {
            self.jit_pending = Some(e);
            self.jit_pending_flag = 1;
        }
    }

    /// One lane of a three-same floating-point vector instruction for a block (`advsimd::fp3`):
    /// `op` the operation plus 256 for double precision, with FPCR and FPSR as the
    /// interpreter has them.
    pub fn jit_helper_fp3(&mut self, op: u32, x: u64, y: u64, d: u64) -> u64 {
        self.jit.helper_fp_lanes += 1;
        let mut fpsr = self.sys.fpsr;
        let mut st = crate::fp::Fpst {
            fpcr: self.sys.fpcr,
            fpsr: &mut fpsr,
        };
        let r = crate::advsimd::fp3_helper(&mut st, op, x, y, d).unwrap_or(0);
        self.sys.fpsr = fpsr;
        r
    }

    /// Interpret one instruction for a block. See `gen` for the status codes.
    pub fn jit_helper_interp(&mut self, insn: u32, pc: u64) -> i32 {
        self.jit.helper_interps += 1;
        if let Some(h) = self.jit.helper_hist.as_mut() {
            // By opcode: the register fields (Rd, Rn, Rm) masked out
            let e = h.entry(insn & 0xffe0_fc00).or_insert((0, insn));
            e.0 += 1;
        }
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        let system = insn & 0xffc0_0000 == 0xd500_0000;
        match self.execute(insn) {
            Ok(()) => {
                if self.next_pc != pc.wrapping_add(4) || (system && !self.system_continues(insn)) {
                    // Control flow or state that translated code depends on
                    // (EL, SPSel, translation, unmasked interrupts) changed:
                    // end the block and continue in the run loop.
                    self.pc = self.next_pc;
                    gen::STATUS_REDIRECT
                } else {
                    gen::STATUS_DONE
                }
            },
            Err(Event::Wfi) => {
                self.pc = self.next_pc;
                gen::STATUS_WFI
            },
            Err(e) => {
                self.pc = pc;
                self.jit_pending = Some(e);
                self.jit_pending_flag = 1;
                gen::STATUS_PENDING
            },
        }
    }

    /// After a system instruction ran in the interpreter: can the block
    /// continue? Reads, cache/TLB maintenance, barriers and hints change
    /// nothing translated code depends on (it reads the fast TLB from memory),
    /// nor do writes of a few plain registers, or of DAIF that keep IRQs
    /// masked. Anything else ends the block.
    fn system_continues(&self, insn: u32) -> bool {
        use crate::sysreg::{self as sr};
        if insn & (1 << 21) != 0 {
            return true; // MRS, SYSL
        }
        let op0 = (insn >> 19) & 3;
        let crn = (insn >> 12) & 0xf;
        match op0 {
            1 => true, // SYS: DC, IC, TLBI, AT
            0 => match crn {
                2 | 3 => true, // hints, barriers, CLREX
                // MSR (immediate): DAIFSet/DAIFClr may continue while IRQs stay masked
                4 => {
                    (insn >> 16) & 7 == 3
                        && matches!((insn >> 5) & 7, 6 | 7)
                        && self.daif & (1 << 7) != 0
                },
                _ => false,
            },
            _ => {
                let key = (insn >> 5) & 0xffff;
                match key {
                    sr::DAIF => self.daif & (1 << 7) != 0,
                    sr::NZCV
                    | sr::FPCR
                    | sr::FPSR
                    | sr::TPIDR_EL0
                    | sr::TPIDR_EL1
                    | sr::TPIDRRO_EL0
                    | sr::SP_EL0 => true,
                    sr::SPSR_EL1
                    | sr::ELR_EL1
                    | sr::ESR_EL1
                    | sr::FAR_EL1
                    | sr::PAR_EL1
                    | sr::CONTEXTIDR_EL1 => true,
                    _ => false,
                }
            },
        }
    }

    /// Run loop glue: returns Some(exit) if the run loop must return.
    #[inline(always)]
    pub(crate) fn jit_dispatch(&mut self, executed: &mut u64, budget: u64) -> Option<Option<Exit>> {
        match self.jit_step(budget.saturating_sub(*executed)) {
            JitStep::NotCompiled => {
                if self.jit.yielded {
                    self.jit.yielded = false;
                    return Some(Some(Exit::Budget));
                }
                None
            },
            JitStep::Ran(n) => {
                *executed += n.max(1);
                Some(None)
            },
            JitStep::Wfi(n) => {
                *executed += n.max(1);
                self.update_timers();
                self.sync_host_irqs_pub();
                if !self.irq_asserted() {
                    return Some(Some(Exit::Idle {
                        wake_ns: self.next_timer_deadline_ns(),
                    }));
                }
                Some(None)
            },
            JitStep::Stop(r) => Some(Some(Exit::Stop(r))),
        }
    }
}

// The native JIT's helpers: compiled blocks call them like the browser's imports, and they find the
// CPU running the block through CURRENT (set by `jit_run`).
#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
thread_local! {
    static CURRENT: std::cell::Cell<*mut ()> = const { std::cell::Cell::new(std::ptr::null_mut()) };
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
fn current<H: Host>() -> *mut Cpu<H> {
    CURRENT.get() as *mut Cpu<H>
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
#[allow(unsafe_code)]
extern "C" fn native_read<H: Host>(va: i64, size: i32) -> i64 {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_read(va as u64, size as u32) as i64
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
#[allow(unsafe_code)]
extern "C" fn native_write<H: Host>(va: i64, size: i32, value: i64) {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_write(va as u64, size as u32, value as u64)
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
#[allow(unsafe_code)]
extern "C" fn native_fp3<H: Host>(op: i32, x: i64, y: i64, d: i64) -> i64 {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_fp3(op as u32, x as u64, y as u64, d as u64) as i64
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
#[allow(unsafe_code)]
extern "C" fn native_interp<H: Host>(insn: i32, pc: i64) -> i32 {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_interp(insn as u32, pc as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with a JIT that takes the compile requests.
    struct Takes;
    impl Host for Takes {
        fn mmio_read(&mut self, _: u64, _: u32) -> Option<u64> {
            None
        }
        fn mmio_write(&mut self, _: u64, _: u32, _: u64) -> bool {
            false
        }
        fn now_ns(&mut self) -> u64 {
            0
        }
        fn wall_clock_secs(&mut self) -> u64 {
            0
        }
        fn jit_compile(&mut self, _: u32, _: &[u8]) -> bool {
            true
        }
    }

    const CODE: u64 = 0x10000;
    const THRESHOLD: u32 = 64;

    /// A machine whose entry at `CODE` has been counted to just below the threshold, with `info` the
    /// page's info (what the other contexts, and the compiles in flight, made of it); `jit_step` is then
    /// run once more, and the count the entry has after it is returned.
    #[allow(unsafe_code)]
    fn count_after_a_step(info: PageInfo) -> u32 {
        let mut c = Cpu::new(4 << 20, Takes);
        c.jit.enabled = true;
        c.jit.threshold = THRESHOLD;
        c.mem.write(CODE as usize, &0x1400_0000u32.to_le_bytes());
        c.pc = CODE;
        // The first step decodes the entry and counts it once.
        assert!(matches!(c.jit_step(1), JitStep::NotCompiled));
        c.jit
            .pre
            .set_count(CODE as usize & !0xfff, 0, THRESHOLD - 1);
        c.jit
            .pages
            .insert(page_of(Jit::key(CODE, c.el, c.spsel)), info);
        assert!(matches!(c.jit_step(1), JitStep::NotCompiled));
        let ops = c.jit.pre.enter(CODE as usize);
        assert!(!ops.is_null());
        // SAFETY: `ops` is the ops of the page at CODE, a live slot of 1024 ops or more.
        unsafe { (*ops).cnt as u32 }
    }

    /// The count of an entry is in the predecoded slot, shared by every context that runs the code, so a
    /// refusal that does not pass (the page has all the entries it may, in one context) must not take it
    /// from the others, while one that does (a compile in flight) counts the entry again from below.
    #[test]
    fn a_refusal_that_does_not_pass_leaves_the_count_and_one_that_passes_resets_it() {
        // The page has as many entries as it may, none of them this one: declined, the count stays.
        let full = PageInfo {
            slot: Some(0),
            entries: (1..=MAX_ENTRIES as u16).collect(),
            ..PageInfo::default()
        };
        assert!(full.covered() >= MAX_ENTRIES && !full.covers(0));
        assert_eq!(count_after_a_step(full), THRESHOLD);
        // A compile that has the entry is in flight: waiting, the count is back above the warm one.
        let flying = PageInfo {
            compiling: Some(0),
            compiling_entries: vec![0],
            ..PageInfo::default()
        };
        assert_eq!(count_after_a_step(flying), THRESHOLD.div_ceil(4) + 1);
    }

    const SUBS_X1: u32 = 0xf100_0421; // subs x1, x1, #1
    const B_NE_BACK: u32 = 0x54ff_ffe1; // b.ne .-4
    const B_EQ_BACK: u32 = 0x54ff_ffe0; // b.eq .-4
    const MOVZ_X0_7: u32 = 0xd280_00e0;
    const MOVZ_X0_8: u32 = 0xd280_0100;
    const MOVZ_X2_9: u32 = 0xd280_0122;

    /// A page whose compiled code covers words that a write misses: the ops decoded from the words written go (a
    /// compare that was decoded with the branch after it, whichever of the two is written), the others are kept.
    #[test]
    fn a_write_that_misses_the_compiled_words_drops_the_ops_of_the_words_it_writes() {
        let mut c = Cpu::new(4 << 20, Takes);
        let w = |c: &Cpu<Takes>, i: usize, insn: u32| {
            c.mem.write(0x1000 + 4 * i, &insn.to_le_bytes());
        };
        for (i, insn) in [SUBS_X1, B_NE_BACK, MOVZ_X0_7, MOVZ_X2_9]
            .into_iter()
            .enumerate()
        {
            w(&c, i, insn);
        }
        // Two passes of the loop, then the two moves: six instructions.
        c.pc = 0x1000;
        c.x[1] = 2;
        c.run(6);
        assert_eq!((c.x[0], c.x[1], c.x[2]), (7, 0, 9));
        assert_eq!(c.jit.code_page[1], 2, "decoded, not compiled");
        // The page has compiled code, which covers word 40 only.
        c.jit.code_page[1] |= 1;
        let mut bits = Box::new([0u64; 16]);
        bits[0] = 1 << 40;
        c.jit.covered.insert(1, bits);

        // A move is rewritten.
        w(&c, 2, MOVZ_X0_8);
        c.jit.code_written(0x1008, 4);
        assert_eq!(c.jit.code_writes_kept, 1);
        c.pc = 0x1000;
        c.x[1] = 2;
        c.x[0] = 0;
        c.run(6);
        assert_eq!((c.x[0], c.x[1]), (8, 0));

        // The branch is rewritten: the compare before it was fused with the old one.
        w(&c, 1, B_EQ_BACK);
        c.jit.code_written(0x1004, 4);
        assert_eq!(c.jit.code_writes_kept, 2);
        c.pc = 0x1000;
        c.x[1] = 2;
        c.x[0] = 0;
        c.run(4);
        assert_eq!((c.x[0], c.x[1], c.pc), (8, 1, 0x1010));

        // The compare is rewritten.
        w(&c, 0, 0xf100_0821); // subs x1, x1, #2
        c.jit.code_written(0x1000, 4);
        assert_eq!(c.jit.code_writes_kept, 3);
        c.pc = 0x1000;
        c.x[1] = 5;
        c.run(4);
        assert_eq!((c.x[1], c.pc), (3, 0x1010));

        // A write to a word the compiled code covers drops the page.
        c.jit.covered.get_mut(&1).unwrap()[0] |= 1 << 3;
        w(&c, 3, MOVZ_X0_7);
        c.jit.code_written(0x100c, 4);
        assert_eq!(c.jit.code_writes_kept, 3);
        assert_eq!(c.jit.code_page[1], 0);
        assert_eq!(c.jit.pre.slot(1), crate::predecode::NONE);
    }
}
