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

pub mod gen;
#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
pub mod native;
pub mod wasm;

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
    lookup_key: Vec<u64>,
    lookup_slot: Vec<u32>,
    hot_key: Vec<u64>,
    hot_count: Vec<u32>,
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
    /// Statistics
    pub compiled: u64,
    pub invalidated: u64,
    pub block_runs: u64,
    /// Full flushes (slots exhausted, reset, configuration).
    pub flushes: u64,
    /// Pages dropped because they were written.
    pub page_invalidations: u64,
    /// Instructions retired inside modules, and those of them interpreted.
    pub block_insns: u64,
    pub helper_interps: u64,
    /// The native JIT's compiled code (None: pages go to the host, i.e. the browser) and the
    /// address of each slot's function in it.
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub(crate) native: Option<native::Native>,
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    native_fns: Vec<usize>,
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
            threshold: 1024,
            lookup_key: vec![u64::MAX; LOOKUP],
            lookup_slot: vec![0; LOOKUP],
            hot_key: vec![u64::MAX; LOOKUP],
            hot_count: vec![0; LOOKUP],
            pages: HashMap::new(),
            slot_page: vec![u64::MAX; SLOTS as usize],
            slot_state: vec![SlotState::Free; SLOTS as usize],
            free: (0..SLOTS).rev().collect(),
            page_keys: HashMap::new(),
            code_page: vec![0; memory_size.div_ceil(4096)],
            pre: crate::predecode::Cache::new(memory_size),
            compiled: 0,
            invalidated: 0,
            block_runs: 0,
            flushes: 0,
            page_invalidations: 0,
            block_insns: 0,
            helper_interps: 0,
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            native: None,
            #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
            native_fns: vec![0; SLOTS as usize],
        }
    }

    #[inline(always)]
    pub fn key(pa: u64, el: u8, spsel: bool) -> u64 {
        (pa << 2) | ((el as u64) << 1) | spsel as u64
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

    /// Count an execution of the entry at `key`. Returns 1 when it became
    /// warm (worth including in its page's next module) and 2 when it is hot
    /// (its page should be compiled now).
    #[inline(always)]
    fn touch(&mut self, key: u64) -> u32 {
        let i = hash(key);
        if self.hot_key[i] != key {
            self.hot_key[i] = key;
            self.hot_count[i] = 0;
        }
        self.hot_count[i] += 1;
        let c = self.hot_count[i];
        if c >= self.threshold {
            2
        } else if c == self.threshold.div_ceil(4) {
            1
        } else {
            0
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

    /// Drop all modules of a physical page.
    pub fn invalidate_page(&mut self, page: usize) {
        if page >= self.code_page.len() || self.code_page[page] == 0 {
            return;
        }
        self.code_page[page] = 0;
        self.page_invalidations += 1;
        self.pre.drop_page(page);
        if let Some(keys) = self.page_keys.remove(&(page as u32)) {
            for k in keys {
                self.remove_page(k);
            }
        }
    }

    pub fn invalidate_range(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let first = (start >> 12) as usize;
        let last = ((start + len - 1) >> 12) as usize;
        for p in first..=last.min(self.code_page.len().saturating_sub(1)) {
            self.invalidate_page(p);
        }
    }

    pub fn flush_all(&mut self) {
        self.flushes += 1;
        let keys: Vec<u64> = self.pages.keys().copied().collect();
        for k in keys {
            self.remove_page(k);
        }
        self.page_keys.clear();
        self.code_page.fill(0);
        self.pre.clear();
        self.hot_key.fill(u64::MAX);
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

    /// Run translated code at the current PC if there is any.
    #[inline(always)]
    pub(crate) fn jit_step(&mut self, budget: u64) -> JitStep {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if self.jit.native.as_ref().is_some_and(|n| n.has_done()) {
            self.jit_take_compiled();
        }
        let Some(pa) = self.jit_pc_pa() else {
            return JitStep::NotCompiled;
        };
        let key = Jit::key(pa, self.el, self.spsel);
        if let Some(slot) = self.jit.ready_slot(key) {
            return self.jit_run(slot, budget);
        }
        match self.jit.touch(key) {
            0 => {},
            1 => self.jit_warm(key),
            _ => {
                if self.jit_compile(key) && self.jit.sync_compile && self.jit.enabled {
                    self.jit.yielded = true;
                }
            },
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
    /// it is in flight. Returns true if a compile was started.
    #[cold]
    fn jit_compile(&mut self, key: u64) -> bool {
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
            return false;
        }
        if info.covers(off) {
            return false;
        }
        let busy = info.compiling.is_some();
        if busy && !additive {
            if !info.warm.contains(&off) {
                info.warm.push(off);
            }
            return false;
        }
        if info.covered() >= MAX_ENTRIES {
            // The page has as many compiled entries as it may: further ones stay interpreted.
            return false;
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
            return false;
        };
        // flush_all may have removed the page info
        let page_pa = (page_key >> 2) as usize;
        if page_pa + 0x1000 > self.mem.len() {
            self.jit.free.push(slot);
            return false;
        }
        let words: Vec<u32> = self.mem[page_pa..page_pa + 0x1000]
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let layout = self.jit_layout_for(page_key);
        let code = gen::translate_page_code(&layout, &words, &entries, MAX_BLOCK);

        let page = page_pa >> 12;
        self.jit.code_page[page] |= 1;
        self.machine.code_pages[page].store(1, std::sync::atomic::Ordering::Relaxed);
        let keys = self.jit.page_keys.entry(page as u32).or_default();
        if !keys.contains(&page_key) {
            keys.push(page_key);
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
        self.jit.compiled += 1;
        // Stores to this page must take the slow path from now on.
        for f in self.fast.iter_mut() {
            f.forget_write_page(page_pa);
        }
        if !self.jit_compile_code(slot, &code, layout.smp != 0) {
            // Nothing can compile it: give up on the JIT.
            self.jit.enabled = false;
            self.jit.flush_all();
            return false;
        }
        true
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
    fn jit_compile_code(&mut self, slot: u32, code: &wasm::Code, smp: bool) -> bool {
        #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
        if let Some(n) = self.jit.native.as_mut() {
            // On a thread of its own: `jit_take_compiled` marks the slot ready when it is. Until
            // then the slot must not keep a function from its previous page.
            self.jit.native_fns[slot as usize] = 0;
            return n.submit(slot, code.b.clone());
        }
        let wasm = wasm::module(&gen::imports(), gen::LOCALS, code, smp);
        self.host.jit_compile(SLOT_BASE + slot, &wasm)
    }

    /// The native compiler's statistics: (functions compiled, nanoseconds spent compiling).
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn native_jit_stats(&self) -> (u64, u64) {
        self.jit.native.as_ref().map_or((0, 0), |n| n.stats())
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
            self.jit.ready(SLOT_BASE + done.slot);
        }
    }

    /// Turn the JIT on with the native compiler (false where the host has no Cranelift backend).
    #[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
    pub fn enable_native_jit(&mut self) -> bool {
        let helpers = native::Helpers {
            read: native_read::<H> as *const () as usize,
            write: native_write::<H> as *const () as usize,
            interp: native_interp::<H> as *const () as usize,
        };
        self.jit.native = native::Native::new(gen::LOCALS, helpers);
        self.jit.enabled = self.jit.native.is_some();
        self.jit_entry = true;
        self.jit.enabled
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
            el: el as u64,
            spsel: spsel as u64,
            irq_dirty: a(self.attn_ptr()),
            smp: (self.machine.ncpus > 1) as u64,
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
    fn jit_run(&mut self, slot: u32, budget: u64) -> JitStep {
        let before = self.insn_count;
        self.jit_limit = before.wrapping_add(budget.clamp(1, CHAIN_BUDGET));
        let pc = self.pc;
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
            // SAFETY: `f` is the function compiled for this slot, in the module the JIT holds; the
            // block reaches this CPU through the addresses in its layout, and its helpers through
            // CURRENT, while nothing else uses the CPU.
            let previous = CURRENT.replace(self as *mut Self as *mut ());
            let status = unsafe {
                let f: extern "C" fn(i64) -> i32 = std::mem::transmute(f);
                f(pc as i64)
            };
            CURRENT.set(previous);
            status
        };
        self.jit.block_runs += 1;
        let ran = self.insn_count.wrapping_sub(before);
        self.jit.block_insns += ran;
        match status {
            0 | 1 => JitStep::Ran(ran),
            3 => JitStep::Wfi(ran),
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
                self.sync_exception_pub(ec, iss, far, ret)
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

    /// Interpret one instruction for a block. See `gen` for the status codes.
    pub fn jit_helper_interp(&mut self, insn: u32, pc: u64) -> i32 {
        self.jit.helper_interps += 1;
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
                    1
                } else {
                    0
                }
            },
            Err(Event::Wfi) => {
                self.pc = self.next_pc;
                3
            },
            Err(e) => {
                self.pc = pc;
                self.jit_pending = Some(e);
                self.jit_pending_flag = 1;
                2
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
extern "C" fn native_read<H: Host>(va: i64, size: i32) -> i64 {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_read(va as u64, size as u32) as i64
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
extern "C" fn native_write<H: Host>(va: i64, size: i32, value: i64) {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_write(va as u64, size as u32, value as u64)
}

#[cfg(all(not(target_arch = "wasm32"), feature = "native-jit"))]
extern "C" fn native_interp<H: Host>(insn: i32, pc: i64) -> i32 {
    // SAFETY: only blocks call the helpers, and only from `jit_run`, which set CURRENT to its CPU
    // (of this H) for the duration of the call.
    let cpu = unsafe { &mut *current::<H>() };
    cpu.jit_helper_interp(insn as u32, pc as u64)
}
