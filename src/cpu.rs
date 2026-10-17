//! AArch64 CPU state, the run loop, exceptions, interrupts and the physical bus.
//!
//! The instruction semantics live in `a64.rs`, address translation in `mmu.rs`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::board::{self, Host};
use crate::mmu::Tlb;
use crate::smp::{self, Machine};

/// Exception classes used in ESR_EL1.EC.
pub mod ec {
    pub const UNKNOWN: u32 = 0x00;
    pub const WFX: u32 = 0x01;
    pub const FP_ACCESS: u32 = 0x07;
    pub const ILLEGAL_STATE: u32 = 0x0e;
    pub const SVC64: u32 = 0x15;
    pub const HVC64: u32 = 0x16;
    pub const SMC64: u32 = 0x17;
    pub const SYS64: u32 = 0x18;
    pub const IABT_LOW: u32 = 0x20;
    pub const IABT_CUR: u32 = 0x21;
    pub const PC_ALIGN: u32 = 0x22;
    pub const DABT_LOW: u32 = 0x24;
    pub const DABT_CUR: u32 = 0x25;
    pub const SP_ALIGN: u32 = 0x26;
    pub const BRK64: u32 = 0x3c;
}

/// Why the CPU stopped for good (or until the host intervenes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// PSCI SYSTEM_OFF / CPU_OFF.
    PowerOff,
    /// PSCI SYSTEM_RESET.
    Reset,
    /// HLT #imm — used by bare-metal tests to signal the end of a run.
    Halt(u16),
    /// Undefined instruction while `trap_undefined` is set (debugging aid).
    Undefined { pc: u64, insn: u32 },
    /// An exception was taken while VBAR_EL1 is 0 and the MMU is off; with no
    /// vector table this would just spin, so the core stops and reports it.
    UnhandledException { esr: u64, elr: u64, far: u64 },
}

/// Result of a `run` slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// The instruction budget ran out; call `run` again.
    Budget,
    /// The guest executed WFI with nothing pending. The host should wait until
    /// `wake_ns` (host clock) or until a device raises an interrupt.
    Idle {
        wake_ns: Option<u64>,
    },
    Stop(StopReason),
}

/// Control-flow outcomes of executing one instruction other than "fall through".
#[derive(Debug)]
pub enum Event {
    /// Synchronous exception to EL1. `ret_next` selects the preferred return
    /// address: the next instruction (SVC/HVC/SMC) or this one (faults).
    Sync {
        ec: u32,
        iss: u32,
        far: Option<u64>,
        ret_next: bool,
    },
    Wfi,
    Stop(StopReason),
}

pub type Res<T> = Result<T, Event>;

impl Event {
    pub fn undefined() -> Event {
        Event::Sync {
            ec: ec::UNKNOWN,
            iss: 0,
            far: None,
            ret_next: false,
        }
    }
}

/// EL1 system registers held in fields; anything else lives in `other`.
#[derive(Clone, Default)]
pub struct SysRegs {
    pub sctlr: u64,
    pub actlr: u64,
    pub cpacr: u64,
    pub ttbr0: u64,
    pub ttbr1: u64,
    pub tcr: u64,
    pub spsr: u64,
    pub elr: u64,
    pub esr: u64,
    pub far: u64,
    pub par: u64,
    pub mair: u64,
    pub amair: u64,
    pub vbar: u64,
    pub contextidr: u64,
    pub tpidr_el1: u64,
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
    pub cntkctl: u64,
    pub cntv_ctl: u64,
    pub cntv_cval: u64,
    pub cntp_ctl: u64,
    pub cntp_cval: u64,
    pub mdscr: u64,
    pub csselr: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub afsr0: u64,
    pub afsr1: u64,
    pub other: std::collections::BTreeMap<u32, u64>,
}

pub struct Cpu<H: Host> {
    /// X0..X30. Register 31 is SP or XZR depending on the instruction.
    pub x: [u64; 31],
    pub pc: u64,
    pub sp_el0: u64,
    pub sp_el1: u64,
    pub n: bool,
    pub z: bool,
    pub c: bool,
    pub v: bool,
    /// PSTATE.DAIF in its SPSR position (bits 9..6).
    pub daif: u32,
    /// Current exception level: 0 or 1.
    pub el: u8,
    /// PSTATE.SPSel at EL1: true = SP_EL1, false = SP_EL0.
    pub spsel: bool,
    pub sys: SysRegs,
    /// SIMD&FP registers V0..V31.
    pub vreg: [u128; 32],

    /// Guest physical RAM, starting at physical address 0.
    /// An alias of `machine.mem`.
    pub mem: crate::ram::Ram,
    /// What the CPUs share (GIC, RTC, timer, power and TLB coordination).
    pub machine: Arc<Machine>,
    /// This CPU's number (MPIDR Aff0, GIC CPU interface).
    pub index: usize,
    /// `machine.cpus[index].attn`, the "look at shared state" flag.
    attn: *const AtomicU8,
    pub host: H,
    pub(crate) tlb: Tlb,
    /// Host fast paths (RAM pages only), one set per exception level:
    /// a direct-mapped read and write TLB, and the current code page.
    pub(crate) fast: [FastTlb; 2],

    /// Exclusive monitor: armed (non-zero) by a load-exclusive of physical
    /// address `excl_addr`. Plain fields so JIT-generated code can use them.
    pub(crate) excl_armed: u8,
    pub(crate) excl_addr: u64,
    /// With several CPUs, exclusives compare values: what the load saw.
    pub(crate) excl_value: u64,
    pub(crate) excl_value2: u64,
    /// TLB generation that a DSB must wait for every CPU to apply (0: none).
    pub(crate) tlb_sync_gen: u64,
    /// Instruction-cache invalidations (Machine::code_inval) applied so far.
    pub(crate) code_seen: u64,
    /// Size of the last load-exclusive (what WFE watches), and the WFE event
    /// register (set by SEVL/SEV).
    pub(crate) excl_size: u32,
    pub(crate) event_register: bool,
    /// The PC is a block entry (target of a taken branch, exception or
    /// return, or the instruction after a JIT block): only there does the
    /// JIT look for or count blocks.
    pub(crate) jit_entry: bool,
    /// insn_count at which a JIT module stops chaining blocks.
    pub(crate) jit_limit: u64,
    pub(crate) next_pc: u64,
    /// Instructions retired since reset.
    pub insn_count: u64,
    /// Basic-block JIT state (enabled only where the host can compile wasm).
    pub jit: crate::jit::Jit,
    /// The pre-decoded page `step` is executing from: its physical base, its slot in
    /// `jit.pre`, and the cache generation the slot was found in.
    dec_base: usize,
    dec_slot: u32,
    dec_gen: u64,
    /// Exception raised inside a JIT helper, and its flag (read by blocks).
    pub(crate) jit_pending: Option<Event>,
    pub(crate) jit_pending_flag: u8,
    /// Stop instead of raising an exception on undefined instructions.
    pub trap_undefined: bool,
    /// Last undefined instruction seen (for diagnostics), as (pc, insn).
    pub last_undefined: Option<(u64, u32)>,
}

// A CPU moves to its host thread once; the raw pointers it holds point into
// its own `machine`, which it keeps alive.
unsafe impl<H: Host + Send> Send for Cpu<H> {}

const FAST_ENTRIES: usize = 256;
/// Executable pages remembered besides the current one (`code_page`).
const CODE_ENTRIES: usize = 64;

/// Virtual page -> offset of the page in `mem`, for pages known to be RAM
/// with the needed permission at one exception level.
pub(crate) struct FastTlb {
    read_tag: Vec<u64>,
    read_base: Vec<usize>,
    write_tag: Vec<u64>,
    write_base: Vec<usize>,
    pub(crate) code_page: u64,
    pub(crate) code_base: usize,
    code_tag: Vec<u64>,
    code_bases: Vec<usize>,
}

impl FastTlb {
    fn new() -> FastTlb {
        FastTlb {
            read_tag: vec![u64::MAX; FAST_ENTRIES],
            read_base: vec![0; FAST_ENTRIES],
            write_tag: vec![u64::MAX; FAST_ENTRIES],
            write_base: vec![0; FAST_ENTRIES],
            code_page: u64::MAX,
            code_base: 0,
            code_tag: vec![u64::MAX; CODE_ENTRIES],
            code_bases: vec![0; CODE_ENTRIES],
        }
    }

    pub(crate) fn flush(&mut self) {
        self.read_tag.fill(u64::MAX);
        self.write_tag.fill(u64::MAX);
        self.code_page = u64::MAX;
        self.code_tag.fill(u64::MAX);
    }

    /// Remove entries for the virtual page `page` (compared under `mask`).
    pub(crate) fn forget_page(&mut self, page: u64, mask: u64) {
        let i = page as usize & (FAST_ENTRIES - 1);
        // The slot is chosen by the low page bits, which `mask` keeps.
        if self.read_tag[i] & mask == page {
            self.read_tag[i] = u64::MAX;
        }
        if self.write_tag[i] & mask == page {
            self.write_tag[i] = u64::MAX;
        }
        if self.code_page & mask == page {
            self.code_page = u64::MAX;
        }
        let c = page as usize & (CODE_ENTRIES - 1);
        if self.code_tag[c] & mask == page {
            self.code_tag[c] = u64::MAX;
        }
    }

    /// Remove write entries mapping to the physical page at `base`.
    pub(crate) fn forget_write_page(&mut self, base: usize) {
        for i in 0..FAST_ENTRIES {
            if self.write_tag[i] != u64::MAX && self.write_base[i] == base {
                self.write_tag[i] = u64::MAX;
            }
        }
    }

    pub(crate) fn read_tag_ptr(&self) -> *const u8 {
        self.read_tag.as_ptr() as *const u8
    }
    pub(crate) fn read_base_ptr(&self) -> *const u8 {
        self.read_base.as_ptr() as *const u8
    }
    pub(crate) fn write_tag_ptr(&self) -> *const u8 {
        self.write_tag.as_ptr() as *const u8
    }
    pub(crate) fn write_base_ptr(&self) -> *const u8 {
        self.write_base.as_ptr() as *const u8
    }
}

const DAIF_ALL: u32 = 0xf << 6;
const DAIF_I: u32 = 1 << 7;

impl<H: Host> Cpu<H> {
    /// Arm or clear the exclusive monitor (keeps the JIT's mirror in sync).
    #[inline]
    pub(crate) fn set_excl(&mut self, e: Option<u64>) {
        self.excl_armed = e.is_some() as u8;
        self.excl_addr = e.unwrap_or(0);
    }

    /// The monitored physical address, if the exclusive monitor is armed.
    #[inline(always)]
    pub(crate) fn excl(&self) -> Option<u64> {
        if self.excl_armed != 0 {
            Some(self.excl_addr)
        } else {
            None
        }
    }

    /// A single-CPU machine.
    pub fn new(memory_size: usize, host: H) -> Cpu<H> {
        assert!(
            memory_size as u64 <= board::RAM_MAX,
            "RAM would overlap the MMIO window"
        );
        Self::new_smp(Arc::new(Machine::new(memory_size, 1)), 0, host)
    }

    /// CPU `index` of `machine`. CPU 0 resets the machine and starts
    /// running; the others stay off until PSCI CPU_ON.
    pub fn new_smp(machine: Arc<Machine>, index: usize, host: H) -> Cpu<H> {
        assert!(index < machine.ncpus);
        let memory_size = machine.mem.len();
        let attn = &machine.cpus[index].attn as *const AtomicU8;
        let mut cpu = Cpu {
            x: [0; 31],
            pc: 0,
            sp_el0: 0,
            sp_el1: 0,
            n: false,
            z: false,
            c: false,
            v: false,
            daif: DAIF_ALL,
            el: 1,
            spsel: true,
            sys: SysRegs::default(),
            vreg: [0; 32],
            mem: machine.mem.alias(),
            machine,
            index,
            attn,
            host,
            tlb: Tlb::new(),
            fast: [FastTlb::new(), FastTlb::new()],
            excl_armed: 0,
            excl_addr: 0,
            excl_value: 0,
            excl_value2: 0,
            tlb_sync_gen: 0,
            code_seen: 0,
            excl_size: 8,
            event_register: false,
            jit_entry: true,
            jit_limit: 0,
            next_pc: 0,
            insn_count: 0,
            trap_undefined: false,
            last_undefined: None,
            jit: crate::jit::Jit::new(memory_size),
            dec_base: usize::MAX,
            dec_slot: crate::predecode::NONE,
            dec_gen: 0,
            jit_pending: None,
            jit_pending_flag: 0,
        };
        cpu.reset();
        cpu
    }

    #[inline(always)]
    fn attn(&self) -> &AtomicU8 {
        // Points into `self.machine`, which this CPU keeps alive.
        unsafe { &*self.attn }
    }

    /// Re-evaluate interrupts (and shared state) before the next instruction.
    #[inline(always)]
    pub(crate) fn set_irq_dirty(&self) {
        self.attn().store(1, Ordering::Relaxed);
    }

    pub(crate) fn attn_ptr(&self) -> *const u8 {
        self.attn as *const u8
    }

    /// Tell the CPUs in `mask` that their interrupt state may have changed.
    pub(crate) fn kick(&self, mask: u32) {
        if mask & (1 << self.index) != 0 {
            self.set_irq_dirty();
        }
        if mask & !(1 << self.index) != 0 {
            self.machine.kick_mask(mask, self.index);
        }
    }

    /// Whether this CPU's IRQ input is asserted.
    pub(crate) fn irq_asserted(&self) -> bool {
        self.machine.gic.lock().irq_asserted(self.index)
    }

    /// Architectural reset into EL1h with the MMU off. CPU 0 also resets
    /// what the CPUs share (GIC, timer, power state of the others).
    pub fn reset(&mut self) {
        if self.index == 0 {
            let m = &self.machine;
            *m.gic.lock() = crate::gic::Gic::new(m.ncpus);
            m.timer_ns_base.store(self.host.now_ns(), Ordering::SeqCst);
            m.timer_cnt_base.store(0, Ordering::SeqCst);
            m.stop.store(smp::STOP_NONE, Ordering::SeqCst);
            for (i, c) in m.cpus.iter().enumerate().take(m.ncpus) {
                c.power.store(
                    if i == 0 { smp::POWER_ON } else { smp::POWER_OFF },
                    Ordering::SeqCst,
                );
            }
        }
        self.reset_local();
    }

    /// Reset this CPU's own state only.
    fn reset_local(&mut self) {
        self.x = [0; 31];
        self.vreg = [0; 32];
        self.pc = board::RAM_BASE;
        self.sp_el0 = 0;
        self.sp_el1 = 0;
        self.n = false;
        self.z = false;
        self.c = false;
        self.v = false;
        self.daif = DAIF_ALL;
        self.el = 1;
        self.spsel = true;
        self.sys = SysRegs {
            // RES1 bits of SCTLR_EL1 as reset by firmware; MMU and caches off.
            sctlr: 0x30d0_0800,
            ..SysRegs::default()
        };
        self.tlb_flush();
        self.jit.flush_all();
        self.set_excl(None);
        self.tlb_sync_gen = 0;
        self.machine.cpus[self.index].tlb_seen.store(
            self.machine.tlb_gen.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        self.set_irq_dirty();
    }

    // ---------------------------------------------------------------- registers

    /// Read Xn where 31 is XZR.
    #[inline(always)]
    pub fn xr(&self, r: u32) -> u64 {
        if r == 31 {
            0
        } else {
            self.x[r as usize]
        }
    }

    /// Read Xn where 31 is SP.
    #[inline(always)]
    pub fn xs(&self, r: u32) -> u64 {
        if r == 31 {
            self.sp()
        } else {
            self.x[r as usize]
        }
    }

    /// Write Xn where 31 is XZR (discarded).
    #[inline(always)]
    pub fn wr(&mut self, r: u32, v: u64) {
        if r != 31 {
            self.x[r as usize] = v;
        }
    }

    /// Write Xn where 31 is SP.
    #[inline(always)]
    pub fn ws(&mut self, r: u32, v: u64) {
        if r == 31 {
            self.set_sp(v);
        } else {
            self.x[r as usize] = v;
        }
    }

    #[inline(always)]
    pub fn sp(&self) -> u64 {
        if self.el == 0 || !self.spsel {
            self.sp_el0
        } else {
            self.sp_el1
        }
    }

    #[inline(always)]
    pub fn set_sp(&mut self, v: u64) {
        if self.el == 0 || !self.spsel {
            self.sp_el0 = v;
        } else {
            self.sp_el1 = v;
        }
    }

    pub fn nzcv(&self) -> u64 {
        ((self.n as u64) << 31)
            | ((self.z as u64) << 30)
            | ((self.c as u64) << 29)
            | ((self.v as u64) << 28)
    }

    pub fn set_nzcv(&mut self, v: u64) {
        self.n = v & (1 << 31) != 0;
        self.z = v & (1 << 30) != 0;
        self.c = v & (1 << 29) != 0;
        self.v = v & (1 << 28) != 0;
    }

    /// PSTATE in SPSR format.
    pub fn pstate(&self) -> u64 {
        let m = if self.el == 0 { 0 } else { 4 | self.spsel as u64 };
        self.nzcv() | self.daif as u64 | m
    }

    fn set_pstate(&mut self, spsr: u64) {
        self.set_nzcv(spsr);
        self.daif = (spsr as u32) & DAIF_ALL;
        match spsr & 0xf {
            0b0000 => {
                self.el = 0;
            },
            0b0100 => {
                self.el = 1;
                self.spsel = false;
            },
            0b0101 => {
                self.el = 1;
                self.spsel = true;
            },
            _ => {
                // Illegal return (EL2/EL3 or AArch32): stay at EL1h.
                self.el = 1;
                self.spsel = true;
            },
        }
        self.set_irq_dirty();
    }

    // -------------------------------------------------------------- exceptions

    /// Enter EL1 at `VBAR_EL1 + offset`, saving state the architectural way.
    /// `kind`: 0 sync, 1 IRQ, 2 FIQ, 3 SError.
    pub(crate) fn take_exception(&mut self, kind: u64, ret: u64) {
        let offset = if self.el == 0 {
            0x400
        } else if self.spsel {
            0x200
        } else {
            0
        };
        self.sys.spsr = self.pstate();
        self.sys.elr = ret;
        self.el = 1;
        self.spsel = true;
        self.daif = DAIF_ALL;
        self.set_excl(None);
        self.pc = self.sys.vbar.wrapping_add(offset + kind * 0x80);
        self.set_irq_dirty();
    }

    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn sync_exception_pub(
        &mut self,
        ec: u32,
        iss: u32,
        far: Option<u64>,
        ret: u64,
    ) -> Result<(), StopReason> {
        self.sync_exception(ec, iss, far, ret)
    }

    fn sync_exception(
        &mut self,
        ec: u32,
        iss: u32,
        far: Option<u64>,
        ret: u64,
    ) -> Result<(), StopReason> {
        self.sys.esr = ((ec as u64) << 26) | (1 << 25) | (iss as u64 & 0x1ff_ffff);
        if let Some(f) = far {
            self.sys.far = f;
        }
        if self.sys.vbar == 0 && self.sys.sctlr & 1 == 0 {
            // Nothing installed a vector table: report rather than spin.
            self.sys.elr = ret;
            return Err(StopReason::UnhandledException {
                esr: self.sys.esr,
                elr: ret,
                far: self.sys.far,
            });
        }
        self.take_exception(0, ret);
        Ok(())
    }

    pub(crate) fn eret(&mut self) {
        let spsr = self.sys.spsr;
        let elr = self.sys.elr;
        self.set_pstate(spsr);
        self.next_pc = elr;
        self.set_excl(None);
    }

    // --------------------------------------------------------------- timers

    /// Generic timer counter value (CNTPCT/CNTVCT; CNTVOFF is zero).
    pub fn counter(&mut self) -> u64 {
        let now = self.host.now_ns();
        let m = &self.machine;
        let ns = now.wrapping_sub(m.timer_ns_base.load(Ordering::Relaxed));
        m.timer_cnt_base
            .load(Ordering::Relaxed)
            .wrapping_add(ns / (1_000_000_000 / board::TIMER_FREQ_HZ))
    }

    /// Recompute timer ISTATUS bits and the PPI lines into the GIC.
    pub fn update_timers(&mut self) {
        let cnt = self.counter();
        let fire = |ctl: &mut u64, cval: u64| -> bool {
            let status = *ctl & 1 != 0 && cnt >= cval;
            if status {
                *ctl |= 4;
            } else {
                *ctl &= !4;
            }
            status && *ctl & 2 == 0
        };
        let v = fire(&mut self.sys.cntv_ctl, self.sys.cntv_cval);
        let p = fire(&mut self.sys.cntp_ctl, self.sys.cntp_cval);
        {
            let mut g = self.machine.gic.lock();
            g.set_line(self.index, board::PPI_VIRT_TIMER, v);
            g.set_line(self.index, board::PPI_PHYS_TIMER, p);
        }
        self.set_irq_dirty();
    }

    /// Host time at which the earliest armed timer will fire.
    pub fn next_timer_deadline_ns(&mut self) -> Option<u64> {
        let cnt = self.counter();
        let now = self.host.now_ns();
        let tick_ns = 1_000_000_000 / board::TIMER_FREQ_HZ;
        let mut best: Option<u64> = None;
        for (ctl, cval) in [
            (self.sys.cntv_ctl, self.sys.cntv_cval),
            (self.sys.cntp_ctl, self.sys.cntp_cval),
        ] {
            if ctl & 1 != 0 && ctl & 2 == 0 {
                let ns = if cval <= cnt {
                    now
                } else {
                    now.saturating_add((cval - cnt).saturating_mul(tick_ns))
                };
                best = Some(best.map_or(ns, |b: u64| b.min(ns)));
            }
        }
        best
    }

    /// Raise or lower a GIC input line (SPI numbers are GIC INTIDs, >= 32).
    pub fn set_irq_line(&mut self, intid: u32, level: bool) {
        let mask = self.machine.gic.lock().set_line(self.index, intid, level);
        self.kick(mask);
        self.set_irq_dirty();
    }

    pub(crate) fn sync_host_irqs_pub(&mut self) {
        self.sync_host_irqs();
    }

    fn sync_host_irqs(&mut self) {
        let index = self.index;
        let mut mask = 0;
        {
            let mut gic = self.machine.gic.lock();
            self.host
                .sync_irqs(&mut |intid, level| mask |= gic.set_line(index, intid, level));
        }
        if mask != 0 {
            self.kick(mask);
        }
    }

    fn irq_masked(&self) -> bool {
        self.daif & DAIF_I != 0
    }

    // --------------------------------------------------------------- run loop

    /// Execute up to `budget` instructions.
    pub fn run(&mut self, budget: u64) -> Exit {
        self.machine.cpus[self.index]
            .run_state
            .store(smp::RUN_RUNNING, Ordering::SeqCst);
        if let Some(exit) = self.check_shared() {
            return self.leave(exit);
        }
        self.update_timers();
        let mut executed = 0;
        let mut last_poll = 0;
        while executed < budget {
            if self.attn().load(Ordering::Relaxed) != 0 {
                self.attn().store(0, Ordering::Relaxed);
                if let Some(exit) = self.check_shared() {
                    return self.leave(exit);
                }
                self.sync_host_irqs();
                if !self.irq_masked() && self.irq_asserted() {
                    let ret = self.pc;
                    self.take_exception(1, ret);
                    self.jit_entry = true;
                }
            }
            if self.jit.enabled && self.jit_entry {
                match self.jit_dispatch(&mut executed, budget) {
                    Some(Some(exit)) => return self.leave(exit),
                    Some(None) => {
                        if executed - last_poll >= 0x400 {
                            last_poll = executed;
                            self.update_timers();
                        }
                        continue;
                    },
                    None => {},
                }
            }
            let pc = self.pc;
            let r = self.step();
            self.jit_entry = self.pc != pc.wrapping_add(4);
            match r {
                Ok(()) => {},
                Err(Event::Wfi) => {
                    self.update_timers();
                    self.sync_host_irqs();
                    if let Some(exit) = self.check_shared() {
                        return self.leave(exit);
                    }
                    if !self.irq_asserted() {
                        let wake_ns = self.next_timer_deadline_ns();
                        return self.leave(Exit::Idle { wake_ns });
                    }
                },
                Err(Event::Stop(r)) => return self.leave(Exit::Stop(r)),
                Err(Event::Sync { .. }) => unreachable!("handled in step"),
            }
            executed += 1;
            // Poll the timer every so often; it is cheap but not free.
            if executed - last_poll >= 0x400 {
                last_poll = executed;
                self.update_timers();
            }
        }
        self.leave(Exit::Budget)
    }

    /// Bookkeeping when `run` returns: an idle CPU holds no translations in
    /// flight, so TLB shootdowns need not wait for it.
    pub(crate) fn leave(&mut self, exit: Exit) -> Exit {
        if matches!(exit, Exit::Idle { .. }) {
            self.machine.cpus[self.index]
                .run_state
                .store(smp::RUN_IDLE, Ordering::SeqCst);
        }
        exit
    }

    /// Act on what other CPUs asked of this one: machine stops, power
    /// changes, TLB shootdowns. Returns an exit if `run` must return.
    pub(crate) fn check_shared(&mut self) -> Option<Exit> {
        let m = self.machine.clone();
        match m.stop.load(Ordering::SeqCst) {
            smp::STOP_NONE => {},
            smp::STOP_RESET => return Some(Exit::Stop(StopReason::Reset)),
            _ => return Some(Exit::Stop(StopReason::PowerOff)),
        }
        let me = &m.cpus[self.index];
        match me.power.load(Ordering::SeqCst) {
            smp::POWER_ON => {},
            smp::POWER_ON_PENDING => {
                let entry = me.entry.load(Ordering::SeqCst);
                let context = me.context.load(Ordering::SeqCst);
                self.reset_local();
                self.pc = entry;
                self.x[0] = context;
                self.jit_entry = true;
                me.power.store(smp::POWER_ON, Ordering::SeqCst);
            },
            _ => {
                // Off: wait to be kicked (by CPU_ON).
                return Some(Exit::Idle { wake_ns: None });
            },
        }
        self.apply_tlb_shootdowns();
        self.apply_code_invalidations();
        None
    }

    /// Drop JIT code that another CPU's IC instructions invalidated.
    pub(crate) fn apply_code_invalidations(&mut self) {
        let m = self.machine.clone();
        let c = m.code_inval.lock();
        if c.gen == self.code_seen {
            return;
        }
        if c.gen - self.code_seen > smp::CODE_RING as u64 {
            self.jit.flush_all();
        } else {
            for n in self.code_seen..c.gen {
                match c.ring[n as usize % smp::CODE_RING] {
                    smp::CODE_ALL => self.jit.flush_all(),
                    page => self.jit.invalidate_page(page as usize),
                }
            }
        }
        self.code_seen = c.gen;
    }

    /// Flush the TLB if another CPU broadcast an invalidation since we last did.
    pub(crate) fn apply_tlb_shootdowns(&mut self) {
        if self.machine.ncpus == 1 {
            return;
        }
        let m = self.machine.clone();
        let seen = m.cpus[self.index].tlb_seen.load(Ordering::Relaxed);
        if m.tlb_gen.load(Ordering::SeqCst) == seen {
            return;
        }
        let ring = m.tlb_ring.lock();
        let gen = m.tlb_gen.load(Ordering::SeqCst);
        if gen - seen > smp::TLB_RING as u64 {
            self.tlb_flush();
        } else {
            for n in seen..gen {
                match ring[n as usize % smp::TLB_RING] {
                    smp::TLB_ALL => {
                        self.tlb_flush();
                        break;
                    },
                    page => self.tlb_flush_page(page),
                }
            }
        }
        drop(ring);
        m.cpus[self.index].tlb_seen.store(gen, Ordering::SeqCst);
    }

    /// A TLBI ... IS: invalidate here and on every other CPU. The other CPUs
    /// flush at their next instruction boundary; a later DSB waits for that.
    /// `page`: the VA page (TLBI by VA), or TLB_ALL.
    pub(crate) fn tlbi_broadcast(&mut self, page: u64) {
        if page == smp::TLB_ALL {
            self.tlb_flush();
        } else {
            self.tlb_flush_page(page);
        }
        if self.machine.ncpus == 1 {
            return;
        }
        let m = self.machine.clone();
        m.stats[0].fetch_add(1, Ordering::Relaxed);
        let gen = {
            let mut ring = m.tlb_ring.lock();
            let n = m.tlb_gen.load(Ordering::SeqCst);
            // Apply what other CPUs broadcast since we last looked, under the
            // lock, so recording ours as seen skips nothing.
            let seen = m.cpus[self.index].tlb_seen.load(Ordering::Relaxed);
            if n - seen > smp::TLB_RING as u64 {
                self.tlb_flush();
            } else {
                for k in seen..n {
                    match ring[k as usize % smp::TLB_RING] {
                        smp::TLB_ALL => self.tlb_flush(),
                        p => self.tlb_flush_page(p),
                    }
                }
            }
            ring[n as usize % smp::TLB_RING] = page;
            m.tlb_gen.store(n + 1, Ordering::SeqCst);
            n + 1
        };
        m.cpus[self.index].tlb_seen.store(gen, Ordering::SeqCst);
        self.tlb_sync_gen = gen;
        self.machine.poke_others(self.index);
    }

    /// DSB after a broadcast TLBI: wait until every other running CPU applied it.
    pub(crate) fn tlb_sync(&mut self) {
        let gen = std::mem::take(&mut self.tlb_sync_gen);
        if gen == 0 {
            return;
        }
        let m = self.machine.clone();
        let mut spins = 0u32;
        loop {
            // Apply other CPUs' shootdowns too, so two waiting CPUs can't deadlock.
            self.apply_tlb_shootdowns();
            let done = (0..m.ncpus).all(|i| {
                let c = &m.cpus[i];
                i == self.index
                    || c.run_state.load(Ordering::SeqCst) != smp::RUN_RUNNING
                    || c.power.load(Ordering::SeqCst) != smp::POWER_ON
                    || c.tlb_seen.load(Ordering::SeqCst) >= gen
            });
            if done || m.stop.load(Ordering::SeqCst) != smp::STOP_NONE {
                return;
            }
            // The CPUs we wait for may not have a host core right now.
            if spins == 0 {
                m.stats[1].fetch_add(1, Ordering::Relaxed);
            }
            spins += 1;
            if spins < if m.oversubscribed() { 64 } else { 100_000 } {
                std::hint::spin_loop();
            } else {
                self.host.yield_now();
            }
        }
    }

    /// Execute exactly one instruction, taking any exception it raises.
    #[inline(always)]
    pub fn step(&mut self) -> Res<()> {
        let pc = self.pc;
        self.next_pc = pc.wrapping_add(4);
        let result = match self.fetch_decoded(pc) {
            Ok(e) => self.exec_decoded(e),
            Err(e) => Err(e),
        };
        self.insn_count = self.insn_count.wrapping_add(1);
        match result {
            Ok(()) => {
                self.pc = self.next_pc;
                Ok(())
            },
            Err(Event::Sync {
                ec,
                iss,
                far,
                ret_next,
            }) => {
                let ret = if ret_next { self.next_pc } else { pc };
                if ec == ec::UNKNOWN {
                    if let Ok(insn) = self.fetch(pc) {
                        self.last_undefined = Some((pc, insn));
                        if self.trap_undefined {
                            return Err(Event::Stop(StopReason::Undefined { pc, insn }));
                        }
                    }
                }
                self.sync_exception(ec, iss, far, ret).map_err(Event::Stop)
            },
            Err(Event::Wfi) => {
                self.pc = self.next_pc;
                Err(Event::Wfi)
            },
            Err(e) => Err(e),
        }
    }

    #[inline(always)]
    fn fetch(&mut self, pc: u64) -> Res<u32> {
        let f = &self.fast[self.el as usize];
        if pc >> 12 == f.code_page {
            let a = f.code_base + (pc & 0xffc) as usize;
            return Ok(u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap()));
        }
        self.fetch_slow(pc)
    }

    /// The pre-decoded entry of the instruction at `pc` (see `predecode`).
    #[inline(always)]
    fn fetch_decoded(&mut self, pc: u64) -> Res<u64> {
        let f = &self.fast[self.el as usize];
        if pc >> 12 == f.code_page
            && f.code_base == self.dec_base
            && self.dec_gen == self.jit.pre.gen
        {
            let i = ((pc & 0xfff) >> 2) as usize;
            let e = self.jit.pre.get(self.dec_slot, i);
            if e != 0 {
                return Ok(e);
            }
            return Ok(self.decode_at(i));
        }
        self.fetch_decoded_slow(pc)
    }

    #[inline(never)]
    fn fetch_decoded_slow(&mut self, pc: u64) -> Res<u64> {
        let insn = self.fetch(pc)?;
        let f = &self.fast[self.el as usize];
        if pc >> 12 != f.code_page {
            // Not a page the fetch path keeps (it ends past RAM): run it undecoded.
            return Ok(crate::predecode::entry(insn));
        }
        let base = f.code_base;
        let page = base >> 12;
        let mut slot = self.jit.pre.slot(page);
        if slot == crate::predecode::NONE {
            slot = self.jit.pre.alloc(page);
            self.jit.code_page[page] |= 2;
            // Stores to the page, here or by a device, must drop what was decoded from it.
            self.machine.code_pages[page].store(1, Ordering::Relaxed);
            for f in self.fast.iter_mut() {
                f.forget_write_page(base);
            }
        }
        self.dec_base = base;
        self.dec_slot = slot;
        self.dec_gen = self.jit.pre.gen;
        let i = ((pc & 0xfff) >> 2) as usize;
        let e = self.jit.pre.get(slot, i);
        Ok(if e != 0 { e } else { self.decode_at(i) })
    }

    /// Decode instruction `i` of the current pre-decoded page.
    #[inline(never)]
    fn decode_at(&mut self, i: usize) -> u64 {
        let a = self.dec_base + i * 4;
        let insn = u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap());
        let e = crate::predecode::entry(insn);
        self.jit.pre.set(self.dec_slot, i, e);
        e
    }

    pub(crate) fn fetch_slow_pub(&mut self, pc: u64) -> Res<u32> {
        self.fetch_slow(pc)
    }

    #[inline(never)]
    fn fetch_slow(&mut self, pc: u64) -> Res<u32> {
        if pc & 3 != 0 {
            return Err(Event::Sync {
                ec: ec::PC_ALIGN,
                iss: 0,
                far: Some(pc),
                ret_next: false,
            });
        }
        let f = &mut self.fast[self.el as usize];
        let i = (pc >> 12) as usize & (CODE_ENTRIES - 1);
        if f.code_tag[i] == pc >> 12 {
            f.code_page = pc >> 12;
            f.code_base = f.code_bases[i];
            let a = f.code_base + (pc & 0xffc) as usize;
            return Ok(u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap()));
        }
        let pa = self.translate(pc, crate::mmu::Access::Exec, false)?;
        let a = pa as usize;
        if a.checked_add(4).is_some_and(|end| end <= self.mem.len()) {
            let page = (pa & !0xfff) as usize;
            if page
                .checked_add(0x1000)
                .is_some_and(|end| end <= self.mem.len())
            {
                let f = &mut self.fast[self.el as usize];
                f.code_page = pc >> 12;
                f.code_base = page;
                f.code_tag[i] = pc >> 12;
                f.code_bases[i] = page;
            }
            Ok(u32::from_le_bytes(self.mem[a..a + 4].try_into().unwrap()))
        } else {
            // Executing from MMIO is not supported: synchronous external abort.
            Err(Event::Sync {
                ec: if self.el == 0 { ec::IABT_LOW } else { ec::IABT_CUR },
                iss: 0x10,
                far: Some(pc),
                ret_next: false,
            })
        }
    }

    // --------------------------------------------------------------- memory

    /// Physical read. `None` = nothing there.
    pub fn phys_read(&mut self, pa: u64, size: u32) -> Option<u64> {
        let a = pa as usize;
        let len = self.mem.len();
        if (pa as u128) + (size as u128) <= len as u128 {
            let s = &self.mem[a..a + size as usize];
            return Some(match size {
                1 => s[0] as u64,
                2 => u16::from_le_bytes([s[0], s[1]]) as u64,
                4 => u32::from_le_bytes(s.try_into().unwrap()) as u64,
                8 => u64::from_le_bytes(s.try_into().unwrap()),
                _ => unreachable!(),
            });
        }
        self.mmio_read(pa, size)
    }

    pub fn phys_write(&mut self, pa: u64, size: u32, value: u64) -> bool {
        let a = pa as usize;
        let len = self.mem.len();
        if (pa as u128) + (size as u128) <= len as u128 {
            let bytes = value.to_le_bytes();
            self.mem[a..a + size as usize].copy_from_slice(&bytes[..size as usize]);
            return true;
        }
        self.mmio_write(pa, size, value)
    }

    fn mmio_read(&mut self, pa: u64, size: u32) -> Option<u64> {
        if (board::GICD_BASE..board::GICD_BASE + board::GICD_SIZE).contains(&pa) {
            return Some(self.machine.gic.lock().dist_read(
                self.index,
                pa - board::GICD_BASE,
                size,
            ));
        }
        if (board::GICC_BASE..board::GICC_BASE + board::GICC_SIZE).contains(&pa) {
            let (v, mask) = self
                .machine
                .gic
                .lock()
                .cpu_read(self.index, pa - board::GICC_BASE);
            self.kick(mask);
            self.set_irq_dirty();
            return Some(v);
        }
        if (board::RTC_BASE..board::RTC_BASE + board::RTC_SIZE).contains(&pa) {
            let now = self.host.wall_clock_secs();
            return Some(self.machine.rtc.lock().read(pa - board::RTC_BASE, now));
        }
        self.host.mmio_read(pa, size)
    }

    fn mmio_write(&mut self, pa: u64, size: u32, value: u64) -> bool {
        if (board::GICD_BASE..board::GICD_BASE + board::GICD_SIZE).contains(&pa) {
            let mask =
                self.machine
                    .gic
                    .lock()
                    .dist_write(self.index, pa - board::GICD_BASE, size, value);
            self.kick(mask);
            self.set_irq_dirty();
            return true;
        }
        if (board::GICC_BASE..board::GICC_BASE + board::GICC_SIZE).contains(&pa) {
            let mask = self
                .machine
                .gic
                .lock()
                .cpu_write(self.index, pa - board::GICC_BASE, value);
            self.kick(mask);
            self.set_irq_dirty();
            return true;
        }
        if (board::RTC_BASE..board::RTC_BASE + board::RTC_SIZE).contains(&pa) {
            let now = self.host.wall_clock_secs();
            let line = self
                .machine
                .rtc
                .lock()
                .write(pa - board::RTC_BASE, value, now);
            self.set_irq_line(board::SPI_RTC, line);
            return true;
        }
        let ok = self.host.mmio_write(pa, size, value);
        // Device writes may raise interrupts through the host.
        self.set_irq_dirty();
        ok
    }

    #[inline(always)]
    fn ram_load(&self, a: usize, size: u32) -> u64 {
        let m = &self.mem;
        match size {
            1 => m[a] as u64,
            2 => u16::from_le_bytes([m[a], m[a + 1]]) as u64,
            4 => u32::from_le_bytes(m[a..a + 4].try_into().unwrap()) as u64,
            _ => u64::from_le_bytes(m[a..a + 8].try_into().unwrap()),
        }
    }

    #[inline(always)]
    fn ram_store(&mut self, a: usize, size: u32, value: u64) {
        let m = &mut self.mem;
        match size {
            1 => m[a] = value as u8,
            2 => m[a..a + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            4 => m[a..a + 4].copy_from_slice(&(value as u32).to_le_bytes()),
            _ => m[a..a + 8].copy_from_slice(&value.to_le_bytes()),
        }
    }

    /// WFE with several CPUs: wait until something may have changed what the
    /// guest waits for (the location it load-exclusived was written, an
    /// interrupt or other request, SEV), polling briefly. Returning early is
    /// always allowed; the guest re-checks and waits again.
    pub(crate) fn wfe(&mut self) {
        if std::mem::take(&mut self.event_register) {
            return;
        }
        let m = self.machine.clone();
        let sev = m.sev.load(Ordering::Relaxed);
        let watch = self
            .excl()
            .filter(|&pa| pa + self.excl_size as u64 <= self.mem.len() as u64);
        let oversubscribed = m.oversubscribed();
        let spins = if oversubscribed { 32 } else { 4000 };
        for _ in 0..spins {
            if self.attn().load(Ordering::Relaxed) != 0 || m.sev.load(Ordering::Relaxed) != sev {
                return;
            }
            if let Some(pa) = watch {
                if self.ram_load(pa as usize, self.excl_size) != self.excl_value {
                    return;
                }
            }
            std::hint::spin_loop();
        }
        if oversubscribed {
            self.host.yield_now();
        }
    }

    /// SEV: set the event register on every CPU.
    pub(crate) fn sev(&mut self) {
        self.event_register = true;
        self.machine.sev.fetch_add(1, Ordering::SeqCst);
    }

    /// Atomic compare-and-swap of guest RAM at an aligned physical address
    /// (STXR with several CPUs). None if `pa` is not RAM or not aligned.
    pub(crate) fn ram_cmpxchg(&self, pa: u64, size: u32, expected: u64, new: u64) -> Option<bool> {
        use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU64};
        if !pa.is_multiple_of(size as u64) || pa + size as u64 > self.mem.len() as u64 {
            return None;
        }
        // In bounds and naturally aligned (RAM is page-aligned on the host).
        let p = unsafe { self.mem.as_mut_ptr().add(pa as usize) };
        let (o, f) = (Ordering::SeqCst, Ordering::SeqCst);
        Some(unsafe {
            match size {
                1 => AtomicU8::from_ptr(p)
                    .compare_exchange(expected as u8, new as u8, o, f)
                    .is_ok(),
                2 => AtomicU16::from_ptr(p as *mut u16)
                    .compare_exchange(expected as u16, new as u16, o, f)
                    .is_ok(),
                4 => AtomicU32::from_ptr(p as *mut u32)
                    .compare_exchange(expected as u32, new as u32, o, f)
                    .is_ok(),
                _ => AtomicU64::from_ptr(p as *mut u64)
                    .compare_exchange(expected, new, o, f)
                    .is_ok(),
            }
        })
    }

    /// Virtual read of `size` bytes with the current (or EL0, if `unpriv`) permissions.
    #[inline(always)]
    pub fn read(&mut self, va: u64, size: u32, unpriv: bool) -> Res<u64> {
        let off = va & 0xfff;
        if !unpriv && off + size as u64 <= 0x1000 {
            let f = &self.fast[self.el as usize];
            let i = (va >> 12) as usize & (FAST_ENTRIES - 1);
            if f.read_tag[i] == va >> 12 {
                return Ok(self.ram_load(f.read_base[i] + off as usize, size));
            }
        }
        self.read_slow(va, size, unpriv)
    }

    #[inline(never)]
    fn read_slow(&mut self, va: u64, size: u32, unpriv: bool) -> Res<u64> {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, crate::mmu::Access::Read, unpriv)?;
            let page = (pa & !0xfff) as usize;
            if !unpriv
                && page
                    .checked_add(0x1000)
                    .is_some_and(|end| end <= self.mem.len())
            {
                let f = &mut self.fast[self.el as usize];
                let i = (va >> 12) as usize & (FAST_ENTRIES - 1);
                f.read_tag[i] = va >> 12;
                f.read_base[i] = page;
            }
            return self
                .phys_read(pa, size)
                .ok_or_else(|| self.external_abort(va, false));
        }
        // Crosses a page boundary: go byte by byte.
        let mut v = 0u64;
        for i in 0..size as u64 {
            let b = self.read(va.wrapping_add(i), 1, unpriv)?;
            v |= b << (8 * i);
        }
        Ok(v)
    }

    #[inline(always)]
    pub fn write(&mut self, va: u64, size: u32, value: u64, unpriv: bool) -> Res<()> {
        let off = va & 0xfff;
        if !unpriv && off + size as u64 <= 0x1000 && self.excl_armed == 0 {
            let f = &self.fast[self.el as usize];
            let i = (va >> 12) as usize & (FAST_ENTRIES - 1);
            if f.write_tag[i] == va >> 12 {
                let a = f.write_base[i] + off as usize;
                self.ram_store(a, size, value);
                return Ok(());
            }
        }
        self.write_slow(va, size, value, unpriv)
    }

    #[inline(never)]
    fn write_slow(&mut self, va: u64, size: u32, value: u64, unpriv: bool) -> Res<()> {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, crate::mmu::Access::Write, unpriv)?;
            let page = (pa & !0xfff) as usize;
            let has_code = self.jit.code_page.get(page >> 12).is_some_and(|&b| b != 0);
            if has_code {
                self.jit.invalidate_page(page >> 12);
            } else if !unpriv
                && page
                    .checked_add(0x1000)
                    .is_some_and(|end| end <= self.mem.len())
            {
                let f = &mut self.fast[self.el as usize];
                let i = (va >> 12) as usize & (FAST_ENTRIES - 1);
                f.write_tag[i] = va >> 12;
                f.write_base[i] = page;
            }
            if let Some(e) = self.excl() {
                if e & !0x3f == pa & !0x3f {
                    self.set_excl(None);
                }
            }
            return if self.phys_write(pa, size, value) {
                Ok(())
            } else {
                Err(self.external_abort(va, true))
            };
        }
        // Translate every byte first so a fault leaves memory untouched.
        for i in 0..size as u64 {
            self.translate(va.wrapping_add(i), crate::mmu::Access::Write, unpriv)?;
        }
        for i in 0..size as u64 {
            self.write(va.wrapping_add(i), 1, (value >> (8 * i)) & 0xff, unpriv)?;
        }
        Ok(())
    }

    fn external_abort(&self, va: u64, write: bool) -> Event {
        Event::Sync {
            ec: if self.el == 0 { ec::DABT_LOW } else { ec::DABT_CUR },
            iss: 0x10 | ((write as u32) << 6),
            far: Some(va),
            ret_next: false,
        }
    }

    // ------------------------------------------------------------------ PSCI

    /// Handle an HVC/SMC as a PSCI firmware call, like QEMU's virt board does.
    pub(crate) fn psci_call(&mut self) -> Res<()> {
        const NOT_SUPPORTED: u64 = (-1i64) as u64;
        const INVALID_PARAMETERS: u64 = (-2i64) as u64;
        const ALREADY_ON: u64 = (-4i64) as u64;
        const ON_PENDING: u64 = (-5i64) as u64;
        let m = self.machine.clone();
        let fid = self.x[0] as u32;
        // MPIDR -> CPU number (Aff0; higher affinity levels must be zero)
        let target = |mpidr: u64| -> Option<usize> {
            let aff = mpidr & 0xff_00ff_ffff;
            (aff < m.ncpus as u64).then_some(aff as usize)
        };
        let ret = match fid {
            0x8400_0000 => 0x0001_0000, // PSCI_VERSION: 1.0
            0x8400_0002 => {
                // CPU_OFF
                if m.ncpus == 1 {
                    return Err(Event::Stop(StopReason::PowerOff));
                }
                m.cpus[self.index]
                    .power
                    .store(smp::POWER_OFF, Ordering::SeqCst);
                self.set_irq_dirty();
                return Err(Event::Wfi);
            },
            0x8400_0003 | 0xC400_0003 => {
                // CPU_ON(target, entry, context)
                match target(self.x[1]) {
                    None => INVALID_PARAMETERS,
                    Some(t) => {
                        let c = &m.cpus[t];
                        c.entry.store(self.x[2], Ordering::SeqCst);
                        c.context.store(self.x[3], Ordering::SeqCst);
                        match c.power.compare_exchange(
                            smp::POWER_OFF,
                            smp::POWER_ON_PENDING,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ) {
                            Ok(_) => {
                                m.kick(t);
                                0
                            },
                            Err(smp::POWER_ON_PENDING) => ON_PENDING,
                            Err(_) => ALREADY_ON,
                        }
                    },
                }
            },
            0x8400_0004 | 0xC400_0004 => {
                // AFFINITY_INFO(target, level)
                match target(self.x[1]) {
                    Some(t) if self.x[2] == 0 => m.cpus[t].power.load(Ordering::SeqCst) as u64,
                    _ => INVALID_PARAMETERS,
                }
            },
            0x8400_0006 => 2, // MIGRATE_INFO_TYPE: not required
            0x8400_0008 => {
                m.request_stop(smp::STOP_POWER_OFF);
                return Err(Event::Stop(StopReason::PowerOff));
            },
            0x8400_0009 => {
                m.request_stop(smp::STOP_RESET);
                return Err(Event::Stop(StopReason::Reset));
            },
            0x8400_000A => match self.x[1] as u32 {
                0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xC400_0003 | 0x8400_0004
                | 0xC400_0004 | 0x8400_0006 | 0x8400_0008 | 0x8400_0009 | 0x8400_000A => 0,
                _ => NOT_SUPPORTED,
            },
            _ => NOT_SUPPORTED,
        };
        self.x[0] = ret;
        Ok(())
    }
}
