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
    /// One byte per physical page of RAM: non-zero if it holds compiled code.
    pub code_page: Vec<u8>,
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
            compiled: 0,
            invalidated: 0,
            block_runs: 0,
            flushes: 0,
            page_invalidations: 0,
            block_insns: 0,
            helper_interps: 0,
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
        }
    }

    /// Drop all modules of a physical page.
    pub fn invalidate_page(&mut self, page: usize) {
        if page >= self.code_page.len() || self.code_page[page] == 0 {
            return;
        }
        self.code_page[page] = 0;
        self.page_invalidations += 1;
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
                if info.compiling != Some(slot) {
                    return;
                }
                info.compiling = None;
                let entries = std::mem::take(&mut info.compiling_entries);
                let old = info.slot.replace(slot);
                let old_entries = std::mem::replace(&mut info.entries, entries.clone());
                self.unmap_entries(page_key, &old_entries);
                if let Some(old) = old {
                    self.free_slot(old);
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
        let page_key = page_of(key);
        let off = ((key >> 2) & 0xfff) as u16;
        let info = self.jit.pages.entry(page_key).or_default();
        if let Some(slot) = info.slot {
            if info.entries.binary_search(&off).is_ok() {
                // Compiled, but its lookup entry was evicted by a collision.
                let i = hash(key);
                self.jit.lookup_key[i] = key;
                self.jit.lookup_slot[i] = slot;
                return false;
            }
        }
        if info.compiling.is_some() {
            if !info.compiling_entries.contains(&off) && !info.warm.contains(&off) {
                info.warm.push(off);
            }
            return false;
        }
        if info.entries.len() >= MAX_ENTRIES {
            // The page module is full: further entries stay interpreted.
            return false;
        }
        // The compiled entries, this one, then warm ones while there is room.
        let mut entries: Vec<u16> = info.entries.clone();
        entries.push(off);
        for &w in &info.warm {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            entries.push(w);
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
        let wasm = gen::translate_page(&layout, &words, &entries, MAX_BLOCK);

        let page = page_pa >> 12;
        self.jit.code_page[page] = 1;
        let keys = self.jit.page_keys.entry(page as u32).or_default();
        if !keys.contains(&page_key) {
            keys.push(page_key);
        }
        let info = self.jit.pages.entry(page_key).or_default();
        info.compiling = Some(slot);
        info.compiling_entries = entries;
        self.jit.slot_page[slot as usize] = page_key;
        self.jit.slot_state[slot as usize] = SlotState::Compiling;
        self.jit.compiled += 1;
        // Stores to this page must take the slow path from now on.
        for f in self.fast.iter_mut() {
            f.forget_write_page(page_pa);
        }
        if !self.host.jit_compile(SLOT_BASE + slot, &wasm) {
            // The host cannot compile: give up on the JIT.
            self.jit.enabled = false;
            self.jit.flush_all();
            return false;
        }
        true
    }

    /// Layout for code of the page key (its EL and SPSel select the TLB and SP).
    pub(crate) fn jit_layout_for(&self, page_key: u64) -> gen::Layout {
        let el = ((page_key >> 1) & 1) as u8;
        let spsel = page_key & 1 != 0;
        self.jit_layout_el(el, spsel)
    }

    pub(crate) fn jit_layout_el(&self, el: u8, spsel: bool) -> gen::Layout {
        let a = |p: *const u8| p as usize as u32;
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
            el: el as u32,
            spsel: spsel as u32,
            irq_dirty: a(self.attn_ptr()),
            smp: (self.machine.ncpus > 1) as u32,
            mem: a(self.mem.as_ptr()),
            read_tag: a(f.read_tag_ptr()),
            read_base: a(f.read_base_ptr()),
            write_tag: a(f.write_tag_ptr()),
            write_base: a(f.write_base_ptr()),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn jit_run(&mut self, _slot: u32, _budget: u64) -> JitStep {
        JitStep::NotCompiled
    }

    #[cfg(target_arch = "wasm32")]
    fn jit_run(&mut self, slot: u32, budget: u64) -> JitStep {
        let before = self.insn_count;
        self.jit_limit = before.wrapping_add(budget.clamp(1, CHAIN_BUDGET));
        let pc = self.pc;
        let status = unsafe {
            let f: extern "C" fn(i64) -> i32 = std::mem::transmute((SLOT_BASE + slot) as usize);
            f(pc as i64)
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
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
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

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
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
