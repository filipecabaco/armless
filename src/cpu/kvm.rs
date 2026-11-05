//! The hardware backend on Linux arm64: KVM (see `hw`).
//!
//! armless keeps its own machine: there is no in-kernel GIC (armless's GICv2 drives each vCPU's IRQ
//! line, `KVM_IRQ_LINE`, and the kernel reports the virtual timer's level back), PSCI calls are
//! forwarded to armless (`KVM_ARM_VM_SMCCC_FILTER`), and device accesses the hardware gives no
//! syndrome for come back too (`KVM_CAP_ARM_NISV_TO_USER`). WFI waits in the kernel, so a kick is
//! a signal to the vCPU's thread.

use super::hw::{Backend, HwVcpu, Reg};
use super::*;
use crate::sysreg::{self as sr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// From <linux/kvm.h> (checked against Linux 6.8's).
const KVM_GET_API_VERSION: u64 = 0xae00;
const KVM_CREATE_VM: u64 = 0xae01;
const KVM_GET_VCPU_MMAP_SIZE: u64 = 0xae04;
const KVM_CREATE_VCPU: u64 = 0xae41;
const KVM_SET_USER_MEMORY_REGION: u64 = 0x4020_ae46;
const KVM_RUN: u64 = 0xae80;
const KVM_IRQ_LINE: u64 = 0x4008_ae61;
const KVM_GET_ONE_REG: u64 = 0x4010_aeab;
const KVM_SET_ONE_REG: u64 = 0x4010_aeac;
const KVM_ARM_VCPU_INIT: u64 = 0x4020_aeae;
const KVM_ARM_PREFERRED_TARGET: u64 = 0x8020_aeaf;
const KVM_ENABLE_CAP: u64 = 0x4068_aea3;
const KVM_SET_DEVICE_ATTR: u64 = 0x4018_aee1;

const KVM_EXIT_HYPERCALL: u32 = 3;
const KVM_EXIT_MMIO: u32 = 6;
const KVM_EXIT_INTR: u32 = 10;
const KVM_EXIT_SYSTEM_EVENT: u32 = 24;
const KVM_EXIT_ARM_NISV: u32 = 28;

const KVM_CAP_ARM_NISV_TO_USER: u32 = 177;
const KVM_ARM_VCPU_PSCI_0_2: u32 = 2;
const KVM_HYPERCALL_EXIT_SMC: u64 = 1;
const KVM_ARM_DEV_EL1_VTIMER: u64 = 1;

// struct kvm_run
const RUN_IMMEDIATE_EXIT: usize = 0x1;
const RUN_EXIT_REASON: usize = 0x8;
const RUN_MMIO_ADDR: usize = 0x20;
const RUN_MMIO_DATA: usize = 0x28;
const RUN_MMIO_LEN: usize = 0x30;
const RUN_MMIO_IS_WRITE: usize = 0x34;
const RUN_HYPERCALL_FLAGS: usize = 0x60;
const RUN_DEVICE_IRQ_LEVEL: usize = 0x130;

// KVM_{GET,SET}_ONE_REG ids
const REG_ARM64: u64 = 0x6000_0000_0000_0000;
const SIZE_U32: u64 = 0x0020_0000_0000_0000;
const SIZE_U64: u64 = 0x0030_0000_0000_0000;
const SIZE_U128: u64 = 0x0040_0000_0000_0000;
const CORE: u64 = 0x0010_0000;
const SYSREG: u64 = 0x0013_0000;

/// A field of `struct kvm_regs` at byte offset `off`.
const fn core(size: u64, off: u64) -> u64 {
    REG_ARM64 | size | CORE | (off / 4)
}

/// The process's VM: the VM, the RAM mapped, and whether it has an in-kernel GICv3.
struct Vm {
    vm: i32,
    ram: (usize, usize),
    gic_v3: bool,
}

static VM: OnceLock<Result<Vm, String>> = OnceLock::new();

/// The vCPUs, made with the VM (an in-kernel GIC is set up once all of them exist): each CPU's
/// thread takes its own the first time it runs.
static VCPUS: std::sync::Mutex<Vec<Option<Vcpu>>> = std::sync::Mutex::new(Vec::new());

const KVM_CREATE_DEVICE: u64 = 0xc00c_aee0;
const KVM_DEV_TYPE_ARM_VGIC_V3: u32 = 7;
const VGIC_GRP_ADDR: u32 = 0;
const VGIC_V3_ADDR_DIST: u64 = 2;
const VGIC_V3_ADDR_REDIST: u64 = 3;
const VGIC_GRP_NR_IRQS: u32 = 3;
const VGIC_GRP_CTRL: u32 = 4;
const VGIC_CTRL_INIT: u64 = 0;
const KVM_ARM_IRQ_TYPE_SPI: u32 = 1;
const KVM_ARM_SET_COUNTER_OFFSET: u64 = 0x4010_aeb5;

/// The counter offset last given to KVM (VM-wide: every vCPU sees one counter), or MAX.
static COUNTER_OFFSET: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

#[repr(C)]
struct Attr {
    flags: u32,
    group: u32,
    attr: u64,
    addr: u64,
}

fn set_attr(fd: i32, group: u32, attr: u64, addr: u64) -> std::io::Result<i32> {
    let a = Attr { flags: 0, group, attr, addr };
    ioctl(fd, KVM_SET_DEVICE_ATTR, &a as *const Attr as u64)
}

/// /dev/kvm and a new VM on it (40-bit guest physical addresses), with the size of a `kvm_run`.
#[allow(unsafe_code)]
fn new_vm() -> Result<(i32, usize), String> {
    let path = c"/dev/kvm";
    // SAFETY: opening a device node
    let kvm = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if kvm < 0 {
        return Err(format!("/dev/kvm: {}", std::io::Error::last_os_error()));
    }
    let e = |what: &str, r: std::io::Error| format!("{what}: {r}");
    if ioctl(kvm, KVM_GET_API_VERSION, 0).map_err(|r| e("KVM_GET_API_VERSION", r))? != 12 {
        return Err("KVM API version is not 12".into());
    }
    let vm = ioctl(kvm, KVM_CREATE_VM, 0).map_err(|r| e("KVM_CREATE_VM", r))?;
    let run_size =
        ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0).map_err(|r| e("KVM_GET_VCPU_MMAP_SIZE", r))? as usize;
    Ok((vm, run_size))
}

/// Whether KVM delivers the interrupt armless's own GIC asks for (`KVM_IRQ_LINE` on a vCPU's
/// line): not under some nested hypervisors (Apple's, which Lima and cloud VMs on Macs use), where
/// the line wakes a WFI but the guest never takes it. A guest of a few instructions unmasks IRQs
/// and waits; its IRQ vector stores to an unmapped address, which comes back as an MMIO exit.
/// Ok(true): armless's GIC works here. Ok(false): it needs the in-kernel GICv3.
#[allow(unsafe_code)]
pub fn delivers_irq_line() -> Result<bool, String> {
    let (vm, run_size) = new_vm()?;
    // Page-aligned, as KVM wants guest memory
    // SAFETY: a private anonymous mapping, unmapped below
    let mem = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            0x10000,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mem == libc::MAP_FAILED {
        return Err(format!("probe memory: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the 64 KiB just mapped, as words
    let code = unsafe { std::slice::from_raw_parts_mut(mem as *mut u32, 0x4000) };
    code[0] = 0x1000_4001; // adr x1, #0x800
    code[1] = 0xd518_c001; // msr vbar_el1, x1
    code[2] = 0xd503_3fdf; // isb
    code[3] = 0xd2a2_0002; // mov x2, #0x10000000
    code[4] = 0xd503_42ff; // msr daifclr, #2
    code[5] = 0xd503_207f; // wfi
    code[6] = 0x17ff_ffff; // b wfi
    code[0xa80 / 4] = 0xf900_0040; // IRQ, current EL with SPx: str x0, [x2]
    code[0xa84 / 4] = 0x1400_0000; // b .
    #[repr(C)]
    struct Region {
        slot: u32,
        flags: u32,
        guest_phys_addr: u64,
        memory_size: u64,
        userspace_addr: u64,
    }
    let r = Region {
        slot: 0,
        flags: 0,
        guest_phys_addr: 0,
        memory_size: 0x10000,
        userspace_addr: mem as u64,
    };
    // SAFETY: closes the probe's VM fd, which nothing uses after
    let close = |fd| unsafe { libc::close(fd) };
    let result = (|| {
        ioctl(vm, KVM_SET_USER_MEMORY_REGION, &r as *const Region as u64)
            .map_err(|e| format!("probe memory: {e}"))?;
        let v = Vcpu::make(0, vm, run_size)?;
        v.set(Reg::Pc, 0);
        v.set(Reg::Cpsr, 0x3c5);
        let line = [0u32, 1];
        ioctl(vm, KVM_IRQ_LINE, line.as_ptr() as u64).map_err(|e| format!("KVM_IRQ_LINE: {e}"))?;
        // Out of a WFI that never ends, after a while
        let run = v.run as usize;
        // SAFETY: this thread; a kick from the timer thread below ends KVM_RUN
        let thread = unsafe { libc::pthread_self() } as usize;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let d = done.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            if !d.load(Ordering::SeqCst) {
                let k = Kick { run: run as *mut u8, thread: thread as libc::pthread_t };
                exit_vcpu(&k as *const Kick as u64);
            }
        });
        let ran = ioctl(v.fd, KVM_RUN, 0);
        done.store(true, Ordering::SeqCst);
        let _ = timer.join();
        let taken = ran.is_ok()
            && v.word(RUN_EXIT_REASON) as u32 == KVM_EXIT_MMIO
            && v.word(RUN_MMIO_ADDR) == 0x1000_0000;
        Ok(taken)
    })();
    close(vm);
    // SAFETY: the mapping made above, which nothing uses any more
    unsafe { libc::munmap(mem, 0x10000) };
    result
}

fn ioctl(fd: i32, req: u64, arg: u64) -> std::io::Result<i32> {
    // SAFETY: the KVM ioctls used here take an integer or a pointer to a structure of the size
    // their number encodes, which the callers pass
    #[allow(unsafe_code)]
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// A device's line to the in-kernel GICv3 (`Machine::irq_sink`): SPIs only.
pub fn spi_line(intid: u32, level: bool) {
    if let Some(Ok(vm)) = VM.get() {
        let line = [KVM_ARM_IRQ_TYPE_SPI << 24 | intid, level as u32];
        if let Err(e) = ioctl(vm.vm, KVM_IRQ_LINE, line.as_ptr() as u64) {
            eprintln!("armless: KVM_IRQ_LINE SPI {intid}: {e}");
        }
    }
}

/// Whether this process can run guests in hardware: makes the VM once, with `mem` mapped at
/// physical address 0, `ncpus` vCPUs, and an in-kernel GICv3 if `gic_v3`. A later machine must
/// use the same RAM.
#[allow(unsafe_code)]
pub fn available(mem: &crate::ram::Ram, ncpus: usize, gic_v3: bool) -> Result<(), String> {
    let want = (mem.as_ptr() as usize, mem.len());
    let got = VM.get_or_init(|| {
        let e = |what: &str, r: std::io::Error| format!("{what}: {r}");
        let (vm, run_size) = new_vm()?;
        // PSCI (the fast calls of SMCCC's standard service range, both conventions) to armless
        for base in [0x8400_0000u32, 0xc400_0000] {
            #[repr(C)]
            struct Filter {
                base: u32,
                nr_functions: u32,
                action: u8,
                pad: [u8; 15],
            }
            let f = Filter { base, nr_functions: 0x20, action: 2, pad: [0; 15] };
            set_attr(vm, 0, 0, &f as *const Filter as u64)
                .map_err(|r| e("KVM_ARM_VM_SMCCC_FILTER", r))?;
        }
        #[repr(C)]
        struct EnableCap {
            cap: u32,
            flags: u32,
            args: [u64; 4],
            pad: [u8; 64],
        }
        let c = EnableCap { cap: KVM_CAP_ARM_NISV_TO_USER, flags: 0, args: [0; 4], pad: [0; 64] };
        ioctl(vm, KVM_ENABLE_CAP, &c as *const EnableCap as u64)
            .map_err(|r| e("KVM_CAP_ARM_NISV_TO_USER", r))?;
        #[repr(C)]
        struct Region {
            slot: u32,
            flags: u32,
            guest_phys_addr: u64,
            memory_size: u64,
            userspace_addr: u64,
        }
        let r = Region {
            slot: 0,
            flags: 0,
            guest_phys_addr: board::RAM_BASE,
            memory_size: want.1 as u64,
            userspace_addr: want.0 as u64,
        };
        ioctl(vm, KVM_SET_USER_MEMORY_REGION, &r as *const Region as u64)
            .map_err(|r| e("KVM_SET_USER_MEMORY_REGION", r))?;
        let gic = if gic_v3 {
            let mut dev = [KVM_DEV_TYPE_ARM_VGIC_V3, 0, 0];
            ioctl(vm, KVM_CREATE_DEVICE, dev.as_mut_ptr() as u64).map_err(|r| e("vGICv3", r))?;
            let gic = dev[1] as i32;
            let dist = board::GICD_BASE;
            let redist = board::GICR_BASE;
            set_attr(gic, VGIC_GRP_ADDR, VGIC_V3_ADDR_DIST, &dist as *const u64 as u64)
                .map_err(|r| e("vGICv3 distributor", r))?;
            set_attr(gic, VGIC_GRP_ADDR, VGIC_V3_ADDR_REDIST, &redist as *const u64 as u64)
                .map_err(|r| e("vGICv3 redistributors", r))?;
            let nr_irqs = 128u32;
            set_attr(gic, VGIC_GRP_NR_IRQS, 0, &nr_irqs as *const u32 as u64)
                .map_err(|r| e("vGICv3 interrupts", r))?;
            Some(gic)
        } else {
            None
        };
        let vcpus = (0..ncpus)
            .map(|i| Vcpu::make(i, vm, run_size).map(Some))
            .collect::<Result<Vec<_>, _>>()?;
        *VCPUS.lock().unwrap() = vcpus;
        if let Some(gic) = gic {
            // Once every vCPU exists
            set_attr(gic, VGIC_GRP_CTRL, VGIC_CTRL_INIT, 0).map_err(|r| e("vGICv3 init", r))?;
        }
        Ok(Vm { vm, ram: want, gic_v3 })
    });
    match got {
        Ok(v) if v.ram == want => Ok(()),
        Ok(_) => Err("one VM per process, and it maps another machine's RAM".into()),
        Err(e) => Err(e.clone()),
    }
}

/// What a kick needs to get a vCPU out of the guest (or out of a WFI in the kernel): its
/// `kvm_run`, to set `immediate_exit`, and its thread, to signal. Leaked: kicks may come at any
/// time from any thread.
struct Kick {
    run: *mut u8,
    thread: libc::pthread_t,
}

/// The signal a kick sends: its handler does nothing, but `KVM_RUN` returns (EINTR).
fn kick_signal() -> i32 {
    libc::SIGRTMIN() + 3
}

extern "C" fn on_kick(_: i32) {}

/// Make the vCPU `id` (as `PerCpu::hv_vcpu` holds it: a `Kick`; 0 is none) leave the guest soon.
#[allow(unsafe_code)]
pub(crate) fn exit_vcpu(id: u64) {
    if id != 0 {
        // SAFETY: a leaked `Kick` of this process's vCPU; its `kvm_run` stays mapped
        unsafe {
            let k = &*(id as *const Kick);
            std::ptr::write_volatile(k.run.add(RUN_IMMEDIATE_EXIT), 1);
            libc::pthread_kill(k.thread, kick_signal());
        }
    }
}

/// A CPU's vCPU: its fd and its `kvm_run`.
// SAFETY: a vCPU moves to its CPU's thread once (from `VCPUS`), which alone runs it from then on
#[allow(unsafe_code)]
unsafe impl Send for Vcpu {}

pub(crate) struct Vcpu {
    fd: i32,
    run: *mut u8,
    /// The levels last given to KVM (IRQ line) and to the GIC (timer)
    irq: bool,
    vtimer: bool,
}

#[allow(unsafe_code)]
impl Drop for Vcpu {
    fn drop(&mut self) {
        // SAFETY: the vCPU's fd; its `kvm_run` stays mapped for kicks that may still come
        unsafe { libc::close(self.fd) };
    }
}

#[allow(unsafe_code)]
impl Vcpu {
    fn make(index: usize, vm: i32, run_size: usize) -> Result<Vcpu, String> {
        static HANDLER: OnceLock<()> = OnceLock::new();
        HANDLER.get_or_init(|| {
            // SAFETY: a handler that does nothing, without SA_RESTART so KVM_RUN returns EINTR
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_kick as *const () as usize;
                libc::sigaction(kick_signal(), &sa, std::ptr::null_mut());
            }
        });
        let e = |what: &str, r: std::io::Error| format!("{what}: {r}");
        let fd = ioctl(vm, KVM_CREATE_VCPU, index as u64).map_err(|r| e("KVM_CREATE_VCPU", r))?;
        // SAFETY: the vCPU's kvm_run, shared with the kernel
        let run = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                run_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if run == libc::MAP_FAILED {
            return Err(format!("mmap of kvm_run: {}", std::io::Error::last_os_error()));
        }
        let mut init = [0u32; 8];
        ioctl(vm, KVM_ARM_PREFERRED_TARGET, init.as_mut_ptr() as u64)
            .map_err(|r| e("KVM_ARM_PREFERRED_TARGET", r))?;
        init[1] |= 1 << KVM_ARM_VCPU_PSCI_0_2;
        ioctl(fd, KVM_ARM_VCPU_INIT, init.as_ptr() as u64).map_err(|r| e("KVM_ARM_VCPU_INIT", r))?;
        Ok(Vcpu { fd, run: run as *mut u8, irq: false, vtimer: false })
    }

    /// Show the guest the ID register `key` as `want`, as far as KVM lets it: KVM refuses a write
    /// that changes a field it doesn't let change, so whole, else field by field from the host's
    /// value. What the guest sees.
    fn set_id_reg(&self, key: u32, want: u64) -> u64 {
        let id = REG_ARM64 | SIZE_U64 | SYSREG | key as u64;
        let write = |value: u64| {
            let mut value = value;
            let r = [id, &mut value as *mut u64 as u64];
            ioctl(self.fd, KVM_SET_ONE_REG, r.as_ptr() as u64).is_ok()
        };
        if write(want) {
            return want;
        }
        let mut have = 0u64;
        self.one_reg(id, false, &mut have as *mut u64 as *mut u8);
        for field in 0..16 {
            let mask = 0xfu64 << (4 * field);
            if have & mask == want & mask {
                continue;
            }
            let next = (have & !mask) | (want & mask);
            if write(next) {
                have = next;
            }
        }
        have
    }

    fn one_reg(&self, id: u64, set: bool, value: *mut u8) {
        let r = [id, value as u64];
        let _ = ioctl(self.fd, if set { KVM_SET_ONE_REG } else { KVM_GET_ONE_REG }, r.as_ptr() as u64);
    }

    fn id(r: Reg) -> u64 {
        match r {
            Reg::X(n) => core(SIZE_U64, 8 * n as u64),
            Reg::SpEl0 => core(SIZE_U64, 0xf8),
            Reg::Pc => core(SIZE_U64, 0x100),
            Reg::Cpsr => core(SIZE_U64, 0x108),
            Reg::SpEl1 => core(SIZE_U64, 0x110),
            Reg::Fpsr => core(SIZE_U32, 0x350),
            Reg::Fpcr => core(SIZE_U32, 0x354),
            Reg::Sys(sr::ELR_EL1) => core(SIZE_U64, 0x118),
            Reg::Sys(sr::SPSR_EL1) => core(SIZE_U64, 0x120),
            // KVM's ids for CNTV_CVAL_EL0 and CNTVCT_EL0 are each other's encodings
            Reg::Sys(sr::CNTV_CVAL_EL0) => REG_ARM64 | SIZE_U64 | SYSREG | sr::CNTVCT_EL0 as u64,
            Reg::Sys(key) => REG_ARM64 | SIZE_U64 | SYSREG | key as u64,
        }
    }

    fn word(&self, at: usize) -> u64 {
        // SAFETY: a field inside this vCPU's kvm_run
        unsafe { std::ptr::read_volatile(self.run.add(at) as *const u64) }
    }
}

impl HwVcpu for Vcpu {
    fn get(&self, r: Reg) -> u64 {
        let mut v = 0u64;
        self.one_reg(Self::id(r), false, &mut v as *mut u64 as *mut u8);
        if matches!(r, Reg::Fpsr | Reg::Fpcr) {
            v &= 0xffff_ffff;
        }
        v
    }

    fn set(&self, r: Reg, v: u64) {
        let mut v = v;
        self.one_reg(Self::id(r), true, &mut v as *mut u64 as *mut u8);
    }

    fn vreg(&self, i: usize) -> u128 {
        let mut v = 0u128;
        self.one_reg(core(SIZE_U128, 0x150 + 16 * i as u64), false, &mut v as *mut u128 as *mut u8);
        v
    }

    fn set_vreg(&self, i: usize, v: u128) {
        let mut v = v;
        self.one_reg(core(SIZE_U128, 0x150 + 16 * i as u64), true, &mut v as *mut u128 as *mut u8);
    }
}

/// After this long in the guest, `run` returns to the runner (which pauses and stops CPUs between
/// runs) even if nothing else made it.
const SLICE: Duration = Duration::from_millis(20);

#[allow(unsafe_code)]
impl<H: Host> Cpu<H> {
    pub(super) fn run_kvm(&mut self, _budget: u64) -> Exit {
        let Some(Ok(vm)) = VM.get() else {
            return self.leave(Exit::Stop(StopReason::PowerOff));
        };
        let mut v = match std::mem::take(&mut self.hv) {
            Backend::Kvm(v) => v,
            _ => match VCPUS
                .lock()
                .unwrap()
                .get_mut(self.index)
                .and_then(Option::take)
                .ok_or_else(|| format!("no vCPU {}", self.index))
            {
                Ok(v) => {
                    // The features armless has, no others (`hw::MASKED_ID_REGS`): before the
                    // vCPU first runs, as KVM wants
                    let mut kept = Vec::new();
                    let mut seen = Vec::new();
                    for (key, want) in self.guest_id_regs() {
                        let got = v.set_id_reg(key, want);
                        if got != want {
                            kept.push(format!("{key:#x} {got:#x} (armless {want:#x})"));
                        }
                        seen.push((key, got));
                    }
                    self.record_features(&seen);
                    if self.index == 0 && !kept.is_empty() {
                        eprintln!("armless: KVM keeps some of the host's features: {}", kept.join(", "));
                    }
                    // SAFETY: this thread, which runs the vCPU from now on
                    let thread = unsafe { libc::pthread_self() };
                    let kick = Box::leak(Box::new(Kick { run: v.run, thread }));
                    self.machine.cpus[self.index]
                        .hv_vcpu
                        .store(kick as *const Kick as u64, Ordering::SeqCst);
                    v
                },
                Err(e) => {
                    eprintln!("armless: {e}; CPU {} interprets", self.index);
                    return self.leave(Exit::Budget);
                },
            },
        };
        self.hw_load(&v);
        let offset = self.counter_offset();
        if COUNTER_OFFSET.swap(offset, Ordering::SeqCst) != offset {
            let o = [offset, 0];
            if let Err(e) = ioctl(vm.vm, KVM_ARM_SET_COUNTER_OFFSET, o.as_ptr() as u64) {
                eprintln!("armless: KVM_ARM_SET_COUNTER_OFFSET: {e}");
            }
        }
        let started = Instant::now();
        let exit = loop {
            if self.attn().load(Ordering::Relaxed) != 0 {
                self.attn().store(0, Ordering::Relaxed);
                if let Some(exit) = self.check_shared() {
                    break exit;
                }
                self.sync_host_irqs();
            }
            if !vm.gic_v3 {
                // The timer's level, as the kernel last reported it, to armless's GIC; the GIC's
                // output to the vCPU's IRQ line
                let vtimer = v.word(RUN_DEVICE_IRQ_LEVEL) & KVM_ARM_DEV_EL1_VTIMER != 0;
                if vtimer != v.vtimer {
                    v.vtimer = vtimer;
                    self.set_irq_line(board::PPI_VIRT_TIMER, vtimer);
                }
                let irq = self.irq_asserted();
                if irq != v.irq {
                    v.irq = irq;
                    let line = [(self.index as u32) << 16, irq as u32];
                    if let Err(e) = ioctl(vm.vm, KVM_IRQ_LINE, line.as_ptr() as u64) {
                        eprintln!("armless: KVM_IRQ_LINE {irq}: {e}");
                    }
                }
            }
            if let Err(e) = ioctl(v.fd, KVM_RUN, 0) {
                // SAFETY: this vCPU's kvm_run
                unsafe { std::ptr::write_volatile(v.run.add(RUN_IMMEDIATE_EXIT), 0) };
                if e.raw_os_error() == Some(libc::EINTR) {
                    break Exit::Budget;
                }
                eprintln!("armless: KVM_RUN: {e}");
                break Exit::Stop(StopReason::PowerOff);
            }
            match v.word(RUN_EXIT_REASON) as u32 {
                KVM_EXIT_MMIO => {
                    let pa = v.word(RUN_MMIO_ADDR);
                    // SAFETY: this vCPU's kvm_run
                    let len = unsafe { std::ptr::read_volatile(v.run.add(RUN_MMIO_LEN) as *const u32) };
                    // SAFETY: this vCPU's kvm_run
                    let write = unsafe { std::ptr::read_volatile(v.run.add(RUN_MMIO_IS_WRITE)) } != 0;
                    if write {
                        let x = v.word(RUN_MMIO_DATA);
                        self.hw_device_write(pa, len, x);
                    } else {
                        // KVM sign-extends and narrows it as the instruction asked
                        let x = self.hw_device_read(pa, len, false, true);
                        // SAFETY: this vCPU's kvm_run
                        unsafe { std::ptr::write_volatile(v.run.add(RUN_MMIO_DATA) as *mut u64, x) };
                    }
                },
                KVM_EXIT_HYPERCALL => {
                    // HVC returns past itself; an SMC leaves the PC at it
                    if v.word(RUN_HYPERCALL_FLAGS) & KVM_HYPERCALL_EXIT_SMC != 0 {
                        v.set(Reg::Pc, v.get(Reg::Pc) + 4);
                    }
                    if let Some(exit) = self.hw_psci(&v) {
                        break exit;
                    }
                },
                KVM_EXIT_ARM_NISV => {
                    if let Err(exit) = self.hw_interpret_one(&v) {
                        break exit;
                    }
                },
                KVM_EXIT_INTR => {},
                KVM_EXIT_SYSTEM_EVENT => break Exit::Stop(StopReason::PowerOff),
                reason => {
                    // KVM_EXIT_FAIL_ENTRY, KVM_EXIT_INTERNAL_ERROR, or one this does not expect
                    eprintln!("armless: KVM exit {reason} at pc {:#x}", v.get(Reg::Pc));
                    break Exit::Stop(StopReason::PowerOff);
                },
            }
            if started.elapsed() >= SLICE {
                break Exit::Budget;
            }
        };
        self.hw_store(&v);
        self.hv = Backend::Kvm(v);
        self.leave(exit)
    }
}
