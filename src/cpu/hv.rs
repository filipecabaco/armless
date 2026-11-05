//! The hardware backend on macOS with Apple silicon: Hypervisor.framework (see `hw`).
//!
//! The process needs the `com.apple.security.hypervisor` entitlement (an ad-hoc signature is
//! enough); without it the VM can't be made, and the CPU stays with the interpreter and the JIT.

use super::hw::{Backend, HwVcpu, Reg, CTL_ENABLE, CTL_IMASK, CTL_ISTATUS};
use super::*;
use crate::sysreg::{self as sr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[allow(non_camel_case_types)]
mod ffi {
    pub type hv_return_t = i32;
    pub const HV_SUCCESS: hv_return_t = 0;

    #[repr(C)]
    pub struct ExitException {
        pub syndrome: u64,
        pub virtual_address: u64,
        pub physical_address: u64,
    }

    #[repr(C)]
    pub struct VcpuExit {
        pub reason: u32,
        pub exception: ExitException,
    }

    pub const EXIT_CANCELED: u32 = 0;
    pub const EXIT_EXCEPTION: u32 = 1;
    pub const EXIT_VTIMER_ACTIVATED: u32 = 2;

    pub const REG_PC: u32 = 31;
    pub const REG_FPCR: u32 = 32;
    pub const REG_FPSR: u32 = 33;
    pub const REG_CPSR: u32 = 34;
    pub const INTERRUPT_IRQ: u32 = 0;

    pub const MEMORY_RWX: u64 = 7;

    #[link(name = "Hypervisor", kind = "framework")]
    extern "C" {
        pub fn hv_vm_create(config: *mut u8) -> hv_return_t;
        pub fn hv_vm_map(addr: *mut u8, ipa: u64, size: usize, flags: u64) -> hv_return_t;
        pub fn hv_vcpu_create(
            vcpu: *mut u64,
            exit: *mut *const VcpuExit,
            config: *mut u8,
        ) -> hv_return_t;
        pub fn hv_vcpu_destroy(vcpu: u64) -> hv_return_t;
        pub fn hv_vcpu_run(vcpu: u64) -> hv_return_t;
        pub fn hv_vcpus_exit(vcpus: *const u64, count: u32) -> hv_return_t;
        pub fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> hv_return_t;
        pub fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> hv_return_t;
        pub fn hv_vcpu_get_simd_fp_reg(vcpu: u64, reg: u32, value: *mut u128) -> hv_return_t;
        // (vcpu, reg, value in v0): called by `set_simd`, as Rust can't pass a vector by value
        pub fn hv_vcpu_set_simd_fp_reg();
        pub fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> hv_return_t;
        pub fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> hv_return_t;
        pub fn hv_vcpu_set_pending_interrupt(vcpu: u64, kind: u32, pending: bool) -> hv_return_t;
        pub fn hv_vcpu_get_vtimer_mask(vcpu: u64, masked: *mut bool) -> hv_return_t;
        pub fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> hv_return_t;
        pub fn hv_vcpu_get_vtimer_offset(vcpu: u64, offset: *mut u64) -> hv_return_t;
        pub fn hv_vcpu_set_vtimer_offset(vcpu: u64, offset: u64) -> hv_return_t;
        pub fn mach_absolute_time() -> u64;
    }
}

/// The process's VM (Hypervisor.framework allows one), with the guest RAM at physical address 0:
/// the address and length of the host memory mapped, or why there is none.
static VM: OnceLock<Result<(usize, usize), String>> = OnceLock::new();

/// Whether this process can run guests in hardware: makes the VM, with `mem` mapped, once. A later
/// machine must use the same RAM.
#[allow(unsafe_code)]
pub fn available(mem: &crate::ram::Ram) -> Result<(), String> {
    let want = (mem.as_ptr() as usize, mem.len());
    let got = VM.get_or_init(|| {
        // SAFETY: plain calls into Hypervisor.framework; the RAM stays mapped in this process for
        // as long as the machine (and so the VM's use of it) lives.
        unsafe {
            let r = ffi::hv_vm_create(std::ptr::null_mut());
            if r != ffi::HV_SUCCESS {
                return Err(format!(
                    "hv_vm_create: {r:#x} (is the binary signed with com.apple.security.hypervisor?)"
                ));
            }
            let r = ffi::hv_vm_map(want.0 as *mut u8, board::RAM_BASE, want.1, ffi::MEMORY_RWX);
            if r != ffi::HV_SUCCESS {
                return Err(format!("hv_vm_map of {} MiB: {r:#x}", want.1 >> 20));
            }
        }
        Ok(want)
    });
    match got {
        Ok(m) if *m == want => Ok(()),
        Ok(_) => Err("one VM per process, and it maps another machine's RAM".into()),
        Err(e) => Err(e.clone()),
    }
}

/// Make the vCPU `id` (as `PerCpu::hv_vcpu` holds it: 0 is none) leave the guest soon.
#[allow(unsafe_code)]
pub(crate) fn exit_vcpu(id: u64) {
    if id != 0 {
        let vcpu = id - 1;
        // SAFETY: a vCPU id of this process; asking one that is gone to exit is an error, no more.
        unsafe { ffi::hv_vcpus_exit(&vcpu, 1) };
    }
}

/// A CPU's vCPU: made on the thread that runs it (Hypervisor.framework requires it).
pub(crate) struct Vcpu {
    id: u64,
    exit: *const ffi::VcpuExit,
}

#[allow(unsafe_code)]
impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: the vCPU this owns, destroyed once
        unsafe { ffi::hv_vcpu_destroy(self.id) };
    }
}

#[allow(unsafe_code)]
impl Vcpu {
    fn sys(&self, key: u32) -> u64 {
        let mut v = 0;
        // SAFETY: this thread's vCPU (as every call below)
        unsafe { ffi::hv_vcpu_get_sys_reg(self.id, key as u16, &mut v) };
        v
    }

    fn set_sys(&self, key: u32, v: u64) {
        // SAFETY: this thread's vCPU
        unsafe { ffi::hv_vcpu_set_sys_reg(self.id, key as u16, v) };
    }
}

#[allow(unsafe_code)]
impl HwVcpu for Vcpu {
    fn get(&self, r: Reg) -> u64 {
        let reg = match r {
            Reg::X(n) => n,
            Reg::Pc => ffi::REG_PC,
            Reg::Cpsr => ffi::REG_CPSR,
            Reg::Fpcr => ffi::REG_FPCR,
            Reg::Fpsr => ffi::REG_FPSR,
            Reg::SpEl0 => return self.sys(sr::SP_EL0),
            Reg::SpEl1 => return self.sys(sr::SP_EL1),
            Reg::Sys(key) => return self.sys(key),
        };
        let mut v = 0;
        // SAFETY: this thread's vCPU
        unsafe { ffi::hv_vcpu_get_reg(self.id, reg, &mut v) };
        v
    }

    fn set(&self, r: Reg, v: u64) {
        let reg = match r {
            Reg::X(n) => n,
            Reg::Pc => ffi::REG_PC,
            Reg::Cpsr => ffi::REG_CPSR,
            Reg::Fpcr => ffi::REG_FPCR,
            Reg::Fpsr => ffi::REG_FPSR,
            Reg::SpEl0 => return self.set_sys(sr::SP_EL0, v),
            Reg::SpEl1 => return self.set_sys(sr::SP_EL1, v),
            Reg::Sys(key) => return self.set_sys(key, v),
        };
        // SAFETY: this thread's vCPU
        unsafe { ffi::hv_vcpu_set_reg(self.id, reg, v) };
    }

    fn vreg(&self, i: usize) -> u128 {
        let mut v = 0u128;
        // SAFETY: this thread's vCPU; a u128 is the register's 16 bytes
        unsafe { ffi::hv_vcpu_get_simd_fp_reg(self.id, i as u32, &mut v) };
        v
    }

    fn set_vreg(&self, i: usize, v: u128) {
        // SAFETY: this thread's vCPU
        unsafe { set_simd(self.id, i as u32, v) };
    }
}

/// The host's `mach_absolute_time`, the counter Hypervisor.framework's virtual timer offset is
/// against.
#[allow(unsafe_code)]
pub(crate) fn mach_time() -> u64 {
    // SAFETY: a plain call with no arguments
    unsafe { ffi::mach_absolute_time() }
}

/// After this long in the guest, `run` returns to the runner (which pauses and stops CPUs between
/// runs) even if nothing else made it.
const SLICE: Duration = Duration::from_millis(20);

#[allow(unsafe_code)]
impl<H: Host> Cpu<H> {
    pub(super) fn run_hvf(&mut self, _budget: u64) -> Exit {
        let v = match std::mem::take(&mut self.hv) {
            Backend::Hvf(v) => v,
            _ => {
                let mut id = 0u64;
                let mut exit: *const ffi::VcpuExit = std::ptr::null();
                // SAFETY: makes a vCPU for this thread, which owns it from now on
                let r = unsafe { ffi::hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) };
                if r != ffi::HV_SUCCESS {
                    eprintln!("armless: hv_vcpu_create: {r:#x}; CPU {} interprets", self.index);
                    return self.leave(Exit::Budget);
                }
                self.machine.cpus[self.index]
                    .hv_vcpu
                    .store(id + 1, Ordering::SeqCst);
                let v = Vcpu { id, exit };
                // Aff0 is the CPU number, as `psci_call` and the GIC take it
                let mpidr = self.read_sysreg(sr::MPIDR_EL1).unwrap_or(0);
                v.set_sys(sr::MPIDR_EL1, mpidr);
                // The features armless has, no others (`hw::MASKED_ID_REGS`), and what the vCPU
                // then shows: a register Hypervisor.framework doesn't model (ID_AA64ISAR2_EL1
                // before macOS 27) neither sets nor reads, and reads as zero in the guest
                let mut seen = Vec::new();
                for (key, want) in self.guest_id_regs() {
                    let mut got = 0;
                    // SAFETY: this thread's vCPU
                    let read = unsafe {
                        ffi::hv_vcpu_set_sys_reg(v.id, key as u16, want);
                        ffi::hv_vcpu_get_sys_reg(v.id, key as u16, &mut got)
                    };
                    if read != ffi::HV_SUCCESS {
                        got = 0;
                    } else if got != want && self.index == 0 {
                        eprintln!("armless: ID register {key:#x} shows {got:#x}, not {want:#x}: the guest sees the host's");
                    }
                    seen.push((key, got));
                }
                self.record_features(&seen);
                v
            },
        };
        self.hw_load(&v);
        let offset = self.counter_offset();
        // SAFETY: this thread's vCPU
        unsafe { ffi::hv_vcpu_set_vtimer_offset(v.id, offset) };
        let started = Instant::now();
        let exit = loop {
            if self.attn().load(Ordering::Relaxed) != 0 {
                self.attn().store(0, Ordering::Relaxed);
                if let Some(exit) = self.check_shared() {
                    break exit;
                }
                self.sync_host_irqs();
            }
            self.hvf_sync_vtimer(&v);
            let irq = self.irq_asserted();
            // SAFETY: this thread's vCPU; `exit` is the structure Hypervisor.framework gave for it,
            // which it fills in during `hv_vcpu_run`
            let r = unsafe {
                ffi::hv_vcpu_set_pending_interrupt(v.id, ffi::INTERRUPT_IRQ, irq);
                ffi::hv_vcpu_run(v.id)
            };
            if r != ffi::HV_SUCCESS {
                eprintln!("armless: hv_vcpu_run: {r:#x}");
                break Exit::Stop(StopReason::PowerOff);
            }
            // SAFETY: the exit structure of this thread's vCPU, filled in by the run that returned
            let (reason, esr, pa) = unsafe {
                let e = &*v.exit;
                (e.reason, e.exception.syndrome, e.exception.physical_address)
            };
            match reason {
                ffi::EXIT_CANCELED => break Exit::Budget,
                ffi::EXIT_VTIMER_ACTIVATED => self.set_irq_line(board::PPI_VIRT_TIMER, true),
                ffi::EXIT_EXCEPTION => {
                    if let Some(exit) = self.hvf_exception(&v, esr, pa) {
                        break exit;
                    }
                },
                other => {
                    eprintln!("armless: vCPU exit {other}");
                    break Exit::Stop(StopReason::PowerOff);
                },
            }
            if started.elapsed() >= SLICE {
                break Exit::Budget;
            }
        };
        self.hw_store(&v);
        self.hv = Backend::Hvf(v);
        self.leave(exit)
    }

    /// The virtual timer's line follows its state: Hypervisor.framework masks the timer when it
    /// fires (`EXIT_VTIMER_ACTIVATED`, which raises PPI 27); once the guest has dealt with it
    /// (disabled, masked or moved it), the line drops and the timer is unmasked.
    fn hvf_sync_vtimer(&mut self, v: &Vcpu) {
        let mut masked = false;
        // SAFETY: this thread's vCPU
        unsafe { ffi::hv_vcpu_get_vtimer_mask(v.id, &mut masked) };
        if !masked {
            return;
        }
        let ctl = v.sys(sr::CNTV_CTL_EL0);
        if ctl & (CTL_ENABLE | CTL_IMASK | CTL_ISTATUS) != CTL_ENABLE | CTL_ISTATUS {
            self.set_irq_line(board::PPI_VIRT_TIMER, false);
            // SAFETY: this thread's vCPU
            unsafe { ffi::hv_vcpu_set_vtimer_mask(v.id, false) };
        }
    }

    /// A synchronous exception the guest took to the hypervisor (`esr`, with the IPA of a stage-2
    /// fault). None: back into the guest.
    fn hvf_exception(&mut self, v: &Vcpu, esr: u64, pa: u64) -> Option<Exit> {
        let pc = |v: &Vcpu| v.get(Reg::Pc);
        match (esr >> 26) & 0x3f {
            // Data abort from a lower EL: a device access (RAM is mapped, so it never faults)
            0x24 => {
                if esr >> 24 & 1 == 0 {
                    // No syndrome to say what the access was: the interpreter runs it
                    return match self.hw_interpret_one(v) {
                        Ok(()) => None,
                        Err(Exit::Idle { .. }) => Some(Exit::Idle {
                            wake_ns: self.hvf_timer_deadline_ns(v),
                        }),
                        Err(e) => Some(e),
                    };
                }
                let size = 1u32 << (esr >> 22 & 3);
                let rt = (esr >> 16 & 31) as u32;
                if esr >> 6 & 1 != 0 {
                    let x = if rt == 31 { 0 } else { v.get(Reg::X(rt)) };
                    self.hw_device_write(pa, size, x);
                } else {
                    let x = self.hw_device_read(pa, size, esr >> 21 & 1 != 0, esr >> 15 & 1 != 0);
                    if rt != 31 {
                        v.set(Reg::X(rt), x);
                    }
                }
                v.set(Reg::Pc, pc(v) + 4);
                None
            },
            // HVC (the PC is past it already)
            0x16 => self.hw_psci(v),
            // SMC
            0x17 => {
                v.set(Reg::Pc, pc(v) + 4);
                self.hw_psci(v)
            },
            // MSR/MRS of a register the hardware leaves to armless
            0x18 => {
                let key = sr::sr(
                    (esr >> 20 & 3) as u32,
                    (esr >> 14 & 7) as u32,
                    (esr >> 10 & 15) as u32,
                    (esr >> 1 & 15) as u32,
                    (esr >> 17 & 7) as u32,
                );
                let rt = (esr >> 5 & 31) as u32;
                if esr & 1 != 0 {
                    let x = self.read_sysreg(key).unwrap_or(0);
                    if rt != 31 {
                        v.set(Reg::X(rt), x);
                    }
                } else {
                    let x = if rt == 31 { 0 } else { v.get(Reg::X(rt)) };
                    let _ = self.write_sysreg(key, x);
                }
                v.set(Reg::Pc, pc(v) + 4);
                None
            },
            // WFI / WFE
            0x01 => {
                v.set(Reg::Pc, pc(v) + 4);
                if esr & 1 != 0 || self.irq_asserted() {
                    return None;
                }
                Some(Exit::Idle {
                    wake_ns: self.hvf_timer_deadline_ns(v),
                })
            },
            _ => {
                eprintln!("armless: unhandled exit to the hypervisor: ESR {esr:#x} at pc {:#x}", pc(v));
                Some(Exit::Stop(StopReason::UnhandledException {
                    esr,
                    elr: pc(v),
                    far: pa,
                }))
            },
        }
    }

    /// When the guest's virtual timer fires, in `Host::now_ns` time (None: not armed).
    fn hvf_timer_deadline_ns(&mut self, v: &Vcpu) -> Option<u64> {
        let ctl = v.sys(sr::CNTV_CTL_EL0);
        if ctl & (CTL_ENABLE | CTL_IMASK) != CTL_ENABLE {
            return None;
        }
        let cval = v.sys(sr::CNTV_CVAL_EL0);
        let mut offset = 0;
        // SAFETY: this thread's vCPU, and the host's counter
        let now = unsafe {
            ffi::hv_vcpu_get_vtimer_offset(v.id, &mut offset);
            ffi::mach_absolute_time()
        }
        .wrapping_sub(offset);
        let ticks = cval.saturating_sub(now);
        let ns = (ticks as u128 * 1_000_000_000 / super::hw::host_counter_hz() as u128) as u64;
        Some(self.host.now_ns().saturating_add(ns))
    }
}

/// `hv_vcpu_set_simd_fp_reg(vcpu, reg, value)`, whose value is a vector passed in v0.
///
/// # Safety
/// `vcpu` is this thread's vCPU.
#[allow(unsafe_code)]
unsafe fn set_simd(vcpu: u64, reg: u32, value: u128) {
    let f = ffi::hv_vcpu_set_simd_fp_reg as *const () as usize;
    // SAFETY: the call as the C ABI makes it: x0, w1, v0 in, every caller-saved register clobbered
    unsafe {
        core::arch::asm!(
            "fmov d0, {lo}",
            "mov v0.d[1], {hi}",
            "blr {f}",
            lo = in(reg) value as u64,
            hi = in(reg) (value >> 64) as u64,
            f = in(reg) f,
            inout("x0") vcpu => _,
            inout("x1") reg as u64 => _,
            clobber_abi("C"),
        )
    };
}
