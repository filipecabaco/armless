//! Scalar floating point and Advanced SIMD data processing (ARMv8.0, no
//! crypto, no FP16 arithmetic). Decoding follows the Arm ARM class tables.

use crate::bits::{bit, field, ones};
use crate::board::Host;
use crate::cpu::{ec, Cpu, Event, Res};
use crate::fp::{self, Float, Fpst, Round};

// ------------------------------------------------------------- lane helpers

#[inline(always)]
pub fn elem(v: u128, i: u32, esize: u32) -> u64 {
    ((v >> (i * esize)) as u64) & ones(esize)
}

#[inline(always)]
pub fn set_elem(v: &mut u128, i: u32, esize: u32, x: u64) {
    let m = (ones(esize) as u128) << (i * esize);
    *v = (*v & !m) | (((x & ones(esize)) as u128) << (i * esize));
}

impl<H: Host> Cpu<H> {
    /// CPACR_EL1.FPEN check for any SIMD&FP instruction.
    pub(crate) fn check_fp_access(&self) -> Res<()> {
        let fpen = (self.sys.cpacr >> 20) & 3;
        let trapped = match fpen {
            3 => false,
            1 => self.el == 0,
            _ => true,
        };
        if trapped {
            return Err(Event::Sync {
                ec: ec::FP_ACCESS,
                iss: 0,
                far: None,
                ret_next: false,
            });
        }
        Ok(())
    }

    /// Run `f` with an FP status context; FPSR flags are written back.
    #[inline]
    fn with_fp<R>(&mut self, f: impl FnOnce(&mut Fpst) -> R) -> R {
        let mut fpsr = self.sys.fpsr;
        let r = {
            let mut st = Fpst {
                fpcr: self.sys.fpcr,
                fpsr: &mut fpsr,
            };
            f(&mut st)
        };
        self.sys.fpsr = fpsr;
        r
    }

    #[inline(always)]
    fn vr(&self, r: u32) -> u128 {
        self.vreg[r as usize]
    }

    pub(crate) fn simd_fp(&mut self, insn: u32) -> Res<()> {
        self.check_fp_access()?;
        if insn & 0x5e00_0000 == 0x1e00_0000 {
            return self.scalar_fp(insn);
        }
        self.advsimd(insn)
    }

    // ============================================================ scalar FP

    fn scalar_fp(&mut self, insn: u32) -> Res<()> {
        let conversion = insn & 0x5f20_0000 == 0x1e00_0000 || insn & 0x5f20_fc00 == 0x1e20_0000;
        if bit(insn, 31) && !conversion {
            return Err(Event::undefined()); // M bit
        }
        if insn & 0x5f00_0000 == 0x1f00_0000 {
            return self.fp_dp3(insn);
        }
        if bit(insn, 29) {
            return Err(Event::undefined()); // S bit set
        }
        if insn & 0x5f20_0000 == 0x1e00_0000 {
            return self.fp_fixed_conv(insn);
        }
        // Everything else has bit 21 set.
        if insn & 0x5f20_fc00 == 0x1e20_0000 {
            return self.fp_int_conv(insn);
        }
        if insn & 0x5f20_7c00 == 0x1e20_4000 {
            return self.fp_dp1(insn);
        }
        if insn & 0x5f20_3c00 == 0x1e20_2000 {
            return self.fp_compare(insn);
        }
        if insn & 0x5f20_1c00 == 0x1e20_1000 {
            return self.fp_imm(insn);
        }
        match field(insn, 10, 2) {
            1 => self.fp_ccmp(insn),
            2 => self.fp_dp2(insn),
            3 => self.fp_csel(insn),
            _ => Err(Event::undefined()),
        }
    }

    fn fp_type(insn: u32) -> Res<u32> {
        match field(insn, 22, 2) {
            0 => Ok(32),
            1 => Ok(64),
            _ => Err(Event::undefined()),
        }
    }

    fn fp_dp2(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let op = field(insn, 12, 4);
        if op > 8 {
            return Err(Event::undefined());
        }
        let (a, b) = (self.vr(rn) as u64, self.vr(rm) as u64);
        let r = self.with_fp(|st| {
            if ty == 32 {
                fp_bin::<f32>(st, op, a, b)
            } else {
                fp_bin::<f64>(st, op, a, b)
            }
        });
        self.vreg[rd as usize] = r as u128;
        Ok(())
    }

    fn fp_dp1(&mut self, insn: u32) -> Res<()> {
        let ty = field(insn, 22, 2);
        let op = field(insn, 15, 6);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let a = self.vr(rn) as u64;
        // FCVT between precisions
        if op & 0b111100 == 0b000100 {
            let to = op & 3;
            if to == ty || to == 2 || ty == 2 {
                return Err(Event::undefined());
            }
            let ahp = self.sys.fpcr & (1 << 26) != 0;
            let r = self.with_fp(|st| {
                let v: f64 = match ty {
                    0 => fp::f32_to_f64(st, f32::from_bits(a as u32)),
                    1 => f64::from_bits(a),
                    _ => fp::f16_to_f64(st, a as u16, ahp),
                };
                match to {
                    0 => {
                        if ty == 1 {
                            fp::f64_to_f32(st, v).to_bits() as u64
                        } else {
                            // half -> single is exact
                            fp::f64_to_f32(st, v).to_bits() as u64
                        }
                    },
                    1 => {
                        if ty == 0 {
                            fp::f32_to_f64(st, f32::from_bits(a as u32)).to_bits()
                        } else {
                            v.to_bits()
                        }
                    },
                    _ => {
                        if ty == 0 {
                            // single -> half: NaN conversion must use the single payload
                            let s = f32::from_bits(a as u32);
                            if s.is_nan() {
                                let w = fp::f32_to_f64(st, s);
                                fp::f64_to_f16(st, w, ahp) as u64
                            } else {
                                fp::f64_to_f16(st, s as f64, ahp) as u64
                            }
                        } else {
                            // double -> half rounds once from the double value
                            f64_to_f16_direct(st, v, ahp) as u64
                        }
                    },
                }
            });
            self.vreg[rd as usize] = r as u128;
            return Ok(());
        }
        if ty > 1 {
            return Err(Event::undefined());
        }
        let r = self.with_fp(|st| -> Option<u64> {
            Some(if ty == 0 { fp_un::<f32>(st, op, a)? } else { fp_un::<f64>(st, op, a)? })
        });
        match r {
            Some(v) => {
                self.vreg[rd as usize] = v as u128;
                Ok(())
            },
            None => Err(Event::undefined()),
        }
    }

    fn fp_compare(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        if field(insn, 14, 2) != 0 || insn & 7 != 0 {
            return Err(Event::undefined());
        }
        let zero = bit(insn, 3);
        let signal = bit(insn, 4);
        let (rn, rm) = (field(insn, 5, 5), field(insn, 16, 5));
        let a = self.vr(rn) as u64;
        let b = if zero { 0 } else { self.vr(rm) as u64 };
        let nzcv = self.with_fp(|st| {
            if ty == 32 {
                fp::compare(st, f32::from_bits64(a), f32::from_bits64(b), signal)
            } else {
                fp::compare(st, f64::from_bits64(a), f64::from_bits64(b), signal)
            }
        });
        self.set_nzcv(nzcv << 28);
        Ok(())
    }

    fn fp_ccmp(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        let (rn, rm) = (field(insn, 5, 5), field(insn, 16, 5));
        let cond = field(insn, 12, 4);
        let signal = bit(insn, 4);
        if self.cond_holds(cond) {
            let (a, b) = (self.vr(rn) as u64, self.vr(rm) as u64);
            let nzcv = self.with_fp(|st| {
                if ty == 32 {
                    fp::compare(st, f32::from_bits64(a), f32::from_bits64(b), signal)
                } else {
                    fp::compare(st, f64::from_bits64(a), f64::from_bits64(b), signal)
                }
            });
            self.set_nzcv(nzcv << 28);
        } else {
            self.set_nzcv(((insn & 0xf) as u64) << 28);
        }
        Ok(())
    }

    fn fp_csel(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let src = if self.cond_holds(field(insn, 12, 4)) { rn } else { rm };
        let v = self.vr(src) & ones(ty) as u128;
        self.vreg[rd as usize] = v;
        Ok(())
    }

    fn fp_imm(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        if field(insn, 5, 5) != 0 {
            return Err(Event::undefined());
        }
        let imm8 = field(insn, 13, 8) as u64;
        self.vreg[(insn & 31) as usize] = vfp_expand_imm(imm8, ty) as u128;
        Ok(())
    }

    fn fp_dp3(&mut self, insn: u32) -> Res<()> {
        let ty = Self::fp_type(insn)?;
        let (rd, rn, rm, ra) = (
            insn & 31,
            field(insn, 5, 5),
            field(insn, 16, 5),
            field(insn, 10, 5),
        );
        let o1 = bit(insn, 21);
        let o0 = bit(insn, 15);
        let (a, n, m) = (self.vr(ra) as u64, self.vr(rn) as u64, self.vr(rm) as u64);
        let sign = if ty == 32 { 1u64 << 31 } else { 1u64 << 63 };
        // FMADD: a + n*m; FMSUB: a + (-n)*m; FNMADD: -a + (-n)*m; FNMSUB: -a + n*m
        let n = if o0 != o1 { n ^ sign } else { n };
        let a = if o1 { a ^ sign } else { a };
        let r = self.with_fp(|st| {
            if ty == 32 {
                fp::muladd(
                    st,
                    f32::from_bits64(a),
                    f32::from_bits64(n),
                    f32::from_bits64(m),
                )
                .bits()
            } else {
                fp::muladd(
                    st,
                    f64::from_bits64(a),
                    f64::from_bits64(n),
                    f64::from_bits64(m),
                )
                .bits()
            }
        });
        self.vreg[rd as usize] = r as u128;
        Ok(())
    }

    fn fp_int_conv(&mut self, insn: u32) -> Res<()> {
        let sf = bit(insn, 31);
        let ty = field(insn, 22, 2);
        let rmode = field(insn, 19, 2);
        let opcode = field(insn, 16, 3);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let ibits = if sf { 64 } else { 32 };
        match opcode {
            0b110 | 0b111 => {
                // FMOV between general and FP registers
                let to_gen = opcode == 0b110;
                match (sf, ty, rmode) {
                    (false, 0, 0) | (true, 1, 0) => {
                        let bits = if sf { 64 } else { 32 };
                        if to_gen {
                            let v = (self.vr(rn) as u64) & ones(bits);
                            self.wr(rd, v);
                        } else {
                            self.vreg[rd as usize] = (self.xr(rn) & ones(bits)) as u128;
                        }
                    },
                    (true, 2, 1) => {
                        if to_gen {
                            let v = (self.vr(rn) >> 64) as u64;
                            self.wr(rd, v);
                        } else {
                            let lo = self.vr(rd) & u64::MAX as u128;
                            self.vreg[rd as usize] = lo | (self.xr(rn) as u128) << 64;
                        }
                    },
                    _ => return Err(Event::undefined()),
                }
                Ok(())
            },
            _ => {
                if ty > 1 {
                    return Err(Event::undefined());
                }
                let fbits = if ty == 0 { 32 } else { 64 };
                let (mode, unsigned, to_int) = match (opcode, rmode) {
                    (0, r) => (Round::from_rmode(r as u64), false, true),
                    (1, r) => (Round::from_rmode(r as u64), true, true),
                    (2, 0) => (Round::Nearest, false, false),
                    (3, 0) => (Round::Nearest, true, false),
                    (4, 0) => (Round::TiesAway, false, true),
                    (5, 0) => (Round::TiesAway, true, true),
                    _ => return Err(Event::undefined()),
                };
                if to_int {
                    let a = self.vr(rn) as u64;
                    let r = self.with_fp(|st| {
                        if fbits == 32 {
                            fp::to_int(st, f32::from_bits64(a), ibits, unsigned, 0, mode)
                        } else {
                            fp::to_int(st, f64::from_bits64(a), ibits, unsigned, 0, mode)
                        }
                    });
                    self.wr(rd, r);
                } else {
                    let mut x = self.xr(rn) & ones(ibits);
                    if !unsigned && !sf {
                        x = x as u32 as i32 as i64 as u64;
                    }
                    let r = self.with_fp(|st| {
                        if fbits == 32 {
                            fp::from_int::<f32>(st, x, !unsigned, 0).bits()
                        } else {
                            fp::from_int::<f64>(st, x, !unsigned, 0).bits()
                        }
                    });
                    self.vreg[rd as usize] = r as u128;
                }
                Ok(())
            },
        }
    }

    fn fp_fixed_conv(&mut self, insn: u32) -> Res<()> {
        let sf = bit(insn, 31);
        let ty = field(insn, 22, 2);
        let rmode = field(insn, 19, 2);
        let opcode = field(insn, 16, 3);
        let scale = field(insn, 10, 6);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        if ty > 1 || (!sf && scale < 32) {
            return Err(Event::undefined());
        }
        let fbits = 64 - scale;
        let ibits = if sf { 64 } else { 32 };
        match (rmode, opcode) {
            (3, 0) | (3, 1) => {
                let unsigned = opcode == 1;
                let a = self.vr(rn) as u64;
                let r = self.with_fp(|st| {
                    if ty == 0 {
                        fp::to_int(st, f32::from_bits64(a), ibits, unsigned, fbits, Round::Zero)
                    } else {
                        fp::to_int(st, f64::from_bits64(a), ibits, unsigned, fbits, Round::Zero)
                    }
                });
                self.wr(rd, r);
            },
            (0, 2) | (0, 3) => {
                let unsigned = opcode == 3;
                let mut x = self.xr(rn) & ones(ibits);
                if !unsigned && !sf {
                    x = x as u32 as i32 as i64 as u64;
                }
                let r = self.with_fp(|st| {
                    if ty == 0 {
                        fp::from_int::<f32>(st, x, !unsigned, fbits).bits()
                    } else {
                        fp::from_int::<f64>(st, x, !unsigned, fbits).bits()
                    }
                });
                self.vreg[rd as usize] = r as u128;
            },
            _ => return Err(Event::undefined()),
        }
        Ok(())
    }
}

/// Two-operand scalar FP ops by FP data-processing (2 source) opcode.
fn fp_bin<F: Float>(st: &mut Fpst, op: u32, a: u64, b: u64) -> u64 {
    let (a, b) = (F::from_bits64(a), F::from_bits64(b));
    match op {
        0 => fp::mul(st, a, b).bits(),
        1 => fp::div(st, a, b).bits(),
        2 => fp::add(st, a, b).bits(),
        3 => fp::sub(st, a, b).bits(),
        4 => fp::maxmin(st, a, b, true).bits(),
        5 => fp::maxmin(st, a, b, false).bits(),
        6 => fp::maxmin_num(st, a, b, true).bits(),
        7 => fp::maxmin_num(st, a, b, false).bits(),
        _ => fp::mul(st, a, b).bits() ^ F::SIGN, // FNMUL negates the result
    }
}

/// One-operand scalar FP ops by FP data-processing (1 source) opcode.
fn fp_un<F: Float>(st: &mut Fpst, op: u32, a: u64) -> Option<u64> {
    let x = F::from_bits64(a & ones(F::BITS));
    Some(match op {
        0 => x.bits(),
        1 => x.bits() & !F::SIGN,
        2 => x.bits() ^ F::SIGN,
        3 => fp::sqrt(st, x).bits(),
        8 => fp::round_int(st, x, Round::Nearest, false).bits(),
        9 => fp::round_int(st, x, Round::PlusInf, false).bits(),
        10 => fp::round_int(st, x, Round::MinusInf, false).bits(),
        11 => fp::round_int(st, x, Round::Zero, false).bits(),
        12 => fp::round_int(st, x, Round::TiesAway, false).bits(),
        14 => {
            let m = st.rmode();
            fp::round_int(st, x, m, true).bits()
        },
        15 => {
            let m = st.rmode();
            fp::round_int(st, x, m, false).bits()
        },
        _ => return None,
    })
}

/// f64 -> f16 directly (FCVT Hd, Dn) honouring NaN payload rules for doubles.
fn f64_to_f16_direct(st: &mut Fpst, v: f64, ahp: bool) -> u16 {
    fp::f64_to_f16(st, v, ahp)
}

/// VFPExpandImm: 8-bit immediate to a single/double constant.
pub fn vfp_expand_imm(imm8: u64, bits: u32) -> u64 {
    let a = (imm8 >> 7) & 1;
    let b = (imm8 >> 6) & 1;
    let cdefgh = imm8 & 0x3f;
    if bits == 32 {
        let exp = ((1 - b) << 7) | (if b == 1 { 0x7c } else { 0 }) | ((imm8 >> 4) & 3);
        (a << 31) | (exp << 23) | ((imm8 & 0xf) << 19)
    } else {
        let exp = ((1 - b) << 10) | (if b == 1 { 0x3fc } else { 0 }) | ((imm8 >> 4) & 3);
        let _ = cdefgh;
        (a << 63) | (exp << 52) | ((imm8 & 0xf) << 48)
    }
}
