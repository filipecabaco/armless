//! What EL0 may do with the system registers and the system instructions, as the architecture
//! lets it: SCTLR_EL1 and CNTKCTL_EL1 decide some, the rest it has always or never.

use super::*;

struct NoDevices;
impl Host for NoDevices {
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
}

/// MRS x5, `key`
fn mrs(key: u32) -> u32 {
    0xd520_0000 | key << 5 | 5
}

/// MSR `key`, x5 (and SYS with op0 = 1 in `key`)
fn msr(key: u32) -> u32 {
    0xd500_0000 | key << 5 | 5
}

/// A CPU at EL0 with SCTLR_EL1 and CNTKCTL_EL1 as given
fn el0(sctlr: u64, cntkctl: u64) -> Cpu<NoDevices> {
    let mut c = Cpu::new(1 << 20, NoDevices);
    c.el = 0;
    c.sys.sctlr = sctlr;
    c.sys.cntkctl = cntkctl;
    c
}

/// What `insn` does at EL0: Ok, or the exception class and ISS it raises
fn run(c: &mut Cpu<NoDevices>, insn: u32) -> Result<(), (u32, u32)> {
    match c.execute(insn) {
        Ok(()) => Ok(()),
        Err(Event::Sync { ec, iss, .. }) => Err((ec, iss)),
        Err(e) => panic!("{insn:#x}: {e:?}"),
    }
}

fn undefined(c: &mut Cpu<NoDevices>, insn: u32) {
    assert_eq!(run(c, insn), Err((ec::UNKNOWN, 0)), "{insn:#x}");
}

fn trapped(c: &mut Cpu<NoDevices>, insn: u32) {
    assert!(
        matches!(run(c, insn), Err((ec::SYS64, _))),
        "{insn:#x}: {:?}",
        run(c, insn)
    );
}

#[test]
fn el0_has_the_flags_the_fp_control_and_its_thread_registers_always() {
    let mut c = el0(0, 0);
    for key in [sr::NZCV, sr::FPCR, sr::FPSR, sr::TPIDR_EL0] {
        c.x[5] = 0x1234;
        assert_eq!(run(&mut c, msr(key)), Ok(()), "{key:#x}");
        assert_eq!(run(&mut c, mrs(key)), Ok(()), "{key:#x}");
    }
    assert_eq!(c.sys.tpidr_el0, 0x1234);
    // TPIDRRO_EL0 and DCZID_EL0 it may read only
    assert_eq!(run(&mut c, mrs(sr::TPIDRRO_EL0)), Ok(()));
    assert_eq!(run(&mut c, mrs(sr::DCZID_EL0)), Ok(()));
    undefined(&mut c, msr(sr::TPIDRRO_EL0));
    undefined(&mut c, msr(sr::DCZID_EL0));
}

#[test]
fn el1_registers_and_unknown_ones_are_undefined_at_el0() {
    let mut c = el0(u64::MAX, u64::MAX);
    for key in [
        sr::SCTLR_EL1,
        sr::MIDR_EL1,
        sr::TPIDR_EL1,
        sr::SP_EL0,
        sr::CURRENT_EL,
        sr::VBAR_EL1,
    ] {
        undefined(&mut c, mrs(key));
        undefined(&mut c, msr(key));
    }
    // op1 = 3, but none EL0 has: PMCR_EL0, and a debug register (op0 = 2)
    undefined(&mut c, mrs(sr::sr(3, 3, 9, 12, 0)));
    undefined(&mut c, mrs(sr::sr(2, 3, 0, 4, 0)));
    // Read-only ones written
    for key in [sr::CTR_EL0, sr::CNTFRQ_EL0, sr::CNTVCT_EL0, sr::CNTPCT_EL0] {
        undefined(&mut c, msr(key));
    }
}

#[test]
fn sctlr_decides_whether_el0_has_daif_and_ctr() {
    let mut c = el0(0, 0);
    trapped(&mut c, mrs(sr::DAIF));
    trapped(&mut c, msr(sr::DAIF));
    trapped(&mut c, mrs(sr::CTR_EL0));
    let mut c = el0(1 << 9 | 1 << 15, 0);
    assert_eq!(run(&mut c, mrs(sr::DAIF)), Ok(()));
    assert_eq!(run(&mut c, mrs(sr::CTR_EL0)), Ok(()));
}

#[test]
fn cntkctl_decides_which_counters_and_timers_el0_has() {
    let mut c = el0(0, 0);
    for key in [
        sr::CNTFRQ_EL0,
        sr::CNTVCT_EL0,
        sr::CNTPCT_EL0,
        sr::CNTV_CTL_EL0,
        sr::CNTP_CVAL_EL0,
    ] {
        trapped(&mut c, mrs(key));
    }
    // EL0VCTEN: the virtual counter and the frequency, not the physical counter or a timer
    let mut c = el0(0, 1 << 1);
    assert_eq!(run(&mut c, mrs(sr::CNTVCT_EL0)), Ok(()));
    assert_eq!(run(&mut c, mrs(sr::CNTFRQ_EL0)), Ok(()));
    trapped(&mut c, mrs(sr::CNTPCT_EL0));
    trapped(&mut c, mrs(sr::CNTV_TVAL_EL0));
    // EL0PCTEN, EL0VTEN, EL0PTEN
    let mut c = el0(0, 1 | 1 << 8 | 1 << 9);
    for key in [
        sr::CNTPCT_EL0,
        sr::CNTFRQ_EL0,
        sr::CNTV_CTL_EL0,
        sr::CNTP_CTL_EL0,
    ] {
        assert_eq!(run(&mut c, mrs(key)), Ok(()), "{key:#x}");
    }
    assert_eq!(run(&mut c, msr(sr::CNTV_CVAL_EL0)), Ok(()));
    trapped(&mut c, mrs(sr::CNTVCT_EL0));
}

#[test]
fn a_trapped_access_says_which_register_and_which_way() {
    let mut c = el0(0, 0);
    // MRS x5, CNTVCT_EL0: Op0 3, Op2 2, Op1 3, CRn 14, Rt 5, CRm 0, a read
    let iss = 3 << 20 | 2 << 17 | 3 << 14 | 14 << 10 | 5 << 5 | 1;
    assert_eq!(run(&mut c, mrs(sr::CNTVCT_EL0)), Err((ec::SYS64, iss)));
    // MSR DAIF, x5: Op2 1, CRn 4, CRm 2, a write
    let iss = 3 << 20 | 1 << 17 | 3 << 14 | 4 << 10 | 5 << 5 | 2 << 1;
    assert_eq!(run(&mut c, msr(sr::DAIF)), Err((ec::SYS64, iss)));
}

#[test]
fn sctlr_uci_decides_whether_el0_maintains_caches_and_nothing_else_is_its() {
    let mut c = el0(0, 0);
    c.x[5] = 0x1000;
    for key in [sr::IC_IVAU, sr::DC_CVAU, sr::DC_CVAC, sr::DC_CIVAC] {
        trapped(&mut c, msr(key));
    }
    let mut c = el0(1 << 26, 0);
    c.x[5] = 0x1000;
    for key in [
        sr::IC_IVAU,
        sr::DC_CVAU,
        sr::DC_CVAC,
        sr::DC_CIVAC,
        sr::DC_CVAP,
    ] {
        assert_eq!(run(&mut c, msr(key)), Ok(()), "{key:#x}");
    }
    // TLB invalidation, address translation and the instruction cache as a whole: EL1's
    for key in [sr::IC_IALLU, sr::AT_S1E1R, sr::sr(1, 0, 8, 7, 0)] {
        undefined(&mut c, msr(key));
    }
}

/// A CPU at EL1 with the MMU off, x1 at `addr`
fn at(addr: u64) -> Cpu<NoDevices> {
    let mut c = Cpu::new(1 << 20, NoDevices);
    c.x[1] = addr;
    c
}

/// The fault `insn` raises with x1 at `addr`: an Alignment fault there, WnR for a write
fn misaligned(insn: u32, addr: u64, write: bool) {
    let mut c = at(addr);
    match c.execute(insn) {
        Err(Event::Sync {
            ec: ec::DABT_CUR,
            iss,
            far: Some(far),
            ..
        }) => {
            assert_eq!(
                (iss & 0x3f, iss >> 6 & 1, far),
                (0x21, write as u32, addr),
                "{insn:#x}"
            );
        },
        r => panic!("{insn:#x} at {addr:#x}: {r:?}"),
    }
}

const LDXR_X0: u32 = 0xc85f_7c20; // ldxr x0, [x1]
const STXR_X0: u32 = 0xc802_7c20; // stxr w2, x0, [x1]
const LDXR_W0: u32 = 0x885f_7c20; // ldxr w0, [x1]
const LDXP_X0: u32 = 0xc87f_0c20; // ldxp x0, x3, [x1]
const STXP_W0: u32 = 0x8822_0c20; // stxp w2, w0, w3, [x1]
const LDAR_X0: u32 = 0xc8df_fc20; // ldar x0, [x1]
const STLR_W0: u32 = 0x889f_fc20; // stlr w0, [x1]
const LDAXRH: u32 = 0x485f_fc20; // ldaxrh w0, [x1]
const LDARB: u32 = 0x08df_fc20; // ldarb w0, [x1]

#[test]
fn an_exclusive_or_acquire_release_access_unaligned_to_its_size_is_an_alignment_fault() {
    misaligned(LDXR_X0, 0x1004, false);
    misaligned(LDXR_W0, 0x1002, false);
    misaligned(LDAXRH, 0x1001, false);
    misaligned(LDAR_X0, 0x1001, false);
    misaligned(STLR_W0, 0x1006, true);
    // A store-exclusive with no monitor armed faults all the same
    misaligned(STXR_X0, 0x1008 + 4, true);
    // A pair: aligned to both registers
    misaligned(LDXP_X0, 0x1008, false);
    misaligned(STXP_W0, 0x1004, true);
    // Aligned: done
    for (insn, addr) in [
        (LDXR_X0, 0x1008),
        (LDXP_X0, 0x1010),
        (LDAR_X0, 0x1000),
        (STLR_W0, 0x1004),
        (LDARB, 0x1003),
    ] {
        let mut c = at(addr);
        assert!(c.execute(insn).is_ok(), "{insn:#x} at {addr:#x}");
    }
}

#[test]
fn a_store_exclusive_to_where_its_load_was_succeeds_and_an_unaligned_one_never_stores() {
    let mut c = at(0x1008);
    c.x[0] = 0xdead;
    assert!(c.execute(LDXR_X0).is_ok());
    c.x[0] = 0x1234;
    assert!(c.execute(STXR_X0).is_ok());
    assert_eq!((c.x[2], c.read(0x1008, 8, false).unwrap()), (0, 0x1234));
    // Armed again, then the store goes to an unaligned address of the same granule
    assert!(c.execute(LDXR_X0).is_ok());
    c.x[1] = 0x100c;
    assert!(c.execute(STXR_X0).is_err());
    assert_eq!(c.read(0x1008, 8, false).unwrap(), 0x1234);
}
