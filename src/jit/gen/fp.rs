//! Scalar floating point, inline, with the architecture's results and flags.
//!
//! The fast path is taken when FPCR asks for nothing unusual (round to nearest, no flush to zero,
//! no default NaN, as Linux runs) and every operand and the result are ordinary numbers: zero or
//! normal, not NaN, infinite or subnormal. Then the host's IEEE-754 arithmetic gives the Arm
//! result bit for bit, and the only exception that can arise is Inexact, which is worked out
//! exactly (products of singles are exact in double precision; sums by TwoSum) and set in FPSR.
//! Anything else (a NaN, an infinity, a subnormal, a result that underflows or rounds into the
//! smallest normal binade inexactly, division by zero, an out-of-range conversion) goes to the
//! interpreter, as every one of these instructions did before. Double-precision arithmetic, whose
//! inexactness needs a fused multiply-add the generated code doesn't have, stays there too.
//!
//! Values are i64 bit patterns in locals and floats only on the stack. Float-to-integer
//! conversions are the saturating forms, and only ever see values in range.

use super::{Gen, DATA, PAGE, T0, T1, T2, T3, T4, VA};
use crate::bits::{bit, field};
use crate::jit::wasm::EMPTY;

/// FPCR.RMode, FZ and DN: the fast path needs them all clear.
const FPCR_UNUSUAL: i64 = 0x03c0_0000;
/// FPSR.IXC
const IXC: i64 = 1 << 4;

impl Gen<'_> {
    /// Push (FPCR & RMode|FZ|DN) != 0 as i32.
    fn fpcr_unusual(&mut self) {
        self.c.i32c(0);
        self.c.i64_load(self.l.fpcr);
        self.c.i64c(FPCR_UNUSUAL);
        self.c.i64_and();
        self.c.i64c(0);
        self.c.i64_ne();
    }

    /// FPSR.IXC |= 1
    fn set_ixc(&mut self) {
        let fpsr = self.l.fpsr;
        self.c.i32c(0);
        self.c.i32c(0);
        self.c.i64_load(fpsr);
        self.c.i64c(IXC);
        self.c.i64_or();
        self.c.i64_store(fpsr);
    }

    /// Push the float in local `t` (its low 32 bits for a single).
    fn fget(&mut self, t: u32, double: bool) {
        self.c.get(t);
        if double {
            self.c.f64_reinterpret_i64();
        } else {
            self.c.i32_wrap_i64();
            self.c.f32_reinterpret_i32();
        }
    }

    /// Pop a float into local `t` as its bits (zero-extended for a single).
    fn fset(&mut self, t: u32, double: bool) {
        if double {
            self.c.i64_reinterpret_f64();
        } else {
            self.c.i32_reinterpret_f32();
            self.c.i64_extend_i32_u();
        }
        self.c.set(t);
    }

    /// Local `t` = the low 32 (single) or 64 (double) bits of Vr.
    fn fload(&mut self, r: u32, double: bool, t: u32) {
        self.vld(r, 0);
        if !double {
            self.c.i64c(0xffff_ffff);
            self.c.i64_and();
        }
        self.c.set(t);
    }

    /// Push i32: the bits in local `t` are not zero or a normal number (NaN, infinity, subnormal).
    fn special(&mut self, t: u32, double: bool) {
        // |x| bits: zero is fine; else it must lie in [smallest normal, infinity)
        let (abs, min_normal, inf) = if double {
            (0x7fff_ffff_ffff_ffffu64 as i64, 1i64 << 52, 0x7ff0_0000_0000_0000i64)
        } else {
            (0x7fff_ffff, 1 << 23, 0x7f80_0000)
        };
        self.c.get(t);
        self.c.i64c(abs);
        self.c.i64_and();
        self.c.i64c(0);
        self.c.i64_ne();
        self.c.get(t);
        self.c.i64c(abs);
        self.c.i64_and();
        self.c.i64c(min_normal);
        self.c.i64_sub();
        self.c.i64c(inf - min_normal);
        self.c.i64_ge_u();
        self.c.i32_and();
    }

    /// Push i32: the bits in local `t` are a zero, or in the smallest normal binade: where an
    /// inexact result may have been tiny before rounding (Arm judges underflow before rounding).
    fn zero_or_least(&mut self, t: u32, double: bool) {
        let (shift, mask) = if double { (52, 0x7ff) } else { (23, 0xff) };
        self.c.get(t);
        self.c.i64c(shift);
        self.c.i64_shr_u();
        self.c.i64c(mask);
        self.c.i64_and();
        self.c.i64c(1);
        self.c.i64_le_u();
    }

    /// Push i32: local `t` (a single's bits) is zero.
    fn is_zero32(&mut self, t: u32) {
        self.c.get(t);
        self.c.i64c(0x7fff_ffff);
        self.c.i64_and();
        self.c.i64_eqz();
    }

    /// `if fallback { interpret } else { fast }`, with `fallback` the i32 already pushed.
    fn fp_split(&mut self, insn: u32, fast: impl FnOnce(&mut Self)) {
        self.c.if_(EMPTY);
        self.interp(insn);
        self.c.else_();
        fast(self);
        self.c.end();
    }

    /// Write the result in local `r` to Vd (single or double), clearing the rest of it, and set
    /// IXC if local `inexact` (i64) is non-zero.
    fn fp_result(&mut self, rd: u32, r: u32, inexact: Option<u32>) {
        self.vst(rd, 0, |g| g.c.get(r));
        self.vst(rd, 1, |g| g.c.i64c(0));
        if let Some(i) = inexact {
            self.c.get(i);
            self.c.i64c(0);
            self.c.i64_ne();
            self.c.if_(EMPTY);
            self.set_ixc();
            self.c.end();
        }
    }

    /// The scalar floating-point instructions translated inline; false for the rest.
    pub(super) fn fp_scalar(&mut self, insn: u32) -> bool {
        let rd = insn & 31;
        let rn = field(insn, 5, 5);
        let rm = field(insn, 16, 5);
        let ty = field(insn, 22, 2);
        let double = ty == 1;
        if ty > 1 && insn & 0x1f00_0000 == 0x1e00_0000 {
            return false; // half precision
        }
        // FMOV / FABS / FNEG (register): bits, never an exception
        if let 0x1e20_4000 | 0x1e20_c000 | 0x1e21_4000 = insn & 0xffbf_fc00 {
            let op = field(insn, 15, 2);
            let sign = if double { i64::MIN } else { 0x8000_0000 };
            self.fload(rn, double, T0);
            self.c.get(T0);
            match op {
                1 => {
                    self.c.i64c(!sign & if double { -1 } else { 0xffff_ffff });
                    self.c.i64_and();
                },
                2 => {
                    self.c.i64c(sign);
                    self.c.i64_xor();
                },
                _ => {},
            }
            self.c.set(T0);
            self.fp_result(rd, T0, None);
            return true;
        }
        // FMOV (immediate)
        if insn & 0xffa0_1fe0 == 0x1e20_1000 {
            let v = crate::simd::vfp_expand_imm(field(insn, 13, 8) as u64, if double { 64 } else { 32 });
            self.c.i64c(v as i64);
            self.c.set(T0);
            self.fp_result(rd, T0, None);
            return true;
        }
        // FCSEL: bits, chosen by a condition
        if insn & 0xffa0_0c00 == 0x1e20_0c00 {
            self.fload(rn, double, T0);
            self.fload(rm, double, T1);
            self.c.get(T0);
            self.c.get(T1);
            self.cond(field(insn, 12, 4));
            self.c.select();
            self.c.set(T0);
            self.fp_result(rd, T0, None);
            return true;
        }
        // FCMP / FCMPE (and with #0.0): NZCV from an exact comparison; a NaN is the interpreter's
        if insn & 0xffa0_fc07 == 0x1e20_2000 {
            let zero = bit(insn, 3);
            self.fload(rn, double, T0);
            if zero {
                self.c.i64c(0);
                self.c.set(T1);
            } else {
                self.fload(rm, double, T1);
            }
            let ne = |g: &mut Self, t: u32| {
                g.fget(t, double);
                g.fget(t, double);
                if double {
                    g.c.f64_ne();
                } else {
                    g.c.f32_ne();
                }
            };
            ne(self, T0);
            ne(self, T1);
            self.c.i32_or();
            self.fpcr_unusual();
            self.c.i32_or();
            let l = *self.l;
            self.fp_split(insn, |g| {
                let cmp = |g: &mut Self, f32op: fn(&mut crate::jit::wasm::Code), f64op: fn(&mut crate::jit::wasm::Code)| {
                    g.fget(T0, double);
                    g.fget(T1, double);
                    if double {
                        f64op(&mut g.c)
                    } else {
                        f32op(&mut g.c)
                    }
                };
                g.set_flag(l.n, |g| cmp(g, |c| c.f32_lt(), |c| c.f64_lt()));
                g.set_flag(l.z, |g| cmp(g, |c| c.f32_eq(), |c| c.f64_eq()));
                g.set_flag(l.c, |g| cmp(g, |c| c.f32_ge(), |c| c.f64_ge()));
                g.set_flag(l.v, |g| g.c.i32c(0));
            });
            return true;
        }
        if insn & 0x7f20_fc00 == 0x1e20_0000 {
            return self.fp_int_conv(insn, rd, rn, double);
        }
        if double {
            // Double precision: conversion to single only
            if insn & 0xffff_fc00 == 0x1e62_4000 {
                return self.fp_narrow(insn, rd, rn);
            }
            return false;
        }
        match insn & 0xffff_fc00 {
            0x1e21_c000 => return self.fp_sqrt(insn, rd, rn),
            0x1e22_c000 => {
                // FCVT Dd, Sn: exact
                self.fload(rn, false, T0);
                self.special(T0, false);
                self.fpcr_unusual();
                self.c.i32_or();
                self.fp_split(insn, |g| {
                    g.fget(T0, false);
                    g.c.f64_promote_f32();
                    g.fset(T1, true);
                    g.fp_result(rd, T1, None);
                });
                return true;
            },
            _ => {},
        }
        if insn & 0xffe0_0c00 == 0x1e20_0800 {
            return self.fp_arith(insn, rd, rn, rm);
        }
        if insn & 0xffc0_0000 == 0x1f00_0000 {
            return self.fp_muladd(insn, rd, rn, rm);
        }
        false
    }

    /// FMUL / FNMUL / FDIV / FADD / FSUB (single).
    fn fp_arith(&mut self, insn: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let op = field(insn, 12, 4);
        if !matches!(op, 0 | 1 | 2 | 3 | 8) {
            return false;
        }
        self.fload(rn, false, T0);
        self.fload(rm, false, T1);
        match op {
            0 | 8 => {
                // The product of two singles is exact as a double: rounded once, to single
                self.fget(T0, false);
                self.c.f64_promote_f32();
                self.fget(T1, false);
                self.c.f64_promote_f32();
                self.c.f64_mul();
                self.fset(T2, true);
                self.fget(T2, true);
                self.c.f32_demote_f64();
                self.fset(T3, false);
                self.fget(T3, false);
                self.c.f64_promote_f32();
                self.fget(T2, true);
                self.c.f64_ne();
            },
            1 => {
                // q = a / b is exact iff q * b == a, and q * b is exact as a double
                self.fget(T0, false);
                self.fget(T1, false);
                self.c.f32_div();
                self.fset(T3, false);
                self.fget(T3, false);
                self.c.f64_promote_f32();
                self.fget(T1, false);
                self.c.f64_promote_f32();
                self.c.f64_mul();
                self.fget(T0, false);
                self.c.f64_promote_f32();
                self.c.f64_ne();
            },
            _ => {
                // s = a + b' with TwoSum's exact error: (a - (s - bb)) + (b' - bb), bb = s - a
                let sub = op == 3;
                let b = |g: &mut Self| {
                    g.fget(T1, false);
                    if sub {
                        g.c.f32_neg();
                    }
                };
                self.fget(T0, false);
                b(self);
                self.c.f32_add();
                self.fset(T3, false);
                self.fget(T3, false);
                self.fget(T0, false);
                self.c.f32_sub();
                self.fset(T2, false); // bb
                self.fget(T0, false);
                self.fget(T3, false);
                self.fget(T2, false);
                self.c.f32_sub();
                self.c.f32_sub();
                b(self);
                self.fget(T2, false);
                self.c.f32_sub();
                self.c.f32_add();
                self.c.i32c(0);
                self.c.f32_reinterpret_i32();
                self.c.f32_ne();
            },
        }
        self.c.i64_extend_i32_u();
        self.c.set(T4); // inexact
        if op == 8 {
            // FNMUL: the product, negated (exactly)
            self.c.get(T3);
            self.c.i64c(0x8000_0000);
            self.c.i64_xor();
            self.c.set(T3);
        }
        self.fpcr_unusual();
        self.special(T0, false);
        self.c.i32_or();
        self.special(T1, false);
        self.c.i32_or();
        self.special(T3, false);
        self.c.i32_or();
        self.c.get(T4);
        self.c.i32_wrap_i64();
        self.zero_or_least(T3, false);
        self.c.i32_and();
        self.c.i32_or();
        if op == 1 {
            self.is_zero32(T1); // division by zero
            self.c.i32_or();
        }
        self.fp_split(insn, |g| g.fp_result(rd, T3, Some(T4)));
        true
    }

    /// FSQRT (single): r is exact iff r * r == a, and r * r is exact as a double.
    fn fp_sqrt(&mut self, insn: u32, rd: u32, rn: u32) -> bool {
        self.fload(rn, false, T0);
        self.fget(T0, false);
        self.c.f32_sqrt();
        self.fset(T3, false);
        self.fget(T3, false);
        self.c.f64_promote_f32();
        self.fget(T3, false);
        self.c.f64_promote_f32();
        self.c.f64_mul();
        self.fget(T0, false);
        self.c.f64_promote_f32();
        self.c.f64_ne();
        self.c.i64_extend_i32_u();
        self.c.set(T4);
        self.fpcr_unusual();
        self.special(T0, false);
        self.c.i32_or();
        self.special(T3, false); // a negative operand gives a NaN
        self.c.i32_or();
        self.fp_split(insn, |g| g.fp_result(rd, T3, Some(T4)));
        true
    }

    /// FCVT Sd, Dn: rounded once; exact iff it widens back to the same double.
    fn fp_narrow(&mut self, insn: u32, rd: u32, rn: u32) -> bool {
        self.fload(rn, true, T0);
        self.fget(T0, true);
        self.c.f32_demote_f64();
        self.fset(T3, false);
        self.fget(T3, false);
        self.c.f64_promote_f32();
        self.fget(T0, true);
        self.c.f64_ne();
        self.c.i64_extend_i32_u();
        self.c.set(T4);
        self.fpcr_unusual();
        self.special(T0, true);
        self.c.i32_or();
        self.special(T3, false);
        self.c.i32_or();
        self.c.get(T4);
        self.c.i32_wrap_i64();
        self.zero_or_least(T3, false);
        self.c.i32_and();
        self.c.i32_or();
        self.fp_split(insn, |g| g.fp_result(rd, T3, Some(T4)));
        true
    }

    /// FMADD / FMSUB / FNMADD / FNMSUB (single): a' + n'*m with one rounding. The product is
    /// exact as a double; the sum s, rounded to double, carries TwoSum's exact error e. Rounding
    /// s to single is the single rounding of the exact sum unless e != 0 and s lies exactly on a
    /// midpoint between singles (no midpoint can lie strictly between s and the exact sum, as
    /// midpoints are doubles); that case is the interpreter's.
    fn fp_muladd(&mut self, insn: u32, rd: u32, rn: u32, rm: u32) -> bool {
        let ra = field(insn, 10, 5);
        let o1 = bit(insn, 21);
        let o0 = bit(insn, 15);
        let (neg_a, neg_n) = (o1, o0 != o1);
        self.fload(rn, false, T0);
        self.fload(rm, false, T1);
        self.fload(ra, false, T2);
        let a = |g: &mut Self| {
            g.fget(T2, false);
            g.c.f64_promote_f32();
            if neg_a {
                g.c.f64_neg();
            }
        };
        // p (exact)
        self.fget(T0, false);
        self.c.f64_promote_f32();
        if neg_n {
            self.c.f64_neg();
        }
        self.fget(T1, false);
        self.c.f64_promote_f32();
        self.c.f64_mul();
        self.fset(PAGE, true);
        // s = a' + p
        a(self);
        self.fget(PAGE, true);
        self.c.f64_add();
        self.fset(T3, true);
        // e = (a' - (s - bb)) + (p - bb), bb = s - a'
        self.fget(T3, true);
        a(self);
        self.c.f64_sub();
        self.fset(VA, true); // bb
        a(self);
        self.fget(T3, true);
        self.fget(VA, true);
        self.c.f64_sub();
        self.c.f64_sub();
        self.fget(PAGE, true);
        self.fget(VA, true);
        self.c.f64_sub();
        self.c.f64_add();
        self.c.i64c(0);
        self.c.f64_reinterpret_i64();
        self.c.f64_ne();
        self.c.i64_extend_i32_u();
        self.c.set(T4); // e != 0
        // r = s rounded to single
        self.fget(T3, true);
        self.c.f32_demote_f64();
        self.fset(DATA, false);
        // inexact = e != 0 || r != s
        self.fget(DATA, false);
        self.c.f64_promote_f32();
        self.fget(T3, true);
        self.c.f64_ne();
        self.c.i64_extend_i32_u();
        self.c.get(T4);
        self.c.i64_or();
        self.c.set(VA);
        // to the interpreter: FPCR, special operands or result, a possibly tiny inexact result,
        // or s on a midpoint with e != 0
        self.fpcr_unusual();
        for t in [T0, T1, T2, DATA] {
            self.special(t, false);
            self.c.i32_or();
        }
        self.c.get(VA);
        self.c.i32_wrap_i64();
        self.zero_or_least(DATA, false);
        self.c.i32_and();
        self.c.i32_or();
        self.c.get(T4);
        self.c.i32_wrap_i64();
        self.c.get(T3);
        self.c.i64c(0x1fff_ffff);
        self.c.i64_and();
        self.c.i64c(0x1000_0000);
        self.c.i64_eq();
        self.c.i32_and();
        self.c.i32_or();
        self.fp_split(insn, |g| g.fp_result(rd, DATA, Some(VA)));
        true
    }

    /// SCVTF / UCVTF (general register to single or double) and FCVTZS / FCVTZU (single or double
    /// to general register).
    fn fp_int_conv(&mut self, insn: u32, rd: u32, rn: u32, double: bool) -> bool {
        let sf = bit(insn, 31);
        match (field(insn, 19, 2), field(insn, 16, 3)) {
            (0, op @ (2 | 3)) => {
                let signed = op == 2;
                self.xr(rn);
                if !sf {
                    if signed {
                        self.c.i64_extend32_s();
                    } else {
                        self.c.i64c(0xffff_ffff);
                        self.c.i64_and();
                    }
                }
                self.c.set(T0);
                self.int_to_fp(insn, rd, signed, sf, double);
                true
            },
            (3, op @ (0 | 1)) => {
                self.fload(rn, double, T0);
                self.fp_to_int(insn, signed_op(op), sf, double, |g| {
                    g.wr(rd, |g| {
                        g.c.get(T2);
                        g.trunc32(sf);
                    })
                });
                true
            },
            _ => false,
        }
    }

    /// The integer in local T0 (sign- or zero-extended to 64 bits) to Vd. Exact in double
    /// precision when it has at most 53 significant bits, which the fast path requires of a
    /// 64-bit integer; a 32-bit one always has.
    pub(super) fn int_to_fp(&mut self, insn: u32, rd: u32, signed: bool, wide: bool, double: bool) {
        let conv = |g: &mut Self, to_double: bool| {
            g.c.get(T0);
            match (to_double, signed) {
                (false, true) => g.c.f32_convert_i64_s(),
                (false, false) => g.c.f32_convert_i64_u(),
                (true, true) => g.c.f64_convert_i64_s(),
                (true, false) => g.c.f64_convert_i64_u(),
            }
        };
        conv(self, double);
        self.fset(T3, double);
        if double {
            self.c.i64c(0);
        } else {
            self.fget(T3, false);
            self.c.f64_promote_f32();
            conv(self, true);
            self.c.f64_ne();
            self.c.i64_extend_i32_u();
        }
        self.c.set(T4);
        self.fpcr_unusual();
        if wide {
            // more than 53 significant bits: not exact as a double
            self.c.get(T0);
            if signed {
                self.c.i64c(1 << 53);
                self.c.i64_add();
            }
            self.c.i64c(if signed { 1 << 54 } else { 1 << 53 });
            self.c.i64_ge_u();
            self.c.i32_or();
        }
        self.fp_split(insn, |g| g.fp_result(rd, T3, Some(T4)));
    }

    /// The float in local T0 truncated toward zero, into local T2 (then `store`), when it is in
    /// range for the integer (`wide`: 64 bits); NaNs and anything out of range (which saturate and
    /// raise Invalid) are the interpreter's.
    pub(super) fn fp_to_int(&mut self, insn: u32, signed: bool, wide: bool, double: bool, store: impl FnOnce(&mut Self)) {
        // Bounds: lo <= x < hi, all exact as singles and doubles
        let (lo, hi): (f64, f64) = match (signed, wide) {
            (true, false) => (-2147483648.0, 2147483648.0),
            (false, false) => (0.0, 4294967296.0),
            (true, true) => (-9223372036854775808.0, 9223372036854775808.0),
            (false, true) => (0.0, 18446744073709551616.0),
        };
        let konst = |g: &mut Self, v: f64| {
            if double {
                g.c.i64c(v.to_bits() as i64);
                g.c.f64_reinterpret_i64();
            } else {
                g.c.i32c((v as f32).to_bits() as i32);
                g.c.f32_reinterpret_i32();
            }
        };
        self.fget(T0, double);
        konst(self, lo);
        if double {
            self.c.f64_ge();
        } else {
            self.c.f32_ge();
        }
        self.fget(T0, double);
        konst(self, hi);
        if double {
            self.c.f64_lt();
        } else {
            self.c.f32_lt();
        }
        self.c.i32_and();
        self.c.i32_eqz();
        self.fpcr_unusual();
        self.c.i32_or();
        self.fp_split(insn, |g| {
            g.fget(T0, double);
            match (double, signed) {
                (false, true) => g.c.i64_trunc_sat_f32_s(),
                (false, false) => g.c.i64_trunc_sat_f32_u(),
                (true, true) => g.c.i64_trunc_sat_f64_s(),
                (true, false) => g.c.i64_trunc_sat_f64_u(),
            }
            g.c.set(T2);
            store(g);
            // Inexact when the value had a fraction
            g.fget(T0, double);
            if double {
                g.c.f64_trunc();
            } else {
                g.c.f32_trunc();
            }
            g.fget(T0, double);
            if double {
                g.c.f64_ne();
            } else {
                g.c.f32_ne();
            }
            g.c.if_(EMPTY);
            g.set_ixc();
            g.c.end();
        });
    }
}

fn signed_op(op: u32) -> bool {
    op == 0
}
