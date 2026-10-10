//! AArch64 CPU state, the run loop, exceptions, interrupts and the physical bus.
//!
//! The instruction semantics live in `a64.rs`, address translation in `mmu.rs`.

use crate::board::{self, Host};
use crate::gic::Gic;
use crate::mmu::Tlb;
use crate::rtc::Rtc;

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
    Idle { wake_ns: Option<u64> },
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

    /// Guest physical RAM, starting at physical address 0 (= v86's mem8).
    pub mem: Vec<u8>,
    pub gic: Gic,
    pub rtc: Rtc,
    pub host: H,
    pub(crate) tlb: Tlb,

    /// Exclusive monitor: physical address of the last load-exclusive.
    pub(crate) excl: Option<u64>,
    pub(crate) next_pc: u64,
    /// Instructions retired since reset.
    pub insn_count: u64,
    /// Re-evaluate the IRQ line before the next instruction.
    pub(crate) irq_dirty: bool,
    /// Generic timer: counter value at `timer_ns_base` (host ns).
    pub(crate) timer_cnt_base: u64,
    pub(crate) timer_ns_base: u64,
    /// Stop instead of raising an exception on undefined instructions.
    pub trap_undefined: bool,
    /// Last undefined instruction seen (for diagnostics), as (pc, insn).
    pub last_undefined: Option<(u64, u32)>,
}

const DAIF_ALL: u32 = 0xf << 6;
const DAIF_I: u32 = 1 << 7;

impl<H: Host> Cpu<H> {
    pub fn new(memory_size: usize, host: H) -> Cpu<H> {
        assert!(memory_size as u64 <= board::RAM_MAX, "RAM would overlap the MMIO window");
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
            mem: vec![0; memory_size],
            gic: Gic::new(),
            rtc: Rtc::default(),
            host,
            tlb: Tlb::new(),
            excl: None,
            next_pc: 0,
            insn_count: 0,
            irq_dirty: true,
            timer_cnt_base: 0,
            timer_ns_base: 0,
            trap_undefined: false,
            last_undefined: None,
        };
        cpu.reset();
        cpu
    }

    /// Architectural reset into EL1h with the MMU off.
    pub fn reset(&mut self) {
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
        self.gic = Gic::new();
        self.tlb.flush();
        self.excl = None;
        self.irq_dirty = true;
        self.timer_ns_base = self.host.now_ns();
        self.timer_cnt_base = 0;
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
        ((self.n as u64) << 31) | ((self.z as u64) << 30) | ((self.c as u64) << 29) | ((self.v as u64) << 28)
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
            }
            0b0100 => {
                self.el = 1;
                self.spsel = false;
            }
            0b0101 => {
                self.el = 1;
                self.spsel = true;
            }
            _ => {
                // Illegal return (EL2/EL3 or AArch32): stay at EL1h.
                self.el = 1;
                self.spsel = true;
            }
        }
        self.irq_dirty = true;
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
        self.excl = None;
        self.pc = self.sys.vbar.wrapping_add(offset + kind * 0x80);
        self.irq_dirty = true;
    }

    fn sync_exception(&mut self, ec: u32, iss: u32, far: Option<u64>, ret: u64) -> Result<(), StopReason> {
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
        self.excl = None;
    }

    // --------------------------------------------------------------- timers

    /// Generic timer counter value (CNTPCT/CNTVCT; CNTVOFF is zero).
    pub fn counter(&mut self) -> u64 {
        let now = self.host.now_ns();
        let ns = now.wrapping_sub(self.timer_ns_base);
        self.timer_cnt_base
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
        self.gic.set_line(board::PPI_VIRT_TIMER, v);
        self.gic.set_line(board::PPI_PHYS_TIMER, p);
        self.irq_dirty = true;
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
        self.gic.set_line(intid, level);
        self.irq_dirty = true;
    }

    fn sync_host_irqs(&mut self) {
        let gic = &mut self.gic;
        self.host.sync_irqs(&mut |intid, level| gic.set_line(intid, level));
    }

    fn irq_masked(&self) -> bool {
        self.daif & DAIF_I != 0
    }

    // --------------------------------------------------------------- run loop

    /// Execute up to `budget` instructions.
    pub fn run(&mut self, budget: u64) -> Exit {
        self.update_timers();
        let mut executed = 0;
        while executed < budget {
            if self.irq_dirty {
                self.irq_dirty = false;
                self.sync_host_irqs();
                if !self.irq_masked() && self.gic.irq_asserted() {
                    let ret = self.pc;
                    self.take_exception(1, ret);
                }
            }
            match self.step() {
                Ok(()) => {}
                Err(Event::Wfi) => {
                    self.update_timers();
                    self.sync_host_irqs();
                    if !self.gic.irq_asserted() {
                        return Exit::Idle {
                            wake_ns: self.next_timer_deadline_ns(),
                        };
                    }
                }
                Err(Event::Stop(r)) => return Exit::Stop(r),
                Err(Event::Sync { .. }) => unreachable!("handled in step"),
            }
            executed += 1;
            // Poll the timer every so often; it is cheap but not free.
            if executed & 0x3ff == 0 {
                self.update_timers();
            }
        }
        Exit::Budget
    }

    /// Execute exactly one instruction, taking any exception it raises.
    pub fn step(&mut self) -> Res<()> {
        let pc = self.pc;
        self.next_pc = pc.wrapping_add(4);
        let result = match self.fetch(pc) {
            Ok(insn) => self.execute(insn),
            Err(e) => Err(e),
        };
        self.insn_count = self.insn_count.wrapping_add(1);
        match result {
            Ok(()) => {
                self.pc = self.next_pc;
                Ok(())
            }
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
            }
            Err(Event::Wfi) => {
                self.pc = self.next_pc;
                Err(Event::Wfi)
            }
            Err(e) => Err(e),
        }
    }

    fn fetch(&mut self, pc: u64) -> Res<u32> {
        if pc & 3 != 0 {
            return Err(Event::Sync {
                ec: ec::PC_ALIGN,
                iss: 0,
                far: Some(pc),
                ret_next: false,
            });
        }
        let pa = self.translate(pc, crate::mmu::Access::Exec, false)?;
        let a = pa as usize;
        if a + 4 <= self.mem.len() {
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
            return Some(self.gic.dist_read(pa - board::GICD_BASE, size));
        }
        if (board::GICC_BASE..board::GICC_BASE + board::GICC_SIZE).contains(&pa) {
            let v = self.gic.cpu_read(pa - board::GICC_BASE);
            self.irq_dirty = true;
            return Some(v);
        }
        if (board::RTC_BASE..board::RTC_BASE + board::RTC_SIZE).contains(&pa) {
            let now = self.host.wall_clock_secs();
            return Some(self.rtc.read(pa - board::RTC_BASE, now));
        }
        self.host.mmio_read(pa, size)
    }

    fn mmio_write(&mut self, pa: u64, size: u32, value: u64) -> bool {
        if (board::GICD_BASE..board::GICD_BASE + board::GICD_SIZE).contains(&pa) {
            self.gic.dist_write(pa - board::GICD_BASE, size, value);
            self.irq_dirty = true;
            return true;
        }
        if (board::GICC_BASE..board::GICC_BASE + board::GICC_SIZE).contains(&pa) {
            self.gic.cpu_write(pa - board::GICC_BASE, value);
            self.irq_dirty = true;
            return true;
        }
        if (board::RTC_BASE..board::RTC_BASE + board::RTC_SIZE).contains(&pa) {
            let now = self.host.wall_clock_secs();
            let line = self.rtc.write(pa - board::RTC_BASE, value, now);
            self.set_irq_line(board::SPI_RTC, line);
            return true;
        }
        let ok = self.host.mmio_write(pa, size, value);
        // Device writes may raise interrupts through the host.
        self.irq_dirty = true;
        ok
    }

    /// Virtual read of `size` bytes with the current (or EL0, if `unpriv`) permissions.
    pub fn read(&mut self, va: u64, size: u32, unpriv: bool) -> Res<u64> {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, crate::mmu::Access::Read, unpriv)?;
            return self.phys_read(pa, size).ok_or_else(|| self.external_abort(va, false));
        }
        // Crosses a page boundary: go byte by byte.
        let mut v = 0u64;
        for i in 0..size as u64 {
            let b = self.read(va.wrapping_add(i), 1, unpriv)?;
            v |= b << (8 * i);
        }
        Ok(v)
    }

    pub fn write(&mut self, va: u64, size: u32, value: u64, unpriv: bool) -> Res<()> {
        if (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, crate::mmu::Access::Write, unpriv)?;
            if let Some(e) = self.excl {
                if e & !0x3f == pa & !0x3f {
                    self.excl = None;
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
        let fid = self.x[0] as u32;
        let ret = match fid {
            0x8400_0000 => 0x0001_0000, // PSCI_VERSION: 1.0
            0x8400_0002 => return Err(Event::Stop(StopReason::PowerOff)), // CPU_OFF
            0x8400_0004 | 0xC400_0004 => 0, // AFFINITY_INFO: on
            0x8400_0006 => 2,           // MIGRATE_INFO_TYPE: not required
            0x8400_0008 => return Err(Event::Stop(StopReason::PowerOff)),
            0x8400_0009 => return Err(Event::Stop(StopReason::Reset)),
            0x8400_000A => match self.x[1] as u32 {
                0x8400_0000 | 0x8400_0002 | 0x8400_0004 | 0xC400_0004 | 0x8400_0006 | 0x8400_0008
                | 0x8400_0009 | 0x8400_000A => 0,
                _ => NOT_SUPPORTED,
            },
            _ => NOT_SUPPORTED,
        };
        self.x[0] = ret;
        Ok(())
    }
}
