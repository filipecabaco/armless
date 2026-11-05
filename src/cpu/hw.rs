//! Running a CPU in hardware (the `hypervisor` feature): the guest's instructions run on the
//! host's cores, under Hypervisor.framework (`hv`, macOS on Apple silicon) or KVM (`kvm`, Linux
//! arm64), and armless sees only what the guest asks of the machine. A load or store to a device
//! is served by `phys_read` / `phys_write` (the GIC's registers included), HVC and SMC are PSCI
//! calls (`psci_call`), and the virtual timer's interrupt goes to armless's GIC as PPI 27. The
//! devices, the GIC, snapshots and the runner are the machine the interpreter and the JIT run, and
//! the architectural state moves between them (`hw_load`, `hw_store`): it is loaded into the vCPU
//! each time `run` starts and read back when it returns, so a machine can stop in one and go on in
//! the other.
//!
//! What the hardware can't describe (a device access without a syndrome: a pair, writeback,
//! SIMD&FP) the interpreter runs, one instruction (`hw_interpret_one`).

use super::*;
use crate::sysreg::{self as sr};

/// A register of a vCPU, as the backends name them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Reg {
    X(u32),
    Pc,
    /// PSTATE as SPSR lays it out
    Cpsr,
    Fpcr,
    Fpsr,
    SpEl0,
    SpEl1,
    /// A system register by its `sr` key
    Sys(u32),
}

/// What `hw_load` and `hw_store` need of a backend's vCPU.
pub(crate) trait HwVcpu {
    fn get(&self, r: Reg) -> u64;
    fn set(&self, r: Reg, v: u64);
    fn vreg(&self, i: usize) -> u128;
    fn set_vreg(&self, i: usize, v: u128);
}

/// How a CPU runs: interpreted or compiled (`Off`), or in hardware once its thread makes the
/// vCPU (`Wanted`, then the backend's vCPU).
#[derive(Default)]
pub(crate) enum Backend {
    #[default]
    Off,
    Wanted,
    #[cfg(armless_hvf)]
    Hvf(super::hv::Vcpu),
    #[cfg(armless_kvm)]
    Kvm(super::kvm::Vcpu),
}

/// A system register's `sr` key and its field in `SysRegs`.
type SysField = (u32, fn(&mut SysRegs) -> &mut u64);

/// The system registers that move with a vCPU's state.
pub(crate) const SYS: &[SysField] = &[
    (sr::SCTLR_EL1, |s| &mut s.sctlr),
    (sr::CPACR_EL1, |s| &mut s.cpacr),
    (sr::TTBR0_EL1, |s| &mut s.ttbr0),
    (sr::TTBR1_EL1, |s| &mut s.ttbr1),
    (sr::TCR_EL1, |s| &mut s.tcr),
    (sr::ESR_EL1, |s| &mut s.esr),
    (sr::FAR_EL1, |s| &mut s.far),
    (sr::PAR_EL1, |s| &mut s.par),
    (sr::MAIR_EL1, |s| &mut s.mair),
    (sr::sr(3, 0, 10, 3, 0), |s| &mut s.amair),
    (sr::VBAR_EL1, |s| &mut s.vbar),
    (sr::CONTEXTIDR_EL1, |s| &mut s.contextidr),
    (sr::TPIDR_EL1, |s| &mut s.tpidr_el1),
    (sr::TPIDR_EL0, |s| &mut s.tpidr_el0),
    (sr::TPIDRRO_EL0, |s| &mut s.tpidrro_el0),
    (sr::CNTKCTL_EL1, |s| &mut s.cntkctl),
    (sr::CNTV_CTL_EL0, |s| &mut s.cntv_ctl),
    (sr::CNTV_CVAL_EL0, |s| &mut s.cntv_cval),
    (sr::sr(2, 0, 0, 2, 2), |s| &mut s.mdscr),
    (sr::CSSELR_EL1, |s| &mut s.csselr),
    (sr::sr(3, 0, 5, 1, 0), |s| &mut s.afsr0),
    (sr::sr(3, 0, 5, 1, 1), |s| &mut s.afsr1),
];

// CNTV_CTL_EL0 bits (Hypervisor.framework leaves the virtual timer's interrupt to armless)
#[cfg(armless_hvf)]
pub(crate) const CTL_ENABLE: u64 = 1;
#[cfg(armless_hvf)]
pub(crate) const CTL_IMASK: u64 = 2;
#[cfg(armless_hvf)]
pub(crate) const CTL_ISTATUS: u64 = 4;

/// The frequency of the host's counter, which a guest in hardware reads as CNTFRQ_EL0.
pub(crate) fn host_counter_hz() -> u64 {
    let f: u64;
    // SAFETY: reading CNTFRQ_EL0 at EL0 is allowed
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f)
    };
    f.max(1)
}

/// The host's counter, which a guest in hardware reads (less the offset `counter_offset` sets).
/// On macOS that is `mach_absolute_time`, which Hypervisor.framework's offset is against: the
/// CNTVCT_EL0 a process reads there is another count (a year apart, here).
#[cfg(armless_hvf)]
pub(crate) fn host_counter() -> u64 {
    super::hv::mach_time()
}

/// The host's counter, which a guest in hardware reads (less the offset `counter_offset` sets):
/// on Linux, CNTVCT_EL0, the count KVM's counter offset is against.
#[cfg(armless_kvm)]
pub(crate) fn host_counter() -> u64 {
    let c: u64;
    // SAFETY: reading CNTVCT_EL0 at EL0 is allowed
    #[allow(unsafe_code)]
    unsafe {
        core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c)
    };
    c
}

/// A host time (`Host::now_ns`) and the host counter at it, taken once: every CPU maps one to the
/// other the same way, so their counters agree to the tick.
static REFERENCE: std::sync::OnceLock<(u64, u64)> = std::sync::OnceLock::new();

/// Whether guests can run in hardware here, and with which GIC: Ok(false) for armless's own
/// GICv2, Ok(true) for the hypervisor's in-kernel GICv3 (KVM, where it doesn't deliver the
/// interrupts armless's GIC raises). Ask before booting: the device tree describes the GIC
/// (`Machine::gic_v3`).
pub fn hypervisor_probe() -> Result<bool, String> {
    #[cfg(armless_kvm)]
    return super::kvm::delivers_irq_line().map(|own_gic_works| !own_gic_works);
    #[cfg(armless_hvf)]
    return Ok(false);
}

/// The feature ID registers a guest in hardware is shown: armless's own (`read_sysreg`), so it
/// uses what the interpreter and the JIT implement and nothing else (pointer authentication,
/// BTI, MTE, SVE, SME, FP16...), and boots and behaves as it does under them, with snapshots
/// that go on in either. The cache's registers (CTR_EL0, CLIDR_EL1, DCZID_EL0) stay the host's:
/// they say what maintenance the hardware needs. With the hypervisor's GICv3 the guest is told
/// of its system register interface (ID_AA64PFR0_EL1.GIC), which the GICv3 driver needs.
pub(crate) const MASKED_ID_REGS: &[u32] = &[
    sr::ID_AA64PFR0_EL1,
    sr::sr(3, 0, 0, 4, 1), // ID_AA64PFR1_EL1
    sr::sr(3, 0, 0, 4, 4), // ID_AA64ZFR0_EL1
    sr::sr(3, 0, 0, 4, 5), // ID_AA64SMFR0_EL1
    sr::ID_AA64DFR0_EL1,
    sr::ID_AA64ISAR0_EL1,
    sr::sr(3, 0, 0, 6, 1), // ID_AA64ISAR1_EL1
    sr::sr(3, 0, 0, 6, 2), // ID_AA64ISAR2_EL1
    sr::ID_AA64MMFR0_EL1,
    sr::ID_AA64MMFR1_EL1,
    sr::ID_AA64MMFR2_EL1,
];

impl<H: Host> Cpu<H> {
    /// Run this CPU in hardware from now on, if the process can; else it stays with the
    /// interpreter and the JIT. The vCPU is made when the CPU first runs, on its thread.
    pub fn enable_hypervisor(&mut self) -> Result<(), String> {
        #[cfg(armless_hvf)]
        super::hv::available(&self.mem)?;
        #[cfg(armless_kvm)]
        super::kvm::available(
            &self.mem,
            self.machine.ncpus,
            self.machine.gic_v3.load(Ordering::Relaxed),
        )?;
        #[cfg(armless_kvm)]
        if self.machine.gic_v3.load(Ordering::Relaxed) {
            let _ = self.machine.irq_sink.set(Box::new(super::kvm::spi_line));
        }
        // A machine that starts here has the host's counter, whose frequency it can't change:
        // the guest reads it at boot, and its snapshots carry it
        if !self.machine.started.load(Ordering::Relaxed) {
            self.machine
                .timer_freq
                .store(host_counter_hz(), Ordering::SeqCst);
        }
        self.hv = Backend::Wanted;
        Ok(())
    }

    /// Why a restored guest can't go on in hardware, if it can't: it keeps the counter frequency
    /// it booted with, which hardware can't change, and the GIC it booted with (armless's).
    fn hw_cannot_resume(&self) -> Option<String> {
        let m = &self.machine;
        if !m.restored.load(Ordering::Relaxed) {
            return None;
        }
        let freq = m.timer_freq.load(Ordering::Relaxed);
        let host = host_counter_hz();
        if freq != host {
            return Some(format!(
                "the snapshot's counter runs at {freq} Hz, this host's at {host} Hz"
            ));
        }
        if m.gic_v3.load(Ordering::Relaxed) {
            return Some("the snapshot's guest has armless's GICv2, which this hypervisor can't deliver".into());
        }
        None
    }

    /// The offset of the guest's counter from the host's: the guest's counter goes on from
    /// armless's (`counter`), at the same rate, whichever runs it.
    pub(super) fn counter_offset(&mut self) -> u64 {
        let &(ref_ns, ref_ctr) =
            REFERENCE.get_or_init(|| (self.host.now_ns(), host_counter()));
        let m = &self.machine;
        let base_ns = m.timer_ns_base.load(Ordering::SeqCst) as i128;
        let base_cnt = m.timer_cnt_base.load(Ordering::SeqCst) as i128;
        let hz = host_counter_hz() as i128;
        // The host counter at `timer_ns_base`
        let at_base = ref_ctr as i128 + (base_ns - ref_ns as i128) * hz / 1_000_000_000;
        (at_base - base_cnt) as u64
    }

    /// Whether this CPU runs in hardware.
    pub fn runs_in_hardware(&self) -> bool {
        !matches!(self.hv, Backend::Off)
    }

    /// `run` for a CPU in hardware.
    pub(super) fn run_hw(&mut self, budget: u64) -> Exit {
        self.machine.cpus[self.index]
            .run_state
            .store(smp::RUN_RUNNING, Ordering::SeqCst);
        if let Some(exit) = self.check_shared() {
            return self.leave(exit);
        }
        if let Some(why) = self.hw_cannot_resume() {
            // Every CPU of the machine comes here: they all go on interpreted and compiled, with
            // armless's GIC
            if self.index == 0 {
                eprintln!("armless: {why}; running without the hypervisor");
            }
            self.machine.gic_v3.store(false, Ordering::SeqCst);
            self.hv = Backend::Off;
            return self.run(budget);
        }
        #[cfg(armless_hvf)]
        return self.run_hvf(budget);
        #[cfg(armless_kvm)]
        return self.run_kvm(budget);
    }

    /// The ID registers to show a guest in hardware (`MASKED_ID_REGS`): `sr` key and value.
    pub(super) fn guest_id_regs(&mut self) -> Vec<(u32, u64)> {
        let gic_v3 = self.machine.gic_v3.load(Ordering::Relaxed);
        MASKED_ID_REGS
            .iter()
            .map(|&key| {
                let mut v = self.read_sysreg(key).unwrap_or(0);
                if key == sr::ID_AA64PFR0_EL1 && gic_v3 {
                    v = (v & !(0xf << 24)) | (1 << 24);
                }
                (key, v)
            })
            .collect()
    }

    /// The architectural state into the vCPU.
    pub(super) fn hw_load(&mut self, v: &impl HwVcpu) {
        for r in 0..31 {
            v.set(Reg::X(r), self.x[r as usize]);
        }
        v.set(Reg::Pc, self.pc);
        let nzcv = (self.n as u64) << 31
            | (self.z as u64) << 30
            | (self.c as u64) << 29
            | (self.v as u64) << 28;
        let mode = match (self.el, self.spsel) {
            (0, _) => 0b0000,
            (_, false) => 0b0100,
            (_, true) => 0b0101,
        };
        v.set(Reg::Cpsr, nzcv | self.daif as u64 | mode);
        v.set(Reg::Fpcr, self.sys.fpcr);
        v.set(Reg::Fpsr, self.sys.fpsr);
        v.set(Reg::SpEl0, self.sp_el0);
        v.set(Reg::SpEl1, self.sp_el1);
        v.set(Reg::Sys(sr::ELR_EL1), self.sys.elr);
        v.set(Reg::Sys(sr::SPSR_EL1), self.sys.spsr);
        for &(key, field) in SYS {
            let x = *field(&mut self.sys);
            v.set(Reg::Sys(key), x);
        }
        for (i, x) in self.vreg.iter().enumerate() {
            v.set_vreg(i, *x);
        }
    }

    /// The vCPU's architectural state back into the CPU.
    pub(super) fn hw_store(&mut self, v: &impl HwVcpu) {
        for r in 0..31 {
            self.x[r as usize] = v.get(Reg::X(r));
        }
        self.pc = v.get(Reg::Pc);
        let cpsr = v.get(Reg::Cpsr);
        self.n = cpsr >> 31 & 1 != 0;
        self.z = cpsr >> 30 & 1 != 0;
        self.c = cpsr >> 29 & 1 != 0;
        self.v = cpsr >> 28 & 1 != 0;
        self.daif = cpsr as u32 & DAIF_ALL;
        self.el = (cpsr >> 2 & 3) as u8;
        self.spsel = cpsr & 1 != 0;
        self.sys.fpcr = v.get(Reg::Fpcr);
        self.sys.fpsr = v.get(Reg::Fpsr);
        self.sp_el0 = v.get(Reg::SpEl0);
        self.sp_el1 = v.get(Reg::SpEl1);
        self.sys.elr = v.get(Reg::Sys(sr::ELR_EL1));
        self.sys.spsr = v.get(Reg::Sys(sr::SPSR_EL1));
        for &(key, field) in SYS {
            *field(&mut self.sys) = v.get(Reg::Sys(key));
        }
        for i in 0..32 {
            self.vreg[i] = v.vreg(i);
        }
        // What the interpreter and the JIT cached of the translation regime is stale
        self.tlb_flush();
    }

    /// A device load of `size` bytes at `pa`, as the register gets it (`sign`-extended to 64 bits,
    /// or to 32 and zero-extended unless `wide`).
    pub(super) fn hw_device_read(&mut self, pa: u64, size: u32, sign: bool, wide: bool) -> u64 {
        let mut v = self.phys_read(pa, size).unwrap_or(0);
        if sign && size < 8 {
            let shift = 64 - 8 * size;
            v = ((v << shift) as i64 >> shift) as u64;
        }
        if !wide {
            v &= 0xffff_ffff;
        }
        v
    }

    /// A device store of the low `size` bytes of `v` at `pa`.
    pub(super) fn hw_device_write(&mut self, pa: u64, size: u32, v: u64) {
        let v = if size == 8 { v } else { v & ((1u64 << (8 * size)) - 1) };
        self.phys_write(pa, size, v);
    }

    /// An HVC or SMC: PSCI, with x0..x3 of the vCPU. None: back into the guest.
    pub(super) fn hw_psci(&mut self, v: &impl HwVcpu) -> Option<Exit> {
        for r in 0..4 {
            self.x[r] = v.get(Reg::X(r as u32));
        }
        let res = self.psci_call();
        for r in 0..4 {
            v.set(Reg::X(r as u32), self.x[r]);
        }
        match res {
            Ok(()) => None,
            Err(Event::Stop(reason)) => Some(Exit::Stop(reason)),
            // CPU_OFF: off until a CPU_ON
            Err(_) => Some(Exit::Idle { wake_ns: None }),
        }
    }

    /// Run the instruction at the vCPU's pc with the interpreter (taking what it raises), then go
    /// on in hardware. Err(Wfi) asks the backend to wait.
    pub(super) fn hw_interpret_one(&mut self, v: &impl HwVcpu) -> Result<(), Exit> {
        self.hw_store(v);
        let r = self.step();
        self.hw_load(v);
        match r {
            Err(Event::Stop(reason)) => Err(Exit::Stop(reason)),
            Err(Event::Wfi) => Err(Exit::Idle { wake_ns: None }),
            _ => Ok(()),
        }
    }
}
