//! Pre-decoded instructions for the interpreter: per physical code page, one `Op` for each instruction
//! word, which `Cpu::run_ops` executes in a loop with the pc, the budget and the op in locals.
//!
//! An op is 16 bytes: its kind, the operands as the loop wants them (register slots, an immediate, the
//! target of a branch as an index into the page), and the count of its executions that decides when the JIT
//! compiles the page. The kinds are the hot instructions (integer arithmetic and logic, bitfields, the
//! conditional selects and compares, multiplies and divides, loads and stores of X and of SIMD&FP registers
//! that the fast tables serve, the branches), and two generic ones:
//! `Gen` runs the instruction through `exec_slow`, the same leaf `execute` reaches, and leaves the loop;
//! `GenPure` does so for a data-processing instruction, which has nothing to do with the pc or the code (and
//! for a system instruction that only reads), and stays in the loop. An op that is `Miss` is not decoded yet:
//! the loop decodes the instructions from there to the end of the block (`Cpu::fill_ops`).
//!
//! The ops of a page live in a slot of the cache of the CPU (`Cache`, `Jit::pre`), 16 KiB each, in memory that
//! stays allocated for as long as the cache lives, so the loop can hold a pointer to them. The cache holds
//! at most `MAX_SLOTS` slots (24 MiB) and, past that, gives the slot of the page that was used the longest
//! ago to the next page. A slot's ops are dropped wherever the JIT's code is: a store to the page, an
//! instruction-cache invalidation, a DMA write into it, a full flush.

use crate::bits::{bit, decode_bit_masks, field, sext};
use crate::board::Host;
use crate::cpu::{Cpu, Res};
use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};

/// Instructions in a 4 KiB page.
pub(crate) const PAGE_INSNS: usize = 1024;
/// Ops in a slot: one per instruction, and the `End` that follows the last.
pub(crate) const SLOT_OPS: usize = PAGE_INSNS + 1;
/// Pages kept decoded (16 KiB each: 24 MiB); past that the slot of the page used the longest ago is reused.
const MAX_SLOTS: usize = 1536;
/// Instructions decoded at once, at most, when the loop meets one that is not decoded yet.
pub(crate) const BLOCK: usize = 64;
pub(crate) const COUNT_MAX: u32 = 0xffff;
/// No slot.
pub(crate) const NONE: u32 = u32::MAX;
/// `Op::x` of a branch whose target is not in its own page.
pub(crate) const OUT: u16 = 0x8000;

/// What an op does. 0 is an instruction that is not decoded yet.
///
/// Operands: `a` is the register slot written (or, for a store, read), `b` the first source, `c` the second;
/// slot 31 reads as zero and slot 32 takes a write that is discarded, so XZR needs no test (see `Cpu::x`).
/// The stack pointer is a slot of its own, which only the memory kinds and the few that take SP reach (`b`,
/// and `a` for the kinds that write it, are 31 for SP).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum K {
    Miss = 0,
    End,
    Gen,
    GenPure,
    Nop,
    B,
    Bl,
    Bcond,
    Cbz64,
    Cbnz64,
    Cbz32,
    Cbnz32,
    Tbz,
    Tbnz,
    Br,
    Blr,
    Adr,
    MovI,
    Movk64,
    Movk32,
    MovR64,
    MovR32,
    AddI64,
    AddI32,
    AddsI64,
    AddsI32,
    SubsI64,
    SubsI32,
    AndI64,
    OrrI64,
    EorI64,
    AndsI64,
    AndI32,
    OrrI32,
    EorI32,
    AndsI32,
    Ubfx,
    Ubfiz,
    Sbfx64,
    Sbfx32,
    Bfm,
    AddR64,
    AddR32,
    SubR64,
    SubR32,
    AddsR64,
    AddsR32,
    SubsR64,
    SubsR32,
    AddRs64,
    AddRs32,
    SubRs64,
    SubRs32,
    AddsRs64,
    AddsRs32,
    SubsRs64,
    SubsRs32,
    AndR64,
    OrrR64,
    EorR64,
    AndsR64,
    AndR32,
    OrrR32,
    EorR32,
    AndsR32,
    Csel64,
    Csinc64,
    Csinv64,
    Csneg64,
    Csel32,
    Csinc32,
    Csinv32,
    Csneg32,
    Shiftv64,
    Shiftv32,
    Madd64,
    Msub64,
    Madd32,
    Msub32,
    LdrLit32,
    LdrLit64,
    LdrLitSw,
    St1Off,
    St2Off,
    St4Off,
    St8Off,
    Ld1Off,
    Ld2Off,
    Ld4Off,
    Ld8Off,
    St1Pre,
    St2Pre,
    St4Pre,
    St8Pre,
    Ld1Pre,
    Ld2Pre,
    Ld4Pre,
    Ld8Pre,
    St1Post,
    St2Post,
    St4Post,
    St8Post,
    Ld1Post,
    Ld2Post,
    Ld4Post,
    Ld8Post,
    St1Reg,
    St2Reg,
    St4Reg,
    St8Reg,
    Ld1Reg,
    Ld2Reg,
    Ld4Reg,
    Ld8Reg,
    Ls1Off,
    Ls2Off,
    Ls4Off,
    Lw1Off,
    Lw2Off,
    Stp4Off,
    Stp8Off,
    Ldp4Off,
    Ldp8Off,
    Stp4Pre,
    Stp8Pre,
    Ldp4Pre,
    Ldp8Pre,
    Stp4Post,
    Stp8Post,
    Ldp4Post,
    Ldp8Post,
    AddSp64,
    AddExt64,
    SubExt64,
    AddsExt64,
    SubsExt64,
    AndRs64,
    OrrRs64,
    EorRs64,
    AndsRs64,
    AndRs32,
    OrrRs32,
    EorRs32,
    AndsRs32,
    Ccmp64,
    Ccmn64,
    Ccmp32,
    Ccmn32,
    CcmpI64,
    CcmnI64,
    CcmpI32,
    CcmnI32,
    AdcSbc,
    Dp1,
    Div,
    Smaddl,
    Smsubl,
    Umaddl,
    Umsubl,
    Smulh,
    Umulh,
    Extr64,
    Extr32,
    Dmb,
    VLdLit,
    VLdp,
    VStp,
    VLdOff,
    VStOff,
    VLdPre,
    VStPre,
    VLdPost,
    VStPost,
    VLdReg,
    VStReg,
    // A compare (SUBS, CMP) and the B.cond that follows it, by the class of the condition (`cond >> 1`: EQ, CS, MI,
    // VS, HI, GE, GT), see `fuse_cmp_bcond`. The compare's operands as it has them; `x` holds the shift that
    // moves a 32-bit operand to the top of 64 bits (0 or 32) and, in bit 8, whether the branch takes the
    // opposite of the class's condition.
    FSubIEq,
    FSubICs,
    FSubIMi,
    FSubIVs,
    FSubIHi,
    FSubIGe,
    FSubIGt,
    FSubREq,
    FSubRCs,
    FSubRMi,
    FSubRVs,
    FSubRHi,
    FSubRGe,
    FSubRGt,
}

/// The fused kinds of the compare with an immediate and of the compare of registers, by condition class.
const FUSED_IMM: [K; 7] = [
    K::FSubIEq,
    K::FSubICs,
    K::FSubIMi,
    K::FSubIVs,
    K::FSubIHi,
    K::FSubIGe,
    K::FSubIGt,
];
const FUSED_REG: [K; 7] = [
    K::FSubREq,
    K::FSubRCs,
    K::FSubRMi,
    K::FSubRVs,
    K::FSubRHi,
    K::FSubRGe,
    K::FSubRGt,
];

/// Makes the compare at `c` and the conditional branch at `b`, the op after it, one op if they are a SUBS
/// with an immediate or a register (not shifted; 64 or 32 bits) and a B.cond on a condition other than AL
/// and NV: the compare becomes the fused kind of the condition, and the loop runs the branch right after it
/// from `b`, which stays an op of its own for a branch that lands on it.
///
/// # Safety
/// `c` and `b` point at ops of a slot, `b` right after `c`.
pub(crate) unsafe fn fuse_cmp_bcond(c: *mut Op, b: *const Op) {
    if (*b).k != K::Bcond || (*b).a >= 14 {
        return;
    }
    let class = ((*b).a >> 1) as usize;
    let inv = ((*b).a & 1) as u16;
    let (kinds, sh) = match (*c).k {
        K::SubsI64 => (&FUSED_IMM, 0),
        K::SubsI32 => (&FUSED_IMM, 32),
        K::SubsR64 => (&FUSED_REG, 0),
        K::SubsR32 => (&FUSED_REG, 32),
        _ => return,
    };
    (*c).k = kinds[class];
    (*c).x = sh | inv << 8;
}

/// One decoded instruction (see the module comment).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Op {
    pub k: K,
    pub a: u8,
    pub b: u8,
    pub c: u8,
    /// Executions counted for the JIT (`Cache::bump`, `Cpu::run_ops`).
    pub cnt: u16,
    /// A condition, a bit position, a shift, or the index of the target of a branch, as the kind says
    /// (`OUT` if the target is not in the page).
    pub x: u16,
    /// An immediate, the address relative to the page of a branch target, or for the memory kinds the
    /// instruction word in the high half and the offset in the low half. `Gen` and `GenPure` hold their entry.
    pub imm: u64,
}

impl K {
    /// Whether the op is a compare fused with the conditional branch at the next word (`fuse_cmp_bcond`).
    #[inline(always)]
    pub(crate) fn is_fused(self) -> bool {
        FUSED_IMM.contains(&self) || FUSED_REG.contains(&self)
    }

    /// Whether the instructions after an op of this kind are not likely to be run right after it.
    #[inline(always)]
    pub(crate) fn ends_block(self) -> bool {
        matches!(self, K::B | K::Bl | K::Br | K::Blr | K::Gen)
    }
}

// Leaves dispatched to directly. 0 means the entry is not decoded yet; GENERIC runs `execute`.
const GENERIC: u8 = 1;
const B_BL: u8 = 2;
const CBZ_CBNZ: u8 = 3;
const TBZ_TBNZ: u8 = 4;
const B_COND: u8 = 5;
const BRANCH_REG: u8 = 6;
const ADR: u8 = 7;
const LDR_LITERAL: u8 = 8;
const ADD_SUB_IMM: u8 = 9;
const LOGICAL_IMM: u8 = 10;
const MOVE_WIDE: u8 = 11;
const BITFIELD_IMM: u8 = 12;
const EXTR: u8 = 13;
const LDST_PAIR: u8 = 14;
const LDST_UIMM: u8 = 15;
const LDST_IMM9: u8 = 16;
const LDST_REGOFF: u8 = 17;
const LOGICAL_SHIFTED: u8 = 18;
const ADD_SUB_SHIFTED: u8 = 19;
const ADD_SUB_EXTENDED: u8 = 20;
const ADC_SBC: u8 = 21;
const COND_COMPARE: u8 = 22;
const COND_SELECT: u8 = 23;
const DP_2SRC: u8 = 24;
const DP_1SRC: u8 = 25;
const DP_3SRC: u8 = 26;

/// The leaf `execute` would reach for `insn`, following the same tests in the same order.
pub(crate) fn leaf_of(insn: u32) -> u8 {
    match (insn >> 25) & 0xf {
        0b1000 | 0b1001 => match (insn >> 23) & 7 {
            0b000 | 0b001 => ADR,
            0b010 => ADD_SUB_IMM,
            0b100 => LOGICAL_IMM,
            0b101 => MOVE_WIDE,
            0b110 => BITFIELD_IMM,
            _ => EXTR,
        },
        0b1010 | 0b1011 => {
            if insn & 0x7c00_0000 == 0x1400_0000 {
                B_BL
            } else if insn & 0x7e00_0000 == 0x3400_0000 {
                CBZ_CBNZ
            } else if insn & 0x7e00_0000 == 0x3600_0000 {
                TBZ_TBNZ
            } else if insn & 0xff00_0010 == 0x5400_0000 {
                B_COND
            } else if insn & 0xfe00_0000 == 0xd600_0000 {
                BRANCH_REG
            } else {
                // Exception generation, system instructions, unallocated
                GENERIC
            }
        },
        0b0100 | 0b0110 | 0b1100 | 0b1110 => {
            if insn & (1 << 26) != 0 || insn & 0x3f00_0000 == 0x0800_0000 {
                // SIMD&FP loads and stores, exclusives
                GENERIC
            } else if insn & 0x3b00_0000 == 0x1800_0000 {
                LDR_LITERAL
            } else if insn & 0x3800_0000 == 0x2800_0000 {
                LDST_PAIR
            } else if insn & 0x3800_0000 == 0x3800_0000 {
                if insn & (1 << 24) != 0 {
                    LDST_UIMM
                } else if insn & (1 << 21) == 0 {
                    LDST_IMM9
                } else if (insn >> 10) & 3 == 2 {
                    LDST_REGOFF
                } else {
                    GENERIC
                }
            } else {
                GENERIC
            }
        },
        0b0101 | 0b1101 => {
            if insn & 0x1f00_0000 == 0x0a00_0000 {
                LOGICAL_SHIFTED
            } else if insn & 0x1f20_0000 == 0x0b00_0000 {
                ADD_SUB_SHIFTED
            } else if insn & 0x1f20_0000 == 0x0b20_0000 {
                ADD_SUB_EXTENDED
            } else if insn & 0x1fe0_0000 == 0x1a00_0000 {
                ADC_SBC
            } else if insn & 0x1fe0_0000 == 0x1a40_0000 {
                COND_COMPARE
            } else if insn & 0x1fe0_0000 == 0x1a80_0000 {
                COND_SELECT
            } else if insn & 0x5fe0_0000 == 0x1ac0_0000 {
                DP_2SRC
            } else if insn & 0x5fe0_0000 == 0x5ac0_0000 {
                DP_1SRC
            } else if insn & 0x1f00_0000 == 0x1b00_0000 {
                DP_3SRC
            } else {
                GENERIC
            }
        },
        _ => GENERIC,
    }
}

/// A decoded entry: the leaf above the instruction word. Never 0.
#[inline(always)]
pub(crate) fn entry(insn: u32) -> u64 {
    (leaf_of(insn) as u64) << 32 | insn as u64
}

const STORE: [[K; 4]; 4] = [
    [K::St1Off, K::St2Off, K::St4Off, K::St8Off],
    [K::St1Pre, K::St2Pre, K::St4Pre, K::St8Pre],
    [K::St1Post, K::St2Post, K::St4Post, K::St8Post],
    [K::St1Reg, K::St2Reg, K::St4Reg, K::St8Reg],
];
const LOAD: [[K; 4]; 4] = [
    [K::Ld1Off, K::Ld2Off, K::Ld4Off, K::Ld8Off],
    [K::Ld1Pre, K::Ld2Pre, K::Ld4Pre, K::Ld8Pre],
    [K::Ld1Post, K::Ld2Post, K::Ld4Post, K::Ld8Post],
    [K::Ld1Reg, K::Ld2Reg, K::Ld4Reg, K::Ld8Reg],
];
/// Loads that extend the byte, halfword or word to 64 bits, to 32 bits.
const LOAD_SX64: [K; 3] = [K::Ls1Off, K::Ls2Off, K::Ls4Off];
const LOAD_SX32: [K; 2] = [K::Lw1Off, K::Lw2Off];
/// Pairs: [offset, pre-indexed, post-indexed][word, doubleword].
const STORE_PAIR: [[K; 2]; 3] = [
    [K::Stp4Off, K::Stp8Off],
    [K::Stp4Pre, K::Stp8Pre],
    [K::Stp4Post, K::Stp8Post],
];
const LOAD_PAIR: [[K; 2]; 3] = [
    [K::Ldp4Off, K::Ldp8Off],
    [K::Ldp4Pre, K::Ldp8Pre],
    [K::Ldp4Post, K::Ldp8Post],
];

#[inline(always)]
fn mk(k: K, a: u32, b: u32, c: u32, x: u32, imm: u64) -> Op {
    Op {
        k,
        a: a as u8,
        b: b as u8,
        c: c as u8,
        cnt: 0,
        x: x as u16,
        imm,
    }
}

/// The slot that a write to register `r` goes to: 31 is the XZR of the instructions that have no SP.
#[inline(always)]
fn w(r: u32) -> u32 {
    if r == 31 {
        32
    } else {
        r
    }
}

/// `x` and `imm` of a direct branch at index `idx` whose target is `off` bytes from it.
#[inline(always)]
fn target(idx: usize, off: i64) -> (u32, u64) {
    let t = idx as i64 * 4 + off;
    let x = if (0..0x1000).contains(&t) { (t >> 2) as u32 } else { OUT as u32 };
    (x, t as u64)
}

/// `imm` of a memory op: the instruction word and the offset (a signed 32-bit).
#[inline(always)]
fn mem_imm(insn: u32, off: i64) -> u64 {
    (insn as u64) << 32 | off as i32 as u32 as u64
}

/// Whether `insn` is a system instruction that only reads or masks what the loop does not hold: MRS (but not
/// of the control register of a timer, which updates the timers) and MSR (immediate) to DAIFSet (masking
/// never makes an interrupt takeable). A write of another register can change the translation, the interrupts
/// that are taken or the stack pointer in use, which the loop has to see: those leave it.
fn pure_system(insn: u32) -> bool {
    use crate::sysreg::{key_of, CNTP_CTL_EL0, CNTV_CTL_EL0};
    insn & 0xfff0_0000 == 0xd530_0000 && !matches!(key_of(insn), CNTP_CTL_EL0 | CNTV_CTL_EL0)
        || insn & 0xffff_f0ff == 0xd503_40df
}

/// The op that runs `insn` as `exec_slow` does: in the loop for a data-processing instruction (it does not
/// touch the pc, the code or the system) and the system instructions that only read, out of it for the others.
fn generic(insn: u32) -> Op {
    let k = match (insn >> 25) & 0xf {
        0b1000 | 0b1001 | 0b0101 | 0b1101 | 0b0111 | 0b1111 => K::GenPure,
        _ if pure_system(insn) => K::GenPure,
        _ => K::Gen,
    };
    mk(k, 0, 0, 0, 0, entry(insn))
}

/// The op of the instruction word `insn` at index `idx` of its page.
pub(crate) fn decode(insn: u32, idx: usize) -> Op {
    let sf = bit(insn, 31);
    let rd = insn & 31;
    let rn = field(insn, 5, 5);
    let rm = field(insn, 16, 5);
    // Not as written, or an unallocated form: `exec_slow` runs it and raises what is to be raised.
    let gen = generic(insn);
    match leaf_of(insn) {
        ADR => {
            let imm = sext(
                ((field(insn, 5, 19) as u64) << 2) | field(insn, 29, 2) as u64,
                21,
            );
            // ADRP counts from the page of the pc, ADR from the pc: both from the page, here.
            let imm =
                if bit(insn, 31) { (imm << 12) as u64 } else { (idx as i64 * 4 + imm) as u64 };
            mk(K::Adr, w(rd), 0, 0, 0, imm)
        },
        ADD_SUB_IMM => {
            let sub = bit(insn, 30);
            let set = bit(insn, 29);
            let mut imm = field(insn, 10, 12) as u64;
            if bit(insn, 22) {
                imm <<= 12;
            }
            if rn == 31 || (!set && rd == 31) {
                // SP: a source (also of the forms that set the flags), or the destination of those that do not.
                if sf && !set {
                    return mk(
                        K::AddSp64,
                        rd,
                        rn,
                        0,
                        0,
                        if sub { imm.wrapping_neg() } else { imm },
                    );
                }
                return gen;
            }
            match (set, sub, sf) {
                (false, false, true) => mk(K::AddI64, rd, rn, 0, 0, imm),
                (false, false, false) => mk(K::AddI32, rd, rn, 0, 0, imm),
                (false, true, true) => mk(K::AddI64, rd, rn, 0, 0, imm.wrapping_neg()),
                (false, true, false) => mk(K::AddI32, rd, rn, 0, 0, imm.wrapping_neg()),
                (true, false, true) => mk(K::AddsI64, w(rd), rn, 0, 0, imm),
                (true, false, false) => mk(K::AddsI32, w(rd), rn, 0, 0, imm),
                (true, true, true) => mk(K::SubsI64, w(rd), rn, 0, 0, imm),
                (true, true, false) => mk(K::SubsI32, w(rd), rn, 0, 0, imm),
            }
        },
        LOGICAL_IMM => {
            let opc = field(insn, 29, 2);
            let nbit = field(insn, 22, 1);
            if !sf && nbit == 1 {
                return gen;
            }
            let Some(imm) = decode_bit_masks(nbit, field(insn, 10, 6), field(insn, 16, 6), sf)
            else {
                return gen;
            };
            if opc != 3 && rd == 31 {
                // SP.
                return gen;
            }
            let k = match (opc, sf) {
                (0, true) => K::AndI64,
                (1, true) => K::OrrI64,
                (2, true) => K::EorI64,
                (_, true) => K::AndsI64,
                (0, false) => K::AndI32,
                (1, false) => K::OrrI32,
                (2, false) => K::EorI32,
                (_, false) => K::AndsI32,
            };
            mk(k, w(rd), rn, 0, 0, imm)
        },
        MOVE_WIDE => {
            let opc = field(insn, 29, 2);
            let hw = field(insn, 21, 2);
            if (!sf && hw > 1) || opc == 1 {
                return gen;
            }
            let shift = hw * 16;
            let imm16 = field(insn, 5, 16);
            let imm = (imm16 as u64) << shift;
            match opc {
                0 => mk(
                    K::MovI,
                    w(rd),
                    0,
                    0,
                    0,
                    if sf { !imm } else { !imm as u32 as u64 },
                ),
                2 => mk(K::MovI, w(rd), 0, 0, 0, imm),
                _ if rd == 31 => mk(K::Nop, 0, 0, 0, 0, 0),
                _ => mk(
                    if sf { K::Movk64 } else { K::Movk32 },
                    rd,
                    shift,
                    0,
                    imm16,
                    0,
                ),
            }
        },
        BITFIELD_IMM => {
            let opc = field(insn, 29, 2);
            let immr = field(insn, 16, 6);
            let imms = field(insn, 10, 6);
            if bit(insn, 22) != sf || opc == 3 || (!sf && (immr >= 32 || imms >= 32)) {
                return gen;
            }
            let size = if sf { 64 } else { 32 };
            match opc {
                0 => mk(
                    if sf { K::Sbfx64 } else { K::Sbfx32 },
                    w(rd),
                    rn,
                    immr,
                    imms,
                    0,
                ),
                1 if rd == 31 => mk(K::Nop, 0, 0, 0, 0, 0),
                1 => mk(K::Bfm, rd, rn, immr, imms | (sf as u32) << 8, 0),
                _ if imms >= immr => mk(
                    K::Ubfx,
                    w(rd),
                    rn,
                    immr,
                    0,
                    crate::bits::ones(imms - immr + 1),
                ),
                _ => mk(
                    K::Ubfiz,
                    w(rd),
                    rn,
                    size - immr,
                    0,
                    crate::bits::ones(imms + 1),
                ),
            }
        },
        EXTR => {
            let lsb = field(insn, 10, 6);
            if field(insn, 29, 2) != 0 || bit(insn, 22) != sf || bit(insn, 21) || (!sf && lsb >= 32)
            {
                return gen;
            }
            mk(
                if sf { K::Extr64 } else { K::Extr32 },
                w(rd),
                rn,
                rm,
                lsb,
                0,
            )
        },
        B_BL => {
            let off = sext((insn & 0x3ff_ffff) as u64, 26) << 2;
            let (x, imm) = target(idx, off);
            mk(if bit(insn, 31) { K::Bl } else { K::B }, 0, 0, 0, x, imm)
        },
        CBZ_CBNZ => {
            let off = sext(field(insn, 5, 19) as u64, 19) << 2;
            let (x, imm) = target(idx, off);
            let k = match (sf, bit(insn, 24)) {
                (true, false) => K::Cbz64,
                (true, true) => K::Cbnz64,
                (false, false) => K::Cbz32,
                (false, true) => K::Cbnz32,
            };
            mk(k, 0, rd, 0, x, imm)
        },
        TBZ_TBNZ => {
            let off = sext(field(insn, 5, 14) as u64, 14) << 2;
            let (x, imm) = target(idx, off);
            let bitno = (field(insn, 31, 1) << 5) | field(insn, 19, 5);
            mk(
                if bit(insn, 24) { K::Tbnz } else { K::Tbz },
                bitno,
                rd,
                0,
                x,
                imm,
            )
        },
        B_COND => {
            let off = sext(field(insn, 5, 19) as u64, 19) << 2;
            let (x, imm) = target(idx, off);
            mk(K::Bcond, insn & 0xf, 0, 0, x, imm)
        },
        BRANCH_REG => {
            let opc = field(insn, 21, 4);
            if opc > 2 || rm != 31 || field(insn, 10, 6) != 0 || rd != 0 {
                // ERET, the unallocated.
                return gen;
            }
            mk(if opc == 1 { K::Blr } else { K::Br }, 0, rn, 0, 0, 0)
        },
        LDR_LITERAL => {
            let off = sext(field(insn, 5, 19) as u64, 19) << 2;
            let imm = mem_imm(insn, idx as i64 * 4 + off);
            match field(insn, 30, 2) {
                0 => mk(K::LdrLit32, w(rd), 0, 0, 0, imm),
                1 => mk(K::LdrLit64, w(rd), 0, 0, 0, imm),
                2 => mk(K::LdrLitSw, w(rd), 0, 0, 0, imm),
                // PRFM
                _ => mk(K::Nop, 0, 0, 0, 0, 0),
            }
        },
        LDST_PAIR => {
            let opc = field(insn, 30, 2);
            let mode = field(insn, 23, 2);
            let load = bit(insn, 22);
            let rt2 = field(insn, 10, 5);
            let (size, size_ix) = match opc {
                0 => (4, 0),
                2 => (8, 1),
                // LDPSW, unallocated
                _ => return gen,
            };
            // The tables: offset (and no-allocate), pre-indexed, post-indexed.
            let m = match mode {
                3 => 1,
                1 => 2,
                _ => 0,
            };
            let imm = mem_imm(insn, sext(field(insn, 15, 7) as u64, 7) * size);
            if load {
                mk(LOAD_PAIR[m][size_ix], w(rd), rn, w(rt2), 0, imm)
            } else {
                mk(STORE_PAIR[m][size_ix], rd, rn, rt2, 0, imm)
            }
        },
        LDST_UIMM => {
            let size = field(insn, 30, 2) as usize;
            let imm = mem_imm(insn, (field(insn, 10, 12) as i64) << size);
            match field(insn, 22, 2) {
                0 => mk(STORE[0][size], rd, rn, 0, 0, imm),
                1 => mk(LOAD[0][size], w(rd), rn, 0, 0, imm),
                2 if size != 3 => mk(LOAD_SX64[size], w(rd), rn, 0, 0, imm),
                3 if size < 2 => mk(LOAD_SX32[size], w(rd), rn, 0, 0, imm),
                _ => gen,
            }
        },
        LDST_IMM9 => {
            let size = field(insn, 30, 2) as usize;
            let imm = mem_imm(insn, sext(field(insn, 12, 9) as u64, 9));
            // The tables: offset (unscaled), pre-indexed, post-indexed; the unprivileged ones are left to `exec_slow`.
            let m = match field(insn, 10, 2) {
                0 => 0,
                3 => 1,
                1 => 2,
                _ => return gen,
            };
            match field(insn, 22, 2) {
                0 => mk(STORE[m][size], rd, rn, 0, 0, imm),
                1 => mk(LOAD[m][size], w(rd), rn, 0, 0, imm),
                2 if m == 0 && size != 3 => mk(LOAD_SX64[size], w(rd), rn, 0, 0, imm),
                3 if m == 0 && size < 2 => mk(LOAD_SX32[size], w(rd), rn, 0, 0, imm),
                _ => gen,
            }
        },
        LDST_REGOFF => {
            let size = field(insn, 30, 2) as usize;
            let option = field(insn, 13, 3);
            if option & 2 == 0 {
                return gen;
            }
            let shift = if bit(insn, 12) { size as i64 } else { 0 };
            let imm = mem_imm(insn, shift);
            match field(insn, 22, 2) {
                0 => mk(STORE[3][size], rd, rn, rm, option, imm),
                1 => mk(LOAD[3][size], w(rd), rn, rm, option, imm),
                _ => gen,
            }
        },
        LOGICAL_SHIFTED => {
            let opc = field(insn, 29, 2);
            let imm6 = field(insn, 10, 6);
            let inv = bit(insn, 21);
            if !sf && imm6 >= 32 {
                return gen;
            }
            if imm6 != 0 || inv {
                // The shift kind, the amount and whether the second operand is inverted, in `x`.
                let k = match (opc, sf) {
                    (0, true) => K::AndRs64,
                    (1, true) => K::OrrRs64,
                    (2, true) => K::EorRs64,
                    (_, true) => K::AndsRs64,
                    (0, false) => K::AndRs32,
                    (1, false) => K::OrrRs32,
                    (2, false) => K::EorRs32,
                    (_, false) => K::AndsRs32,
                };
                return mk(
                    k,
                    w(rd),
                    rn,
                    rm,
                    field(insn, 22, 2) | imm6 << 2 | (inv as u32) << 8,
                    0,
                );
            }
            if opc == 1 && rn == 31 {
                return mk(if sf { K::MovR64 } else { K::MovR32 }, w(rd), 0, rm, 0, 0);
            }
            let k = match (opc, sf) {
                (0, true) => K::AndR64,
                (1, true) => K::OrrR64,
                (2, true) => K::EorR64,
                (_, true) => K::AndsR64,
                (0, false) => K::AndR32,
                (1, false) => K::OrrR32,
                (2, false) => K::EorR32,
                (_, false) => K::AndsR32,
            };
            mk(k, w(rd), rn, rm, 0, 0)
        },
        ADD_SUB_EXTENDED => {
            let imm3 = field(insn, 10, 3);
            if !sf || imm3 > 4 || field(insn, 22, 2) != 0 {
                return gen;
            }
            // The extension and the shift, in `x`. SP is a source, and the destination of the forms that do not
            // set the flags: `a` and `b` are 31 for it.
            let x = field(insn, 13, 3) | imm3 << 3;
            match (bit(insn, 29), bit(insn, 30)) {
                (false, false) => mk(K::AddExt64, rd, rn, rm, x, 0),
                (false, true) => mk(K::SubExt64, rd, rn, rm, x, 0),
                (true, false) => mk(K::AddsExt64, w(rd), rn, rm, x, 0),
                (true, true) => mk(K::SubsExt64, w(rd), rn, rm, x, 0),
            }
        },
        ADC_SBC => {
            if field(insn, 10, 6) != 0 {
                return gen;
            }
            // Subtract, set the flags and the size, in `x`.
            let x = bit(insn, 30) as u32 | (bit(insn, 29) as u32) << 1 | (sf as u32) << 2;
            mk(K::AdcSbc, w(rd), rn, rm, x, 0)
        },
        COND_COMPARE => {
            if !bit(insn, 29) || bit(insn, 10) || bit(insn, 4) {
                return gen;
            }
            // `a` is the NZCV the flags take if the condition (`x`) does not hold; `c` is the second register
            // or the immediate.
            let k = match (bit(insn, 11), bit(insn, 30), sf) {
                (false, true, true) => K::Ccmp64,
                (false, false, true) => K::Ccmn64,
                (false, true, false) => K::Ccmp32,
                (false, false, false) => K::Ccmn32,
                (true, true, true) => K::CcmpI64,
                (true, false, true) => K::CcmnI64,
                (true, true, false) => K::CcmpI32,
                (true, false, false) => K::CcmnI32,
            };
            mk(k, insn & 0xf, rn, rm, field(insn, 12, 4), 0)
        },
        ADD_SUB_SHIFTED => {
            let kind = field(insn, 22, 2);
            let imm6 = field(insn, 10, 6);
            if kind == 3 || (!sf && imm6 >= 32) {
                return gen;
            }
            let sub = bit(insn, 30);
            let set = bit(insn, 29);
            let plain = imm6 == 0;
            let k = match (set, sub, sf, plain) {
                (false, false, true, true) => K::AddR64,
                (false, false, false, true) => K::AddR32,
                (false, true, true, true) => K::SubR64,
                (false, true, false, true) => K::SubR32,
                (true, false, true, true) => K::AddsR64,
                (true, false, false, true) => K::AddsR32,
                (true, true, true, true) => K::SubsR64,
                (true, true, false, true) => K::SubsR32,
                (false, false, true, false) => K::AddRs64,
                (false, false, false, false) => K::AddRs32,
                (false, true, true, false) => K::SubRs64,
                (false, true, false, false) => K::SubRs32,
                (true, false, true, false) => K::AddsRs64,
                (true, false, false, false) => K::AddsRs32,
                (true, true, true, false) => K::SubsRs64,
                (true, true, false, false) => K::SubsRs32,
            };
            mk(k, w(rd), rn, rm, kind | imm6 << 2, 0)
        },
        COND_SELECT => {
            if bit(insn, 29) || bit(insn, 11) {
                return gen;
            }
            let k = match (sf, bit(insn, 30), bit(insn, 10)) {
                (true, false, false) => K::Csel64,
                (true, false, true) => K::Csinc64,
                (true, true, false) => K::Csinv64,
                (true, true, true) => K::Csneg64,
                (false, false, false) => K::Csel32,
                (false, false, true) => K::Csinc32,
                (false, true, false) => K::Csinv32,
                (false, true, true) => K::Csneg32,
            };
            mk(k, w(rd), rn, rm, field(insn, 12, 4), 0)
        },
        DP_2SRC => {
            let opcode = field(insn, 10, 6);
            if bit(insn, 29) {
                return gen;
            }
            match opcode {
                0b001000..=0b001011 => mk(
                    if sf { K::Shiftv64 } else { K::Shiftv32 },
                    w(rd),
                    rn,
                    rm,
                    opcode & 3,
                    0,
                ),
                // UDIV, SDIV (the CRC32 ones are rare)
                0b000010 | 0b000011 => mk(K::Div, w(rd), rn, rm, opcode & 1 | (sf as u32) << 8, 0),
                _ => gen,
            }
        },
        DP_1SRC => {
            // RBIT, REV16, REV32, REV, CLZ, CLS (the opcode, and the size in the bit above it, in `x`)
            let opcode = field(insn, 10, 6);
            if bit(insn, 29) || rm != 0 || opcode > 5 || (opcode == 3 && !sf) {
                return gen;
            }
            mk(K::Dp1, w(rd), rn, 0, opcode | (sf as u32) << 8, 0)
        },
        DP_3SRC => {
            if field(insn, 29, 2) != 0 {
                return gen;
            }
            let ra = field(insn, 10, 5);
            let k = match (field(insn, 21, 3), sf, bit(insn, 15)) {
                (0, true, false) => K::Madd64,
                (0, true, true) => K::Msub64,
                (0, false, false) => K::Madd32,
                (0, false, true) => K::Msub32,
                (1, true, false) => K::Smaddl,
                (1, true, true) => K::Smsubl,
                (5, true, false) => K::Umaddl,
                (5, true, true) => K::Umsubl,
                (2, true, false) => K::Smulh,
                (6, true, false) => K::Umulh,
                _ => return gen,
            };
            mk(k, w(rd), rn, rm, ra, 0)
        },
        // The hints that do nothing here (the ones with a CRm other than 0 are BTI, PAC and the like; NOP is
        // 0, the others of CRm 0 wait or signal), and DMB (the barrier of a CPU among others).
        _ if insn & 0xffff_f01f == 0xd503_201f && insn & 0xf00 != 0 => mk(K::Nop, 0, 0, 0, 0, 0),
        _ if insn == 0xd503_201f => mk(K::Nop, 0, 0, 0, 0, 0),
        _ if insn & 0xffff_f0ff == 0xd503_30bf => mk(K::Dmb, 0, 0, 0, 0, 0),
        // SIMD&FP loads and stores of registers
        _ if (insn >> 25) & 5 == 4 && insn & 0x0400_0000 != 0 => vldst(insn, idx).unwrap_or(gen),
        _ => gen,
    }
}

/// The op of an SIMD&FP load or store of registers (not the structure ones): `a` is the register, `b` the base,
/// `x` the log2 of the size in bytes (0..=4, the pair kinds hold the way the base is written back in the bits
/// above that; the register-offset kinds the extension above it, at bit 4), `imm` as the integer loads have it.
fn vldst(insn: u32, idx: usize) -> Option<Op> {
    let rt = insn & 31;
    let rn = field(insn, 5, 5);
    if insn & 0x3b00_0000 == 0x1800_0000 {
        // LDR (literal)
        let scale = match field(insn, 30, 2) {
            0 => 2,
            1 => 3,
            2 => 4,
            _ => return None,
        };
        let off = sext(field(insn, 5, 19) as u64, 19) << 2;
        return Some(mk(
            K::VLdLit,
            rt,
            0,
            0,
            scale,
            mem_imm(insn, idx as i64 * 4 + off),
        ));
    }
    if insn & 0x3800_0000 == 0x2800_0000 {
        // LDP, STP (and the non-temporal ones): the way the base is written back is 0 for none, 1 for after, 2
        // for before.
        let scale = match field(insn, 30, 2) {
            0 => 2,
            1 => 3,
            2 => 4,
            _ => return None,
        };
        let wb = match field(insn, 23, 2) {
            1 => 1,
            3 => 2,
            _ => 0,
        };
        let imm = mem_imm(insn, sext(field(insn, 15, 7) as u64, 7) << scale);
        let k = if bit(insn, 22) { K::VLdp } else { K::VStp };
        return Some(mk(k, rt, rn, field(insn, 10, 5), scale | wb << 8, imm));
    }
    if insn & 0x3800_0000 == 0x3800_0000 {
        let size = field(insn, 30, 2);
        let opc = field(insn, 22, 2);
        let scale = if opc & 2 != 0 {
            if size != 0 {
                return None;
            }
            4
        } else {
            size
        };
        let load = opc & 1 != 0;
        let pick = |m: usize| match (load, m) {
            (true, 0) => K::VLdOff,
            (true, 1) => K::VLdPre,
            (true, 2) => K::VLdPost,
            (true, _) => K::VLdReg,
            (false, 0) => K::VStOff,
            (false, 1) => K::VStPre,
            (false, 2) => K::VStPost,
            (false, _) => K::VStReg,
        };
        if bit(insn, 24) {
            let imm = mem_imm(insn, (field(insn, 10, 12) as i64) << scale);
            return Some(mk(pick(0), rt, rn, 0, scale, imm));
        }
        if !bit(insn, 21) {
            let imm = mem_imm(insn, sext(field(insn, 12, 9) as u64, 9));
            let m = match field(insn, 10, 2) {
                0 => 0,
                3 => 1,
                1 => 2,
                // The unprivileged ones do not exist for SIMD&FP.
                _ => return None,
            };
            return Some(mk(pick(m), rt, rn, 0, scale, imm));
        }
        if field(insn, 10, 2) == 2 {
            let option = field(insn, 13, 3);
            if option & 2 == 0 {
                return None;
            }
            let shift = if bit(insn, 12) { scale as i64 } else { 0 };
            return Some(mk(
                pick(3),
                rt,
                rn,
                field(insn, 16, 5),
                scale | option << 4,
                mem_imm(insn, shift),
            ));
        }
    }
    None
}

/// The decoded pages of one CPU.
pub struct Cache {
    /// Physical page number -> its slot plus one, or 0 for no slot (`NONE`). Zero is the empty value so that the
    /// table is allocated zeroed and no page of it is touched until a page of that range is decoded.
    page_slot: Vec<u32>,
    /// Slot -> its page's ops (`SLOT_OPS` of them), allocated once and kept until the cache is dropped: a
    /// dropped page's slot is zeroed when it is given to another page, not before, so a pointer to the ops
    /// that the interpreter holds stays valid.
    slots: Vec<*mut Op>,
    slot_page: Vec<u32>,
    free: Vec<u32>,
    /// Whether the slot was current since the clock hand last passed it.
    referenced: Vec<bool>,
    hand: usize,
    /// The page `step_page` executes from: its physical base, its slot and its ops. Reset when any slot is
    /// dropped, so `step_page` never runs the ops of a dropped page.
    pub(crate) cur_base: usize,
    pub(crate) cur_slot: u32,
    pub(crate) cur_ptr: *mut Op,
    /// Slots at most (`MAX_SLOTS`).
    pub(crate) max_slots: usize,
    /// Statistics: pages decoded, pages dropped, pages evicted for want of a slot, full flushes, blocks of ops
    /// decoded and the instructions in them.
    pub decoded: u64,
    pub dropped: u64,
    pub evictions: u64,
    pub flushes: u64,
    pub blocks: u64,
    pub insns: u64,
}

impl Cache {
    pub fn new(memory_size: usize) -> Cache {
        Cache {
            page_slot: vec![0; memory_size.div_ceil(4096)],
            slots: Vec::new(),
            slot_page: Vec::new(),
            free: Vec::new(),
            referenced: Vec::new(),
            hand: 0,
            cur_base: usize::MAX,
            cur_slot: NONE,
            cur_ptr: std::ptr::null_mut(),
            max_slots: MAX_SLOTS,
            decoded: 0,
            dropped: 0,
            evictions: 0,
            flushes: 0,
            blocks: 0,
            insns: 0,
        }
    }

    /// Keep at most `bytes` of decoded pages (2 pages at the least). Slots already allocated past it stay until
    /// the cache is dropped (the interpreter may hold a pointer to them); none is allocated past it from now on.
    pub fn set_limit(&mut self, bytes: usize) {
        self.max_slots = (bytes / (SLOT_OPS * std::mem::size_of::<Op>())).max(2);
    }

    /// The slots allocated so far and the bytes they hold, with the cache's tables.
    pub fn bytes(&self) -> (usize, usize) {
        let slots = self.slots.len();
        let tables = self.page_slot.capacity() * 4
            + self.slot_page.capacity() * 4
            + self.free.capacity() * 4
            + self.referenced.capacity()
            + self.slots.capacity() * std::mem::size_of::<*mut Op>();
        (slots, slots * SLOT_OPS * std::mem::size_of::<Op>() + tables)
    }

    #[inline(always)]
    pub(crate) fn slot(&self, page: usize) -> u32 {
        self.page_slot[page].wrapping_sub(1)
    }

    /// Record `slot` (or `NONE`) as the slot of physical page `page`: the table holds the slot plus one, so
    /// `NONE` is stored as 0.
    #[inline(always)]
    fn set_slot(&mut self, page: usize, slot: u32) {
        self.page_slot[page] = slot.wrapping_add(1);
    }

    /// Make `slot`, the slot of the page at physical address `base`, the current page.
    pub(crate) fn set_current(&mut self, base: usize, slot: u32) {
        self.cur_base = base;
        self.cur_slot = slot;
        self.referenced[slot as usize] = true;
        self.cur_ptr = self.slots[slot as usize];
    }

    /// The ops of the current page, if `base` is that page, else null.
    #[inline(always)]
    pub(crate) fn current(&self, base: usize) -> *mut Op {
        if base == self.cur_base {
            self.cur_ptr
        } else {
            std::ptr::null_mut()
        }
    }

    /// The ops of the page at physical base `base`, made the current page, or null if it has no slot (the run
    /// of a loop that goes on in another page, see `Cpu::enter_page`).
    #[inline(always)]
    pub(crate) fn enter(&mut self, base: usize) -> *mut Op {
        if base == self.cur_base {
            return self.cur_ptr;
        }
        let slot = self.slot(base >> 12);
        if slot == NONE {
            return std::ptr::null_mut();
        }
        self.set_current(base, slot);
        self.cur_ptr
    }

    /// Count one execution of op `i` of the page at physical base `base` in the op itself (saturating at
    /// `COUNT_MAX`) and return the count, or 0 when the op is not decoded yet (or its page has no slot). Makes
    /// that page the current one.
    #[inline(always)]
    #[allow(unsafe_code)]
    pub(crate) fn bump(&mut self, base: usize, i: usize) -> u32 {
        debug_assert!(i < PAGE_INSNS);
        if base != self.cur_base {
            let slot = self.slot(base >> 12);
            if slot == NONE {
                return 0;
            }
            self.set_current(base, slot);
        }
        // SAFETY: `cur_ptr` points at the ops of the page at `cur_base`, a live slot, and `i` is an
        // instruction index within a page.
        unsafe {
            let p = self.cur_ptr.add(i);
            if (*p).k == K::Miss {
                return 0;
            }
            let c = (*p).cnt;
            if c as u32 != COUNT_MAX {
                (*p).cnt = c + 1;
                c as u32 + 1
            } else {
                COUNT_MAX
            }
        }
    }

    /// Set the count of op `i` of the page at physical base `base` to `v` (at most `COUNT_MAX`), if that is the
    /// current page, as it is after `bump` unless something since dropped it.
    #[inline(always)]
    pub(crate) fn set_count(&mut self, base: usize, i: usize, v: u32) {
        debug_assert!(i < PAGE_INSNS && v <= COUNT_MAX);
        if base == self.cur_base {
            // SAFETY: `cur_ptr` points at the ops of the page at `cur_base`, a live slot, and `i` is an
            // instruction index within a page.
            unsafe { (*self.cur_ptr.add(i)).cnt = v as u16 };
        }
    }

    /// A slot for physical page `page`, with nothing decoded yet. Reuses the slot of a dropped page, or
    /// allocates one, or, past `max_slots`, takes the one of the page used the longest ago (clock order).
    pub(crate) fn alloc(&mut self, page: usize) -> u32 {
        let slot = if let Some(s) = self.free.pop() {
            s
        } else if self.slots.len() < self.max_slots.max(2) {
            let layout = Layout::array::<Op>(SLOT_OPS).unwrap();
            // SAFETY: the layout is not empty. All zero is `K::Miss` all through, and the `End` after the
            // last instruction is set once and never written again.
            let p = unsafe { alloc_zeroed(layout) } as *mut Op;
            if p.is_null() {
                handle_alloc_error(layout);
            }
            // SAFETY: `p` is not null (checked above) and the slot has `SLOT_OPS` (`PAGE_INSNS + 1`) ops.
            unsafe { (*p.add(PAGE_INSNS)).k = K::End };
            self.slots.push(p);
            self.slot_page.push(NONE);
            self.referenced.push(false);
            let s = (self.slots.len() - 1) as u32;
            self.set_slot(page, s);
            self.slot_page[s as usize] = page as u32;
            self.decoded += 1;
            return s;
        } else {
            self.evict()
        };
        // SAFETY: the slot has `SLOT_OPS` ops, `PAGE_INSNS` of which are the ones to clear.
        unsafe { std::ptr::write_bytes(self.slots[slot as usize], 0, PAGE_INSNS) };
        self.set_slot(page, slot);
        self.slot_page[slot as usize] = page as u32;
        self.decoded += 1;
        slot
    }

    /// The slot of the page that was current the longest ago, but not the current one, free of its page.
    fn evict(&mut self) -> u32 {
        let n = self.slots.len();
        loop {
            if self.hand >= n {
                self.hand = 0;
            }
            let s = self.hand;
            self.hand += 1;
            if s as u32 == self.cur_slot {
                continue;
            }
            if std::mem::take(&mut self.referenced[s]) {
                continue;
            }
            let page = self.slot_page[s];
            self.set_slot(page as usize, NONE);
            self.slot_page[s] = NONE;
            self.evictions += 1;
            return s as u32;
        }
    }

    /// Forget the ops at words `first..=last` of physical page `page` (written since, by a store that no
    /// compiled code covers): they are decoded again when they run. A compare that was fused with the branch after
    /// it holds that branch's operands, so when the first word is a branch, the op before it is forgotten too.
    #[allow(unsafe_code)]
    pub(crate) fn forget_words(&mut self, page: usize, first: usize, last: usize) {
        debug_assert!(first <= last && last < PAGE_INSNS);
        let Some(&stored) = self.page_slot.get(page) else {
            return;
        };
        let slot = stored.wrapping_sub(1);
        if slot == NONE {
            return;
        }
        let ops = self.slots[slot as usize];
        // SAFETY: `ops` points at the `SLOT_OPS` ops of the slot, and the indexes are in the page. All zero is
        // `K::Miss`.
        unsafe {
            if first > 0 && (*ops.add(first - 1)).k.is_fused() {
                std::ptr::write_bytes(ops.add(first - 1), 0, 1);
            }
            std::ptr::write_bytes(ops.add(first), 0, last - first + 1);
        }
    }

    /// Forget the decoded instructions of physical page `page`, if any.
    pub(crate) fn drop_page(&mut self, page: usize) {
        let Some(&stored) = self.page_slot.get(page) else {
            return;
        };
        let slot = stored.wrapping_sub(1);
        if slot == NONE {
            return;
        }
        self.set_slot(page, NONE);
        self.slot_page[slot as usize] = NONE;
        self.free.push(slot);
        self.dropped += 1;
        self.reset_current();
    }

    pub(crate) fn clear(&mut self) {
        for (slot, page) in self.slot_page.iter_mut().enumerate() {
            if *page != NONE {
                self.page_slot[*page as usize] = 0;
                *page = NONE;
                self.free.push(slot as u32);
            }
        }
        self.flushes += 1;
        self.reset_current();
    }

    fn reset_current(&mut self) {
        self.cur_base = usize::MAX;
        self.cur_slot = NONE;
        self.cur_ptr = std::ptr::null_mut();
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        let layout = Layout::array::<Op>(SLOT_OPS).unwrap();
        for &p in &self.slots {
            // SAFETY: allocated in `alloc` with this layout, and nothing points at it any more.
            unsafe { dealloc(p as *mut u8, layout) };
        }
    }
}

impl<H: Host> Cpu<H> {
    /// Execute a decoded entry: the leaf `execute` would reach, directly. `Cpu::pc` and `next_pc` are set.
    #[inline(never)]
    pub(crate) fn exec_slow(&mut self, e: u64) -> Res<()> {
        let insn = e as u32;
        match (e >> 32) as u8 {
            ADR => self.adr(insn),
            ADD_SUB_IMM => self.add_sub_imm(insn),
            LOGICAL_IMM => self.logical_imm(insn),
            MOVE_WIDE => self.move_wide(insn),
            BITFIELD_IMM => self.bitfield_imm(insn),
            EXTR => self.extr(insn),
            B_BL => self.b_bl(insn),
            CBZ_CBNZ => self.cbz_cbnz(insn),
            TBZ_TBNZ => self.tbz_tbnz(insn),
            B_COND => self.b_cond(insn),
            BRANCH_REG => self.branch_reg(insn),
            LDR_LITERAL => self.ldr_literal(insn),
            LDST_PAIR => self.ldst_pair(insn),
            LDST_UIMM => self.ldst_uimm(insn),
            LDST_IMM9 => self.ldst_imm9(insn),
            LDST_REGOFF => self.ldst_regoff(insn),
            LOGICAL_SHIFTED => self.logical_shifted(insn),
            ADD_SUB_SHIFTED => self.add_sub_shifted(insn),
            ADD_SUB_EXTENDED => self.add_sub_extended(insn),
            ADC_SBC => self.adc_sbc(insn),
            COND_COMPARE => self.cond_compare(insn),
            COND_SELECT => self.cond_select(insn),
            DP_2SRC => self.dp_2src_leaf(insn),
            DP_1SRC => self.dp_1src_leaf(insn),
            DP_3SRC => self.dp_3src_leaf(insn),
            _ => self.execute(insn),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::Cpu;

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

    const MOVZ_X0: u32 = 0xd280_0000; // movz x0, #imm16 (imm in bits 5..21)
    const ADD_X0_1: u32 = 0x9100_0400; // add x0, x0, #1
    const STR_W2_X3: u32 = 0xb900_0062; // str w2, [x3]

    fn cpu() -> Cpu<NoDevices> {
        Cpu::new(8 << 20, NoDevices)
    }

    fn put(c: &mut Cpu<NoDevices>, addr: usize, insn: u32) {
        c.mem.write(addr, &insn.to_le_bytes());
    }

    /// Runs the one instruction at `pc`.
    fn run_at(c: &mut Cpu<NoDevices>, pc: u64) {
        c.pc = pc;
        c.run(1);
    }

    #[test]
    fn the_leaf_of_an_instruction_is_the_one_execute_reaches() {
        assert_eq!(leaf_of(ADD_X0_1), ADD_SUB_IMM);
        assert_eq!(leaf_of(STR_W2_X3), LDST_UIMM);
        assert_eq!(leaf_of(0x5400_0001), B_COND); // b.ne
        assert_eq!(leaf_of(0xd65f_03c0), BRANCH_REG); // ret
        assert_eq!(leaf_of(0xd503_201f), GENERIC); // nop: a system instruction
        assert_eq!(leaf_of(0x4ea1_1c20), GENERIC); // mov v0.16b, v1.16b: SIMD
        assert_ne!(
            entry(0),
            0,
            "an entry is never mistaken for an undecoded one"
        );
    }

    #[test]
    fn a_store_over_decoded_code_is_executed_as_written() {
        let mut c = cpu();
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 1);
        // The guest rewrites the instruction it already ran.
        put(&mut c, 0x2000, STR_W2_X3);
        c.x[2] = (MOVZ_X0 | 2 << 5) as u64;
        c.x[3] = 0x1000;
        run_at(&mut c, 0x2000);
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 2);
    }

    #[test]
    fn a_device_writing_over_decoded_code_is_seen() {
        let mut c = cpu();
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        let io = crate::dev::Io::new(c.machine.clone(), || 0);
        assert!(io.write(0x1000, &(MOVZ_X0 | 3 << 5).to_le_bytes()));
        run_at(&mut c, 0x1000);
        assert_eq!(c.x[0], 3);
    }

    #[test]
    fn a_device_writing_the_same_decoded_page_again_is_seen_again() {
        let mut c = cpu();
        let io = crate::dev::Io::new(c.machine.clone(), || 0);
        put(&mut c, 0x1000, MOVZ_X0 | 1 << 5);
        run_at(&mut c, 0x1000);
        for n in 2..5u32 {
            // The page was decoded again since the last write: this write must drop it too.
            assert!(io.write(0x1000, &(MOVZ_X0 | n << 5).to_le_bytes()));
            run_at(&mut c, 0x1000);
            assert_eq!(c.x[0], n as u64);
        }
    }

    #[test]
    fn a_limit_in_bytes_caps_the_slots_a_cache_allocates() {
        let mut c = cpu();
        c.jit.pre.set_limit(4 * SLOT_OPS * std::mem::size_of::<Op>());
        assert_eq!(c.jit.pre.max_slots, 4);
        for p in 0..10 {
            put(&mut c, 0x1000 * (p + 1), ADD_X0_1);
            run_at(&mut c, 0x1000 * (p as u64 + 1));
        }
        assert_eq!(c.x[0], 10);
        assert_eq!(c.jit.pre.bytes().0, 4);
        // Never below two slots: the page running and the one it jumps to
        c.jit.pre.set_limit(0);
        assert_eq!(c.jit.pre.max_slots, 2);
    }

    #[test]
    fn more_code_pages_than_are_kept_still_run_correctly() {
        let mut c = cpu();
        let flushes = c.jit.pre.flushes;
        c.jit.pre.max_slots = 8;
        let pages = 8 + 10;
        for p in 0..pages {
            put(&mut c, 0x1000 * (p + 1), ADD_X0_1);
        }
        for round in 1..=2 {
            for p in 0..pages {
                run_at(&mut c, 0x1000 * (p as u64 + 1));
            }
            assert_eq!(c.x[0], (round * pages) as u64);
        }
        // The slots of the pages used the longest ago were given to the others, and nothing was cleared.
        assert!(c.jit.pre.evictions >= 10);
        assert_eq!(c.jit.pre.flushes, flushes);
    }

    const NOP: u32 = 0xd503_201f;
    const WFI: u32 = 0xd503_207f;
    const SVC_0: u32 = 0xd400_0001;
    const ADR_X1_16: u32 = 0x1000_0081; // adr x1, .+16
    const LDR_X2_LIT: u32 = 0x5800_0102; // ldr x2, .+32

    #[test]
    fn a_run_over_decoded_pages_counts_every_instruction_and_stops_at_the_budget() {
        let mut c = cpu();
        // 1024 instructions fill the page at 0x1000, and the code goes on in the next page.
        for i in 0..1124 {
            put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
        }
        // The second round runs from entries decoded in the first.
        for _ in 0..2 {
            c.pc = 0x1000;
            c.x[0] = 0;
            let before = c.insn_count;
            assert!(matches!(c.run(1100), crate::cpu::Exit::Budget));
            assert_eq!(c.x[0], 1100);
            assert_eq!(c.insn_count - before, 1100);
            assert_eq!(c.pc, 0x1000 + 4 * 1100);
        }
    }

    #[test]
    fn a_store_into_the_page_being_run_takes_effect_at_the_next_instruction() {
        let mut c = cpu();
        // str w2, [x3]; nop; movz x0, #1: the store rewrites the movz.
        put(&mut c, 0x1000, STR_W2_X3);
        put(&mut c, 0x1004, NOP);
        put(&mut c, 0x1008, MOVZ_X0 | 1 << 5);
        c.x[3] = 0x1008;
        for value in 1..4u32 {
            c.pc = 0x1000;
            c.x[2] = (MOVZ_X0 | value << 5) as u64;
            c.run(3);
            assert_eq!(c.x[0], value as u64);
        }
        assert_eq!(c.insn_count, 9);
    }

    #[test]
    fn instructions_that_read_the_pc_see_it_in_the_middle_of_a_run() {
        let mut c = cpu();
        put(&mut c, 0x1000, ADD_X0_1);
        put(&mut c, 0x1004, ADR_X1_16);
        put(&mut c, 0x1008, LDR_X2_LIT);
        c.mem.write(0x1028, &0x1234_5678_9abc_def0u64.to_le_bytes());
        for round in 1..=2u64 {
            c.pc = 0x1000;
            c.x[1] = 0;
            c.x[2] = 0;
            c.run(3);
            assert_eq!(c.x[0], round);
            assert_eq!(c.x[1], 0x1014);
            assert_eq!(c.x[2], 0x1234_5678_9abc_def0);
            assert_eq!(c.pc, 0x100c);
        }
    }

    #[test]
    fn an_exception_in_the_middle_of_a_run_returns_to_the_right_instruction() {
        let mut c = cpu();
        c.sys.vbar = 0x4000;
        put(&mut c, 0x1000, ADD_X0_1);
        put(&mut c, 0x1004, ADD_X0_1);
        put(&mut c, 0x1008, SVC_0);
        put(&mut c, 0x100c, ADD_X0_1);
        for round in 1..=2u64 {
            c.pc = 0x1000;
            c.x[0] = 0;
            c.run(3);
            assert_eq!(c.x[0], 2);
            assert_eq!(c.insn_count, 3 * round);
            assert_eq!(c.sys.elr, 0x100c);
            assert_eq!(c.sys.esr >> 26, 0x15);
            assert!((0x4000..0x4800).contains(&c.pc));
            // The guest returned from the handler, as far as this test goes.
            c.el = 1;
            c.spsel = true;
        }
    }

    #[test]
    fn waiting_for_an_interrupt_in_the_middle_of_a_run_stops_the_run() {
        let mut c = cpu();
        put(&mut c, 0x1000, ADD_X0_1);
        put(&mut c, 0x1004, WFI);
        put(&mut c, 0x1008, ADD_X0_1);
        for round in 1..=2u64 {
            c.pc = 0x1000;
            c.x[0] = 0;
            assert!(matches!(c.run(10), crate::cpu::Exit::Idle { .. }));
            assert_eq!(c.x[0], 1);
            assert_eq!(c.insn_count, 2 * round);
            assert_eq!(c.pc, 0x1008);
        }
    }
    const SUB_X0_1: u32 = 0xd100_0400; // sub x0, x0, #1
    const CBNZ_X0_BACK: u32 = 0xb5ff_ffe0; // cbnz x0, .-4
    const B_BACK: u32 = 0x17ff_ffff; // b .-4

    #[test]
    fn a_block_is_decoded_once_and_run_from_its_ops_afterwards() {
        let mut c = cpu();
        for i in 0..100 {
            put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
        }
        c.pc = 0x1000;
        c.run(100);
        assert_eq!(c.x[0], 100);
        // A block of 64 instructions, and a second that ends at the word after the last ADD (a zero word,
        // which `Gen` runs), which the run did not reach.
        let (blocks, insns) = (c.jit.pre.blocks, c.jit.pre.insns);
        assert_eq!(blocks, 2);
        assert_eq!(insns, 64 + 37);
        c.pc = 0x1000;
        c.run(100);
        assert_eq!(c.x[0], 200);
        assert_eq!((c.jit.pre.blocks, c.jit.pre.insns), (blocks, insns));
        assert_eq!(c.jit.pre.decoded, 1);
    }

    #[test]
    fn a_run_of_any_length_ends_exactly_at_its_budget() {
        let mut c = cpu();
        for i in 0..100 {
            put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
        }
        let mut counted = 0;
        for n in 1..=100u64 {
            c.pc = 0x1000;
            c.x[0] = 0;
            c.run(n);
            counted += n;
            assert_eq!(c.x[0], n);
            assert_eq!(c.pc, 0x1000 + 4 * n);
            assert_eq!(c.insn_count, counted);
        }
    }

    #[test]
    fn a_loop_in_the_page_runs_to_the_budget_and_counts_its_instructions() {
        for body in [[SUB_X0_1, CBNZ_X0_BACK], [ADD_X0_1, B_BACK]] {
            let mut c = cpu();
            put(&mut c, 0x1000, body[0]);
            put(&mut c, 0x1004, body[1]);
            for round in 1..=3u64 {
                c.pc = 0x1000;
                c.x[0] = 1000;
                let before = c.insn_count;
                c.run(1001);
                assert_eq!(c.insn_count - before, 1001, "round {round}");
                // `sub; cbnz` is back at the sub after an even number of instructions, `add; b` too.
                assert_eq!(c.pc, 0x1004, "round {round}");
            }
        }
    }

    #[test]
    fn an_exception_after_many_instructions_returns_to_the_right_one() {
        let mut c = cpu();
        c.sys.vbar = 0x4000;
        for i in 0..100 {
            put(&mut c, 0x1000 + 4 * i, ADD_X0_1);
        }
        put(&mut c, 0x1000 + 4 * 100, SVC_0);
        put(&mut c, 0x1000 + 4 * 101, ADD_X0_1);
        for round in 1..=2u64 {
            c.pc = 0x1000;
            c.x[0] = 0;
            c.run(101);
            assert_eq!(c.x[0], 100);
            assert_eq!(c.insn_count, 101 * round);
            assert_eq!(c.sys.elr, 0x1000 + 4 * 101);
            assert_eq!(c.sys.esr >> 26, 0x15);
            c.el = 1;
            c.spsel = true;
        }
    }

    #[test]
    fn a_load_that_faults_in_a_run_returns_to_itself() {
        let mut c = cpu();
        c.sys.vbar = 0x4000;
        // ldr x1, [x2] with x2 outside RAM.
        put(&mut c, 0x1000, ADD_X0_1);
        put(&mut c, 0x1004, 0xf940_0041);
        put(&mut c, 0x1008, ADD_X0_1);
        c.x[2] = 0x4000_0000_0000;
        c.pc = 0x1000;
        c.run(2);
        assert_eq!(c.x[0], 1);
        assert_eq!(c.sys.elr, 0x1004);
        // A data abort, from the current or a lower exception level.
        assert_eq!((c.sys.esr >> 26) | 1, 0x25);
    }

    #[test]
    fn the_flags_a_compare_leaves_are_seen_by_the_instructions_after_it() {
        let mut c = cpu();
        // subs xzr, x0, x1; b.lo .+8; mov x2, #1; mov x3, #2
        put(&mut c, 0x1000, 0xeb01_001f);
        put(&mut c, 0x1004, 0x5400_0043);
        put(&mut c, 0x1008, 0xd280_0022);
        put(&mut c, 0x100c, 0xd280_0043);
        for (x0, x1, lower) in [
            (1u64, 2u64, true),
            (2, 1, false),
            (u64::MAX, 1, false),
            (0, u64::MAX, true),
        ] {
            for _ in 0..2 {
                c.pc = 0x1000;
                c.x[0] = x0;
                c.x[1] = x1;
                c.x[2] = 0;
                c.x[3] = 0;
                c.run(if lower { 3 } else { 4 });
                assert_eq!(
                    (c.x[2], c.x[3]),
                    if lower { (0, 2) } else { (1, 2) },
                    "{x0} {x1}"
                );
            }
        }
    }
}
