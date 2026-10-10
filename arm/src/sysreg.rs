//! System register encodings: key = op0:op1:CRn:CRm:op2 packed into 16 bits.

pub const fn sr(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

/// Decode the key out of an MRS/MSR/SYS instruction.
pub fn key_of(insn: u32) -> u32 {
    (insn >> 5) & 0xffff
}

pub const MIDR_EL1: u32 = sr(3, 0, 0, 0, 0);
pub const MPIDR_EL1: u32 = sr(3, 0, 0, 0, 5);
pub const REVIDR_EL1: u32 = sr(3, 0, 0, 0, 6);
pub const ID_AA64PFR0_EL1: u32 = sr(3, 0, 0, 4, 0);
pub const ID_AA64DFR0_EL1: u32 = sr(3, 0, 0, 5, 0);
pub const ID_AA64ISAR0_EL1: u32 = sr(3, 0, 0, 6, 0);
pub const ID_AA64MMFR0_EL1: u32 = sr(3, 0, 0, 7, 0);
pub const ID_AA64MMFR1_EL1: u32 = sr(3, 0, 0, 7, 1);
pub const ID_AA64MMFR2_EL1: u32 = sr(3, 0, 0, 7, 2);
pub const CCSIDR_EL1: u32 = sr(3, 1, 0, 0, 0);
pub const CLIDR_EL1: u32 = sr(3, 1, 0, 0, 1);
pub const CSSELR_EL1: u32 = sr(3, 2, 0, 0, 0);
pub const CTR_EL0: u32 = sr(3, 3, 0, 0, 1);
pub const DCZID_EL0: u32 = sr(3, 3, 0, 0, 7);

pub const SCTLR_EL1: u32 = sr(3, 0, 1, 0, 0);
pub const CPACR_EL1: u32 = sr(3, 0, 1, 0, 2);
pub const TTBR0_EL1: u32 = sr(3, 0, 2, 0, 0);
pub const TTBR1_EL1: u32 = sr(3, 0, 2, 0, 1);
pub const TCR_EL1: u32 = sr(3, 0, 2, 0, 2);
pub const SPSR_EL1: u32 = sr(3, 0, 4, 0, 0);
pub const ELR_EL1: u32 = sr(3, 0, 4, 0, 1);
pub const SP_EL0: u32 = sr(3, 0, 4, 1, 0);
pub const SP_EL1: u32 = sr(3, 4, 4, 1, 0);
pub const SPSEL: u32 = sr(3, 0, 4, 2, 0);
pub const CURRENT_EL: u32 = sr(3, 0, 4, 2, 2);
pub const PAN: u32 = sr(3, 0, 4, 2, 3);
pub const UAO: u32 = sr(3, 0, 4, 2, 4);
pub const NZCV: u32 = sr(3, 3, 4, 2, 0);
pub const DAIF: u32 = sr(3, 3, 4, 2, 1);
pub const FPCR: u32 = sr(3, 3, 4, 4, 0);
pub const FPSR: u32 = sr(3, 3, 4, 4, 1);
pub const ESR_EL1: u32 = sr(3, 0, 5, 2, 0);
pub const FAR_EL1: u32 = sr(3, 0, 6, 0, 0);
pub const PAR_EL1: u32 = sr(3, 0, 7, 4, 0);
pub const MAIR_EL1: u32 = sr(3, 0, 10, 2, 0);
pub const VBAR_EL1: u32 = sr(3, 0, 12, 0, 0);
pub const ISR_EL1: u32 = sr(3, 0, 12, 1, 0);
pub const CONTEXTIDR_EL1: u32 = sr(3, 0, 13, 0, 1);
pub const TPIDR_EL1: u32 = sr(3, 0, 13, 0, 4);
pub const TPIDR_EL0: u32 = sr(3, 3, 13, 0, 2);
pub const TPIDRRO_EL0: u32 = sr(3, 3, 13, 0, 3);
pub const CNTKCTL_EL1: u32 = sr(3, 0, 14, 1, 0);
pub const CNTFRQ_EL0: u32 = sr(3, 3, 14, 0, 0);
pub const CNTPCT_EL0: u32 = sr(3, 3, 14, 0, 1);
pub const CNTVCT_EL0: u32 = sr(3, 3, 14, 0, 2);
pub const CNTP_TVAL_EL0: u32 = sr(3, 3, 14, 2, 0);
pub const CNTP_CTL_EL0: u32 = sr(3, 3, 14, 2, 1);
pub const CNTP_CVAL_EL0: u32 = sr(3, 3, 14, 2, 2);
pub const CNTV_TVAL_EL0: u32 = sr(3, 3, 14, 3, 0);
pub const CNTV_CTL_EL0: u32 = sr(3, 3, 14, 3, 1);
pub const CNTV_CVAL_EL0: u32 = sr(3, 3, 14, 3, 2);
pub const CNTPCTSS_EL0: u32 = sr(3, 3, 14, 0, 5);
pub const CNTVCTSS_EL0: u32 = sr(3, 3, 14, 0, 6);
pub const OSLSR_EL1: u32 = sr(2, 0, 1, 1, 4);

// SYS instructions (op0 = 1)
pub const DC_ZVA: u32 = sr(1, 3, 7, 4, 1);
pub const AT_S1E1R: u32 = sr(1, 0, 7, 8, 0);
pub const AT_S1E1W: u32 = sr(1, 0, 7, 8, 1);
pub const AT_S1E0R: u32 = sr(1, 0, 7, 8, 2);
pub const AT_S1E0W: u32 = sr(1, 0, 7, 8, 3);

/// Fixed identification values.
pub mod id {
    /// Arm Cortex-A53 r0p4.
    pub const MIDR: u64 = 0x410F_D034;
    /// Uniprocessor, affinity 0.
    pub const MPIDR: u64 = 0x8000_0000 | (1 << 30);
    /// EL0/EL1 AArch64 only, no EL2/EL3. FP and AdvSIMD advertised as
    /// "not implemented" (0xF) until the FP/SIMD unit lands; GIC system
    /// register interface absent (memory-mapped GICv2 instead).
    pub const AA64PFR0: u64 = 0x1 | (0x1 << 4) | (0xF << 16) | (0xF << 20);
    /// PARange=32 bits, 8-bit ASIDs, 4K granule only (16K/64K unsupported).
    pub const AA64MMFR0: u64 = 0x0 | (0xF << 24);
    /// Debug architecture v8, no PMU.
    pub const AA64DFR0: u64 = 0x6;
    /// 64-byte cache lines, PIPT I-cache, IDC/DIC set: no cache maintenance needed.
    pub const CTR: u64 = (1 << 31) | (1 << 29) | (1 << 28) | (4 << 16) | (3 << 14) | 4;
    /// DC ZVA permitted, 64-byte block.
    pub const DCZID: u64 = 4;
    pub const CLIDR: u64 = 0;
}
