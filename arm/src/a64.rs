//! A64 integer instruction set: decoding and execution.
//!
//! Decoding follows the top-level encoding table of the Arm ARM (op0 = bits
//! 28..25). Anything not implemented raises an Undefined Instruction exception,
//! which is also what real hardware does for unallocated encodings.

use crate::bits::{bit, decode_bit_masks, field, ones, sext, trunc};
use crate::board::{self, Host};
use crate::cpu::{ec, Cpu, Event, Res, StopReason};
use crate::sysreg::{self as sr, id};

/// AddWithCarry: returns (result, N, Z, C, V).
#[inline(always)]
fn add_with_carry(x: u64, y: u64, carry: bool, sf: bool) -> (u64, bool, bool, bool, bool) {
    if sf {
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u64);
        let v = ((x ^ r) & (y ^ r)) >> 63 != 0;
        (r, r >> 63 != 0, r == 0, c1 || c2, v)
    } else {
        let (x, y) = (x as u32, y as u32);
        let (r1, c1) = x.overflowing_add(y);
        let (r, c2) = r1.overflowing_add(carry as u32);
        let v = ((x ^ r) & (y ^ r)) >> 31 != 0;
        (r as u64, r >> 31 != 0, r == 0, c1 || c2, v)
    }
}

/// Shift a register operand (LSL/LSR/ASR/ROR) at the operand size.
#[inline(always)]
fn shift_reg(v: u64, kind: u32, amount: u32, sf: bool) -> u64 {
    if sf {
        let a = amount & 63;
        match kind {
            0 => v.wrapping_shl(a),
            1 => v.wrapping_shr(a),
            2 => ((v as i64).wrapping_shr(a)) as u64,
            _ => v.rotate_right(a),
        }
    } else {
        let v = v as u32;
        let a = amount & 31;
        (match kind {
            0 => v.wrapping_shl(a),
            1 => v.wrapping_shr(a),
            2 => ((v as i32).wrapping_shr(a)) as u32,
            _ => v.rotate_right(a),
        }) as u64
    }
}

/// ExtendReg: UXTB..SXTX followed by a left shift of 0..4.
#[inline(always)]
fn extend_reg(v: u64, option: u32, shift: u32) -> u64 {
    let e = match option {
        0 => v & 0xff,
        1 => v & 0xffff,
        2 => v & 0xffff_ffff,
        3 => v,
        4 => v as u8 as i8 as i64 as u64,
        5 => v as u16 as i16 as i64 as u64,
        6 => v as u32 as i32 as i64 as u64,
        _ => v,
    };
    e.wrapping_shl(shift)
}

impl<H: Host> Cpu<H> {
    pub(crate) fn execute(&mut self, insn: u32) -> Res<()> {
        match (insn >> 25) & 0xf {
            0b1000 | 0b1001 => self.dp_imm(insn),
            0b1010 | 0b1011 => self.branch_sys(insn),
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.ldst(insn),
            0b0101 | 0b1101 => self.dp_reg(insn),
            0b0111 | 0b1111 => self.simd_fp(insn),
            _ => Err(Event::undefined()),
        }
    }

    #[inline(always)]
    pub(crate) fn cond_holds(&self, cond: u32) -> bool {
        let r = match cond >> 1 {
            0 => self.z,
            1 => self.c,
            2 => self.n,
            3 => self.v,
            4 => self.c && !self.z,
            5 => self.n == self.v,
            6 => self.n == self.v && !self.z,
            _ => true,
        };
        if cond & 1 != 0 && cond != 0xf {
            !r
        } else {
            r
        }
    }

    #[inline(always)]
    fn set_flags(&mut self, n: bool, z: bool, c: bool, v: bool) {
        self.n = n;
        self.z = z;
        self.c = c;
        self.v = v;
    }

    #[inline(always)]
    fn set_nz_logical(&mut self, r: u64, sf: bool) {
        let n = if sf { r >> 63 != 0 } else { (r >> 31) & 1 != 0 };
        self.set_flags(n, r == 0, false, false);
    }

    // ===================================================== data processing (imm)

    fn dp_imm(&mut self, insn: u32) -> Res<()> {
        let sf = bit(insn, 31);
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        match field(insn, 23, 3) {
            0b000 | 0b001 => {
                // ADR / ADRP
                let immlo = field(insn, 29, 2) as u64;
                let immhi = field(insn, 5, 19) as u64;
                let imm = sext((immhi << 2) | immlo, 21);
                let v = if bit(insn, 31) {
                    (self.pc & !0xfff).wrapping_add((imm << 12) as u64)
                } else {
                    self.pc.wrapping_add(imm as u64)
                };
                self.wr(rd, v);
            },
            0b010 => {
                // ADD/ADDS/SUB/SUBS (immediate)
                let sub = bit(insn, 30);
                let set = bit(insn, 29);
                let mut imm = field(insn, 10, 12) as u64;
                if bit(insn, 22) {
                    imm <<= 12;
                }
                let x = self.xs(rn);
                let (r, n, z, c, v) = if sub {
                    add_with_carry(x, !imm, true, sf)
                } else {
                    add_with_carry(x, imm, false, sf)
                };
                let r = trunc(r, sf);
                if set {
                    self.set_flags(n, z, c, v);
                    self.wr(rd, r);
                } else {
                    self.ws(rd, r);
                }
            },
            0b100 => {
                // Logical (immediate)
                let opc = field(insn, 29, 2);
                let nbit = field(insn, 22, 1);
                if !sf && nbit == 1 {
                    return Err(Event::undefined());
                }
                let imm = decode_bit_masks(nbit, field(insn, 10, 6), field(insn, 16, 6), sf)
                    .ok_or_else(Event::undefined)?;
                let x = self.xr(rn);
                let r = trunc(
                    match opc {
                        0 | 3 => x & imm,
                        1 => x | imm,
                        _ => x ^ imm,
                    },
                    sf,
                );
                if opc == 3 {
                    self.set_nz_logical(r, sf);
                    self.wr(rd, r);
                } else {
                    self.ws(rd, r);
                }
            },
            0b101 => {
                // MOVN / MOVZ / MOVK
                let opc = field(insn, 29, 2);
                let hw = field(insn, 21, 2);
                if !sf && hw > 1 {
                    return Err(Event::undefined());
                }
                let shift = hw * 16;
                let imm = (field(insn, 5, 16) as u64) << shift;
                let r = match opc {
                    0 => !imm,
                    2 => imm,
                    3 => (self.xr(rd) & !(0xffffu64 << shift)) | imm,
                    _ => return Err(Event::undefined()),
                };
                self.wr(rd, trunc(r, sf));
            },
            0b110 => {
                // SBFM / BFM / UBFM
                let opc = field(insn, 29, 2);
                let nbit = bit(insn, 22);
                if nbit != sf || opc == 3 {
                    return Err(Event::undefined());
                }
                let immr = field(insn, 16, 6);
                let imms = field(insn, 10, 6);
                let size = if sf { 64 } else { 32 };
                if !sf && (immr >= 32 || imms >= 32) {
                    return Err(Event::undefined());
                }
                let src = trunc(self.xr(rn), sf);
                let r = self.bitfield(opc, src, self.xr(rd), immr, imms, size);
                self.wr(rd, trunc(r, sf));
            },
            _ => {
                // EXTR
                if field(insn, 29, 2) != 0 || bit(insn, 22) != sf || bit(insn, 21) {
                    return Err(Event::undefined());
                }
                let rm = field(insn, 16, 5);
                let lsb = field(insn, 10, 6);
                let (hi, lo) = (self.xr(rn), self.xr(rm));
                let r = if sf {
                    if lsb == 0 {
                        lo
                    } else {
                        (lo >> lsb) | (hi << (64 - lsb))
                    }
                } else {
                    if lsb >= 32 {
                        return Err(Event::undefined());
                    }
                    let (hi, lo) = (hi as u32, lo as u32);
                    (if lsb == 0 { lo } else { (lo >> lsb) | (hi << (32 - lsb)) }) as u64
                };
                self.wr(rd, r);
            },
        }
        Ok(())
    }

    fn bitfield(&self, opc: u32, src: u64, dst: u64, immr: u32, imms: u32, size: u32) -> u64 {
        if imms >= immr {
            // Extract bits [imms:immr] to the bottom.
            let width = imms - immr + 1;
            let f = (src >> immr) & ones(width);
            match opc {
                0 => (sext(f, width) as u64) & ones(size),
                1 => (dst & !ones(width)) | f,
                _ => f,
            }
        } else {
            // Insert the low imms+1 bits at position size-immr.
            let width = imms + 1;
            let pos = size - immr;
            let f = (src & ones(width)) << pos;
            match opc {
                0 => (sext(f, pos + width) as u64) & ones(size),
                1 => {
                    let mask = ones(width) << pos;
                    (dst & !mask) | f
                },
                _ => f,
            }
        }
    }

    // ================================================ branches, exceptions, system

    fn branch_sys(&mut self, insn: u32) -> Res<()> {
        if insn & 0x7c00_0000 == 0x1400_0000 {
            // B / BL
            let off = sext((insn & 0x3ff_ffff) as u64, 26) << 2;
            if bit(insn, 31) {
                self.x[30] = self.pc.wrapping_add(4);
            }
            self.next_pc = self.pc.wrapping_add(off as u64);
        } else if insn & 0x7e00_0000 == 0x3400_0000 {
            // CBZ / CBNZ
            let sf = bit(insn, 31);
            let t = trunc(self.xr(insn & 31), sf);
            if (t == 0) != bit(insn, 24) {
                let off = sext(field(insn, 5, 19) as u64, 19) << 2;
                self.next_pc = self.pc.wrapping_add(off as u64);
            }
        } else if insn & 0x7e00_0000 == 0x3600_0000 {
            // TBZ / TBNZ
            let b = (field(insn, 31, 1) << 5) | field(insn, 19, 5);
            let set = (self.xr(insn & 31) >> b) & 1 != 0;
            if set == bit(insn, 24) {
                let off = sext(field(insn, 5, 14) as u64, 14) << 2;
                self.next_pc = self.pc.wrapping_add(off as u64);
            }
        } else if insn & 0xff00_0010 == 0x5400_0000 {
            // B.cond
            if self.cond_holds(insn & 0xf) {
                let off = sext(field(insn, 5, 19) as u64, 19) << 2;
                self.next_pc = self.pc.wrapping_add(off as u64);
            }
        } else if insn & 0xff00_0000 == 0xd400_0000 {
            return self.exception_gen(insn);
        } else if insn & 0xffc0_0000 == 0xd500_0000 {
            return self.system(insn);
        } else if insn & 0xfe00_0000 == 0xd600_0000 {
            return self.branch_reg(insn);
        } else {
            return Err(Event::undefined());
        }
        Ok(())
    }

    fn exception_gen(&mut self, insn: u32) -> Res<()> {
        let opc = field(insn, 21, 3);
        let ll = insn & 3;
        let imm16 = field(insn, 5, 16);
        match (opc, ll) {
            (0, 1) => Err(Event::Sync {
                ec: ec::SVC64,
                iss: imm16,
                far: None,
                ret_next: true,
            }),
            // There is no EL2/EL3: HVC and SMC go to the emulated PSCI firmware.
            (0, 2) | (0, 3) => {
                if self.el == 0 {
                    return Err(Event::undefined());
                }
                self.psci_call()
            },
            (1, 0) => Err(Event::Sync {
                ec: ec::BRK64,
                iss: imm16,
                far: None,
                ret_next: false,
            }),
            (2, 0) => Err(Event::Stop(StopReason::Halt(imm16 as u16))),
            _ => Err(Event::undefined()),
        }
    }

    fn branch_reg(&mut self, insn: u32) -> Res<()> {
        let opc = field(insn, 21, 4);
        let op2 = field(insn, 16, 5);
        let op3 = field(insn, 10, 6);
        let rn = field(insn, 5, 5);
        let op4 = insn & 31;
        if op2 != 31 {
            return Err(Event::undefined());
        }
        match opc {
            0 | 1 | 2 if op3 == 0 && op4 == 0 => {
                let target = self.xr(rn);
                if opc == 1 {
                    self.x[30] = self.pc.wrapping_add(4);
                }
                self.next_pc = target;
            },
            4 if op3 == 0 && rn == 31 && op4 == 0 => {
                // ERET
                if self.el == 0 {
                    return Err(Event::undefined());
                }
                self.eret();
            },
            _ => return Err(Event::undefined()),
        }
        Ok(())
    }

    fn system(&mut self, insn: u32) -> Res<()> {
        let l = bit(insn, 21);
        let op0 = field(insn, 19, 2);
        let op1 = field(insn, 16, 3);
        let crn = field(insn, 12, 4);
        let crm = field(insn, 8, 4);
        let op2 = field(insn, 5, 3);
        let rt = insn & 31;
        match op0 {
            0 => {
                if l {
                    return Err(Event::undefined());
                }
                match crn {
                    2 => {
                        // Hints: NOP, YIELD, WFE, WFI, SEV(L), BTI, PAC*, ...
                        if crm == 0 && op2 == 3 {
                            if self.el == 0 && self.sys.sctlr & (1 << 16) == 0 {
                                return Err(Event::Sync {
                                    ec: ec::WFX,
                                    iss: 0,
                                    far: None,
                                    ret_next: false,
                                });
                            }
                            return Err(Event::Wfi);
                        }
                        Ok(())
                    },
                    3 => {
                        // Barriers and CLREX
                        if op2 == 2 {
                            self.set_excl(None);
                        }
                        Ok(())
                    },
                    4 => {
                        // MSR (immediate) to PSTATE fields
                        if self.el == 0
                            && !(op1 == 3
                                && (op2 == 6 || op2 == 7)
                                && self.sys.sctlr & (1 << 9) != 0)
                        {
                            return Err(Event::undefined());
                        }
                        match (op1, op2) {
                            (0, 5) => {
                                self.spsel = crm & 1 != 0;
                            },
                            (3, 6) => {
                                // Masking never makes an interrupt takeable.
                                self.daif |= (crm << 6) & (0xf << 6);
                            },
                            (3, 7) => {
                                self.daif &= !((crm << 6) & (0xf << 6));
                                self.irq_dirty = true;
                            },
                            _ => {}, // PAN/UAO/DIT/SSBS... not implemented: ignore
                        }
                        Ok(())
                    },
                    _ => Ok(()),
                }
            },
            1 => {
                if l {
                    // SYSL: nothing implemented
                    return Err(Event::undefined());
                }
                self.sys_insn(sr::key_of(insn), rt, op1, crn)
            },
            _ => {
                let key = sr::key_of(insn);
                if self.el == 0 && op1 != 3 {
                    return Err(Event::undefined());
                }
                if l {
                    let v = self.read_sysreg(key)?;
                    self.wr(rt, v);
                } else {
                    let v = self.xr(rt);
                    self.write_sysreg(key, v)?;
                }
                Ok(())
            },
        }
    }

    /// SYS instructions: cache maintenance, TLB invalidation, address translation.
    fn sys_insn(&mut self, key: u32, rt: u32, op1: u32, crn: u32) -> Res<()> {
        if key == sr::DC_ZVA {
            let va = self.xr(rt) & !63;
            for i in 0..8 {
                self.write(va + i * 8, 8, 0, false)?;
            }
            return Ok(());
        }
        if self.el == 0 && op1 != 3 {
            return Err(Event::undefined());
        }
        match crn {
            8 => self.tlb_flush(), // TLBI *
            7 => match key {
                sr::AT_S1E1R => self.address_translate(self.xr(rt), false, false),
                sr::AT_S1E1W => self.address_translate(self.xr(rt), true, false),
                sr::AT_S1E0R => self.address_translate(self.xr(rt), false, true),
                sr::AT_S1E0W => self.address_translate(self.xr(rt), true, true),
                _ => {}, // IC/DC maintenance: caches are coherent
            },
            _ => {},
        }
        Ok(())
    }

    pub fn read_sysreg(&mut self, key: u32) -> Res<u64> {
        let s = &self.sys;
        Ok(match key {
            sr::MIDR_EL1 => id::MIDR,
            sr::MPIDR_EL1 => id::MPIDR,
            sr::REVIDR_EL1 => 0,
            sr::ID_AA64PFR0_EL1 => id::AA64PFR0,
            sr::ID_AA64MMFR0_EL1 => id::AA64MMFR0,
            sr::ID_AA64DFR0_EL1 => id::AA64DFR0,
            sr::ID_AA64ISAR0_EL1 | sr::ID_AA64MMFR1_EL1 | sr::ID_AA64MMFR2_EL1 => 0,
            sr::CTR_EL0 => id::CTR,
            sr::DCZID_EL0 => id::DCZID,
            sr::CLIDR_EL1 => id::CLIDR,
            sr::CCSIDR_EL1 => 0,
            sr::CSSELR_EL1 => s.csselr,
            sr::SCTLR_EL1 => s.sctlr,
            sr::CPACR_EL1 => s.cpacr,
            sr::TTBR0_EL1 => s.ttbr0,
            sr::TTBR1_EL1 => s.ttbr1,
            sr::TCR_EL1 => s.tcr,
            sr::SPSR_EL1 => s.spsr,
            sr::ELR_EL1 => s.elr,
            sr::SP_EL0 => {
                if self.el == 1 && !self.spsel {
                    return Err(Event::undefined());
                }
                self.sp_el0
            },
            sr::SPSEL => self.spsel as u64,
            sr::CURRENT_EL => (self.el as u64) << 2,
            sr::NZCV => self.nzcv(),
            sr::DAIF => self.daif as u64,
            sr::PAN | sr::UAO => 0,
            sr::FPCR => s.fpcr,
            sr::FPSR => s.fpsr,
            sr::ESR_EL1 => s.esr,
            sr::FAR_EL1 => s.far,
            sr::PAR_EL1 => s.par,
            sr::MAIR_EL1 => s.mair,
            sr::VBAR_EL1 => s.vbar,
            sr::ISR_EL1 => (self.gic.irq_asserted() as u64) << 7,
            sr::CONTEXTIDR_EL1 => s.contextidr,
            sr::TPIDR_EL1 => s.tpidr_el1,
            sr::TPIDR_EL0 => s.tpidr_el0,
            sr::TPIDRRO_EL0 => s.tpidrro_el0,
            sr::CNTKCTL_EL1 => s.cntkctl,
            sr::CNTFRQ_EL0 => board::TIMER_FREQ_HZ,
            sr::CNTPCT_EL0 | sr::CNTVCT_EL0 | sr::CNTPCTSS_EL0 | sr::CNTVCTSS_EL0 => self.counter(),
            sr::CNTV_CTL_EL0 | sr::CNTP_CTL_EL0 => {
                self.update_timers();
                if key == sr::CNTV_CTL_EL0 {
                    self.sys.cntv_ctl
                } else {
                    self.sys.cntp_ctl
                }
            },
            sr::CNTV_CVAL_EL0 => s.cntv_cval,
            sr::CNTP_CVAL_EL0 => s.cntp_cval,
            sr::CNTV_TVAL_EL0 | sr::CNTP_TVAL_EL0 => {
                let cval = if key == sr::CNTV_TVAL_EL0 { s.cntv_cval } else { s.cntp_cval };
                cval.wrapping_sub(self.counter()) & 0xffff_ffff
            },
            sr::OSLSR_EL1 => 0x8, // OS lock implemented, unlocked
            _ => *s.other.get(&key).unwrap_or(&0),
        })
    }

    pub fn write_sysreg(&mut self, key: u32, v: u64) -> Res<()> {
        match key {
            sr::SCTLR_EL1 => {
                self.sys.sctlr = v;
                self.tlb_flush();
            },
            sr::CPACR_EL1 => self.sys.cpacr = v,
            sr::TTBR0_EL1 => {
                self.sys.ttbr0 = v;
                self.tlb_flush();
            },
            sr::TTBR1_EL1 => {
                self.sys.ttbr1 = v;
                self.tlb_flush();
            },
            sr::TCR_EL1 => {
                self.sys.tcr = v;
                self.tlb_flush();
            },
            sr::SPSR_EL1 => self.sys.spsr = v,
            sr::ELR_EL1 => self.sys.elr = v,
            sr::SP_EL0 => {
                if self.el == 1 && !self.spsel {
                    return Err(Event::undefined());
                }
                self.sp_el0 = v;
            },
            sr::SP_EL1 => return Err(Event::undefined()),
            sr::SPSEL => self.spsel = v & 1 != 0,
            sr::NZCV => self.set_nzcv(v),
            sr::DAIF => {
                self.daif = (v as u32) & (0xf << 6);
                if self.daif & (1 << 7) == 0 {
                    self.irq_dirty = true;
                }
            },
            sr::FPCR => self.sys.fpcr = v,
            sr::FPSR => self.sys.fpsr = v,
            sr::ESR_EL1 => self.sys.esr = v,
            sr::FAR_EL1 => self.sys.far = v,
            sr::PAR_EL1 => self.sys.par = v,
            sr::MAIR_EL1 => self.sys.mair = v,
            sr::VBAR_EL1 => self.sys.vbar = v & !0x7ff,
            sr::CONTEXTIDR_EL1 => self.sys.contextidr = v,
            sr::TPIDR_EL1 => self.sys.tpidr_el1 = v,
            sr::TPIDR_EL0 => self.sys.tpidr_el0 = v,
            sr::TPIDRRO_EL0 => {
                if self.el == 0 {
                    return Err(Event::undefined());
                }
                self.sys.tpidrro_el0 = v
            },
            sr::CNTKCTL_EL1 => self.sys.cntkctl = v,
            sr::CSSELR_EL1 => self.sys.csselr = v,
            sr::CNTV_CTL_EL0 => {
                self.sys.cntv_ctl = (self.sys.cntv_ctl & 4) | (v & 3);
                self.update_timers();
            },
            sr::CNTP_CTL_EL0 => {
                self.sys.cntp_ctl = (self.sys.cntp_ctl & 4) | (v & 3);
                self.update_timers();
            },
            sr::CNTV_CVAL_EL0 => {
                self.sys.cntv_cval = v;
                self.update_timers();
            },
            sr::CNTP_CVAL_EL0 => {
                self.sys.cntp_cval = v;
                self.update_timers();
            },
            sr::CNTV_TVAL_EL0 | sr::CNTP_TVAL_EL0 => {
                let cval = self.counter().wrapping_add(v as u32 as i32 as i64 as u64);
                if key == sr::CNTV_TVAL_EL0 {
                    self.sys.cntv_cval = cval;
                } else {
                    self.sys.cntp_cval = cval;
                }
                self.update_timers();
            },
            // Read-only identification and status registers ignore writes.
            sr::MIDR_EL1
            | sr::MPIDR_EL1
            | sr::CTR_EL0
            | sr::DCZID_EL0
            | sr::CURRENT_EL
            | sr::ISR_EL1
            | sr::CNTFRQ_EL0
            | sr::CNTPCT_EL0
            | sr::CNTVCT_EL0 => {},
            _ => {
                self.sys.other.insert(key, v);
            },
        }
        Ok(())
    }

    // ===================================================== loads and stores

    fn ldst(&mut self, insn: u32) -> Res<()> {
        if bit(insn, 26) {
            return self.simd_ldst(insn);
        }
        if insn & 0x3f00_0000 == 0x0800_0000 {
            self.ldst_exclusive(insn)
        } else if insn & 0x3b00_0000 == 0x1800_0000 {
            // LDR (literal)
            let off = sext(field(insn, 5, 19) as u64, 19) << 2;
            let addr = self.pc.wrapping_add(off as u64);
            let rt = insn & 31;
            match field(insn, 30, 2) {
                0 => {
                    let v = self.read(addr, 4, false)?;
                    self.wr(rt, v);
                },
                1 => {
                    let v = self.read(addr, 8, false)?;
                    self.wr(rt, v);
                },
                2 => {
                    let v = self.read(addr, 4, false)?;
                    self.wr(rt, v as u32 as i32 as i64 as u64);
                },
                _ => {}, // PRFM
            }
            Ok(())
        } else if insn & 0x3800_0000 == 0x2800_0000 {
            self.ldst_pair(insn)
        } else if insn & 0x3800_0000 == 0x3800_0000 {
            self.ldst_reg(insn)
        } else {
            Err(Event::undefined())
        }
    }

    fn ldst_pair(&mut self, insn: u32) -> Res<()> {
        let opc = field(insn, 30, 2);
        let mode = field(insn, 23, 2);
        let load = bit(insn, 22);
        let rt2 = field(insn, 10, 5);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let (size, signed) = match (opc, load) {
            (0, _) => (4u32, false),
            (2, _) => (8, false),
            (1, true) if mode != 0 => (4, true), // LDPSW (there is no LDNPSW)
            _ => return Err(Event::undefined()),
        };
        let imm = sext(field(insn, 15, 7) as u64, 7) * size as i64;
        let base = self.xs(rn);
        let addr = if mode == 1 { base } else { base.wrapping_add(imm as u64) };
        if load {
            let a = self.read(addr, size, false)?;
            let b = self.read(addr.wrapping_add(size as u64), size, false)?;
            let fix = |v: u64| if signed { v as u32 as i32 as i64 as u64 } else { v };
            self.wr(rt, fix(a));
            self.wr(rt2, fix(b));
        } else {
            let (a, b) = (self.xr(rt), self.xr(rt2));
            self.write(addr, size, a, false)?;
            self.write(addr.wrapping_add(size as u64), size, b, false)?;
        }
        if mode == 1 || mode == 3 {
            self.ws(rn, base.wrapping_add(imm as u64));
        }
        Ok(())
    }

    fn ldst_reg(&mut self, insn: u32) -> Res<()> {
        let size_log = field(insn, 30, 2);
        let opc = field(insn, 22, 2);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let base = self.xs(rn);
        let mut unpriv = false;
        let mut writeback: Option<u64> = None;
        let addr = if bit(insn, 24) {
            // Unsigned scaled 12-bit offset
            base.wrapping_add((field(insn, 10, 12) as u64) << size_log)
        } else if !bit(insn, 21) {
            let imm = sext(field(insn, 12, 9) as u64, 9) as u64;
            match field(insn, 10, 2) {
                0 => base.wrapping_add(imm), // LDUR/STUR
                1 => {
                    writeback = Some(base.wrapping_add(imm));
                    base
                },
                2 => {
                    unpriv = true; // LDTR/STTR
                    base.wrapping_add(imm)
                },
                _ => {
                    let a = base.wrapping_add(imm);
                    writeback = Some(a);
                    a
                },
            }
        } else if field(insn, 10, 2) == 2 {
            // Register offset
            let rm = field(insn, 16, 5);
            let option = field(insn, 13, 3);
            if option & 2 == 0 {
                return Err(Event::undefined());
            }
            let shift = if bit(insn, 12) { size_log } else { 0 };
            base.wrapping_add(extend_reg(self.xr(rm), option, shift))
        } else {
            // Atomic memory operations (LSE): not advertised.
            return Err(Event::undefined());
        };
        let bytes = 1u32 << size_log;
        match opc {
            0 => {
                let v = self.xr(rt);
                self.write(addr, bytes, v, unpriv)?;
            },
            1 => {
                let v = self.read(addr, bytes, unpriv)?;
                self.wr(rt, v);
            },
            2 => {
                if size_log == 3 {
                    // PRFM/PRFUM: only without writeback or unprivileged access
                    if writeback.is_some() || unpriv {
                        return Err(Event::undefined());
                    }
                    return Ok(());
                } else {
                    let v = self.read(addr, bytes, unpriv)?;
                    self.wr(rt, sext(v, bytes * 8) as u64);
                }
            },
            _ => {
                if size_log >= 2 {
                    return Err(Event::undefined());
                }
                let v = self.read(addr, bytes, unpriv)?;
                self.wr(rt, sext(v, bytes * 8) as u64 & 0xffff_ffff);
            },
        }
        if let Some(wb) = writeback {
            self.ws(rn, wb);
        }
        Ok(())
    }

    /// LDXR/STXR/LDAXR/STLXR (+pair forms), LDAR/STLR.
    fn ldst_exclusive(&mut self, insn: u32) -> Res<()> {
        let size_log = field(insn, 30, 2);
        let o2 = bit(insn, 23);
        let load = bit(insn, 22);
        let o1 = bit(insn, 21);
        let rs = field(insn, 16, 5);
        let rt2 = field(insn, 10, 5);
        let rn = field(insn, 5, 5);
        let rt = insn & 31;
        let bytes = 1u32 << size_log;
        let addr = self.xs(rn);
        // LDAR/STLR: Rs and Rt2 must be all ones (UNDEFINED otherwise, as in QEMU).
        if o2 && (rs != 31 || rt2 != 31) {
            return Err(Event::undefined());
        }
        if o2 {
            if o1 || !bit(insn, 15) {
                // CAS family (LSE) and LDLAR/STLLR (LORegions): not in ARMv8.0.
                return Err(Event::undefined());
            }
            // LDAR/STLR (and LDLAR/STLLR): ordinary accesses on a single core.
            if load {
                let v = self.read(addr, bytes, false)?;
                self.wr(rt, v);
            } else {
                let v = self.xr(rt);
                self.write(addr, bytes, v, false)?;
            }
            return Ok(());
        }
        if o1 && size_log < 2 {
            return Err(Event::undefined());
        }
        if load {
            let pa = self.translate(addr, crate::mmu::Access::Read, false)?;
            if o1 {
                let a = self.read(addr, bytes, false)?;
                let b = self.read(addr.wrapping_add(bytes as u64), bytes, false)?;
                self.wr(rt, a);
                self.wr(rt2, b);
            } else {
                let v = self.read(addr, bytes, false)?;
                self.wr(rt, v);
            }
            self.set_excl(Some(pa));
        } else {
            let pa = self.translate(addr, crate::mmu::Access::Write, false)?;
            let ok = self.excl().is_some_and(|e| e & !0x3f == pa & !0x3f);
            if ok {
                if o1 {
                    let (a, b) = (self.xr(rt), self.xr(rt2));
                    self.write(addr, bytes, a, false)?;
                    self.write(addr.wrapping_add(bytes as u64), bytes, b, false)?;
                } else {
                    let v = self.xr(rt);
                    self.write(addr, bytes, v, false)?;
                }
            }
            self.set_excl(None);
            self.wr(rs, (!ok) as u64);
        }
        Ok(())
    }

    // ===================================================== data processing (reg)

    fn dp_reg(&mut self, insn: u32) -> Res<()> {
        let sf = bit(insn, 31);
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        let rm = field(insn, 16, 5);
        if insn & 0x1f00_0000 == 0x0a00_0000 {
            // Logical (shifted register)
            let opc = field(insn, 29, 2);
            let imm6 = field(insn, 10, 6);
            if !sf && imm6 >= 32 {
                return Err(Event::undefined());
            }
            let mut op2 = shift_reg(self.xr(rm), field(insn, 22, 2), imm6, sf);
            if bit(insn, 21) {
                op2 = !op2;
            }
            let x = self.xr(rn);
            let r = trunc(
                match opc {
                    0 | 3 => x & op2,
                    1 => x | op2,
                    _ => x ^ op2,
                },
                sf,
            );
            if opc == 3 {
                self.set_nz_logical(r, sf);
            }
            self.wr(rd, r);
        } else if insn & 0x1f20_0000 == 0x0b00_0000 {
            // ADD/SUB (shifted register)
            let kind = field(insn, 22, 2);
            let imm6 = field(insn, 10, 6);
            if kind == 3 || (!sf && imm6 >= 32) {
                return Err(Event::undefined());
            }
            let op2 = shift_reg(self.xr(rm), kind, imm6, sf);
            let r = self.addsub(insn, self.xr(rn), op2, sf);
            self.wr(rd, r);
        } else if insn & 0x1f20_0000 == 0x0b20_0000 {
            // ADD/SUB (extended register)
            let imm3 = field(insn, 10, 3);
            if imm3 > 4 || field(insn, 22, 2) != 0 {
                return Err(Event::undefined());
            }
            let op2 = trunc(extend_reg(self.xr(rm), field(insn, 13, 3), imm3), sf);
            let r = self.addsub(insn, self.xs(rn), op2, sf);
            if bit(insn, 29) {
                self.wr(rd, r);
            } else {
                self.ws(rd, r);
            }
        } else if insn & 0x1fe0_0000 == 0x1a00_0000 {
            // ADC/SBC
            if field(insn, 10, 6) != 0 {
                return Err(Event::undefined());
            }
            let mut y = self.xr(rm);
            if bit(insn, 30) {
                y = !y;
            }
            let (r, n, z, c, v) = add_with_carry(self.xr(rn), y, self.c, sf);
            if bit(insn, 29) {
                self.set_flags(n, z, c, v);
            }
            self.wr(rd, trunc(r, sf));
        } else if insn & 0x1fe0_0000 == 0x1a40_0000 {
            // CCMN/CCMP (register or immediate)
            if !bit(insn, 29) || bit(insn, 10) || bit(insn, 4) {
                return Err(Event::undefined());
            }
            let cond = field(insn, 12, 4);
            if self.cond_holds(cond) {
                let y = if bit(insn, 11) { rm as u64 } else { self.xr(rm) };
                let x = self.xr(rn);
                let (_, n, z, c, v) = if bit(insn, 30) {
                    add_with_carry(x, !y, true, sf)
                } else {
                    add_with_carry(x, y, false, sf)
                };
                self.set_flags(n, z, c, v);
            } else {
                self.set_nzcv(((insn & 0xf) as u64) << 28);
            }
        } else if insn & 0x1fe0_0000 == 0x1a80_0000 {
            // CSEL/CSINC/CSINV/CSNEG
            if bit(insn, 29) || bit(insn, 11) {
                return Err(Event::undefined());
            }
            let op = bit(insn, 30);
            let o2 = bit(insn, 10);
            let r = if self.cond_holds(field(insn, 12, 4)) {
                self.xr(rn)
            } else {
                let m = self.xr(rm);
                match (op, o2) {
                    (false, false) => m,
                    (false, true) => m.wrapping_add(1),
                    (true, false) => !m,
                    (true, true) => m.wrapping_neg(),
                }
            };
            self.wr(rd, trunc(r, sf));
        } else if insn & 0x5fe0_0000 == 0x1ac0_0000 {
            self.dp_2src(insn, sf, rd, rn, rm)?;
        } else if insn & 0x5fe0_0000 == 0x5ac0_0000 {
            self.dp_1src(insn, sf, rd, rn)?;
        } else if insn & 0x1f00_0000 == 0x1b00_0000 {
            self.dp_3src(insn, sf, rd, rn, rm)?;
        } else {
            return Err(Event::undefined());
        }
        Ok(())
    }

    #[inline(always)]
    fn addsub(&mut self, insn: u32, x: u64, y: u64, sf: bool) -> u64 {
        let (r, n, z, c, v) = if bit(insn, 30) {
            add_with_carry(x, !y, true, sf)
        } else {
            add_with_carry(x, y, false, sf)
        };
        if bit(insn, 29) {
            self.set_flags(n, z, c, v);
        }
        trunc(r, sf)
    }

    fn dp_2src(&mut self, insn: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> Res<()> {
        if bit(insn, 29) {
            return Err(Event::undefined());
        }
        let (a, b) = (self.xr(rn), self.xr(rm));
        let r = match field(insn, 10, 6) {
            0b000010 => {
                if sf {
                    a.checked_div(b).unwrap_or(0)
                } else {
                    (a as u32).checked_div(b as u32).unwrap_or(0) as u64
                }
            },
            0b000011 => {
                if sf {
                    if b == 0 {
                        0
                    } else {
                        (a as i64).wrapping_div(b as i64) as u64
                    }
                } else if b as u32 == 0 {
                    0
                } else {
                    (a as i32).wrapping_div(b as i32) as u32 as u64
                }
            },
            0b001000 => shift_reg(a, 0, b as u32, sf),
            0b001001 => shift_reg(a, 1, b as u32, sf),
            0b001010 => shift_reg(a, 2, b as u32, sf),
            0b001011 => shift_reg(a, 3, b as u32, sf),
            _ => return Err(Event::undefined()),
        };
        self.wr(rd, trunc(r, sf));
        Ok(())
    }

    fn dp_1src(&mut self, insn: u32, sf: bool, rd: u32, rn: u32) -> Res<()> {
        if bit(insn, 29) || field(insn, 16, 5) != 0 {
            return Err(Event::undefined());
        }
        let a = self.xr(rn);
        let r = match (field(insn, 10, 6), sf) {
            (0, true) => a.reverse_bits(),
            (0, false) => (a as u32).reverse_bits() as u64,
            (1, _) => {
                // REV16: swap bytes in each halfword
                let m = 0x00ff_00ff_00ff_00ffu64;
                ((a >> 8) & m) | ((a & m) << 8)
            },
            (2, false) => (a as u32).swap_bytes() as u64,
            (2, true) => {
                // REV32: swap bytes in each word
                let lo = (a as u32).swap_bytes() as u64;
                let hi = ((a >> 32) as u32).swap_bytes() as u64;
                (hi << 32) | lo
            },
            (3, true) => a.swap_bytes(),
            (4, true) => a.leading_zeros() as u64,
            (4, false) => (a as u32).leading_zeros() as u64,
            (5, true) => {
                let s = a ^ ((a as i64 >> 1) as u64);
                (s.leading_zeros() as u64).saturating_sub(1)
            },
            (5, false) => {
                let w = a as u32;
                let s = w ^ ((w as i32 >> 1) as u32);
                (s.leading_zeros() as u64).saturating_sub(1)
            },
            _ => return Err(Event::undefined()),
        };
        self.wr(rd, trunc(r, sf));
        Ok(())
    }

    fn dp_3src(&mut self, insn: u32, sf: bool, rd: u32, rn: u32, rm: u32) -> Res<()> {
        if field(insn, 29, 2) != 0 {
            return Err(Event::undefined());
        }
        let op31 = field(insn, 21, 3);
        let o0 = bit(insn, 15);
        let ra = field(insn, 10, 5);
        let (n, m, acc) = (self.xr(rn), self.xr(rm), self.xr(ra));
        let r = match (op31, sf) {
            (0, _) => {
                let p = n.wrapping_mul(m);
                if o0 {
                    acc.wrapping_sub(p)
                } else {
                    acc.wrapping_add(p)
                }
            },
            (1, true) => {
                let p = (n as i32 as i64).wrapping_mul(m as i32 as i64) as u64;
                if o0 {
                    acc.wrapping_sub(p)
                } else {
                    acc.wrapping_add(p)
                }
            },
            (5, true) => {
                let p = (n as u32 as u64).wrapping_mul(m as u32 as u64);
                if o0 {
                    acc.wrapping_sub(p)
                } else {
                    acc.wrapping_add(p)
                }
            },
            (2, true) if !o0 => ((n as i64 as i128 * m as i64 as i128) >> 64) as u64,
            (6, true) if !o0 => ((n as u128 * m as u128) >> 64) as u64,
            _ => return Err(Event::undefined()),
        };
        self.wr(rd, trunc(r, sf));
        Ok(())
    }
}
