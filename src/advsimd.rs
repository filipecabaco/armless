//! Advanced SIMD (vector and scalar forms), ARMv8.0 with the Cryptographic Extension, without
//! FP16.

use crate::bits::{bit, field, ones};
use crate::board::Host;
use crate::cpu::{Cpu, Event, Res};
use crate::crypto;
use crate::fp::{self, Float, Fpst, Round};
use crate::simd::{elem, set_elem, vfp_expand_imm};

#[inline(always)]
fn sx(x: u64, esize: u32) -> i128 {
    crate::bits::sext(x, esize) as i128
}

#[inline(always)]
fn ux(x: u64, esize: u32) -> i128 {
    (x & ones(esize)) as i128
}

#[inline(always)]
fn ext(x: u64, esize: u32, unsigned: bool) -> i128 {
    if unsigned {
        ux(x, esize)
    } else {
        sx(x, esize)
    }
}

/// Saturate to `esize` bits; returns (bits, saturated).
fn sat(v: i128, esize: u32, unsigned: bool) -> (u64, bool) {
    let (lo, hi) = if unsigned {
        (0i128, ((1u128 << esize) - 1) as i128)
    } else {
        (-(1i128 << (esize - 1)), (1i128 << (esize - 1)) - 1)
    };
    if v < lo {
        ((lo as u64) & ones(esize), true)
    } else if v > hi {
        ((hi as u64) & ones(esize), true)
    } else {
        ((v as u64) & ones(esize), false)
    }
}

/// Shift a lane value by a signed amount (negative = right) with optional
/// rounding, in i128. Left shifts of `esize` or more produce a value that
/// truncates to 0 and saturates in the direction of the input's sign.
fn shl_signed(v: i128, shift: i64, round: bool, esize: u32) -> i128 {
    if shift >= 0 {
        if shift >= esize as i64 {
            v.signum() << (esize + 1)
        } else {
            v << shift
        }
    } else {
        let s = (-shift).min(127) as u32;
        if round {
            // (v + 2^(s-1)) >> s without overflow
            (v >> (s - 1)).wrapping_add(1) >> 1
        } else {
            v >> s
        }
    }
}

fn poly_mul(a: u64, b: u64, bits: u32) -> u128 {
    let mut r = 0u128;
    for i in 0..bits {
        if (b >> i) & 1 != 0 {
            r ^= (a as u128 & ones(bits) as u128) << i;
        }
    }
    r
}

impl<H: Host> Cpu<H> {
    pub(crate) fn advsimd(&mut self, insn: u32) -> Res<()> {
        let scalar = bit(insn, 28);
        if bit(insn, 31) || (scalar && !bit(insn, 30)) {
            return Err(Event::undefined());
        }
        if bit(insn, 24) {
            // 0x0F / 0x1F groups: modified immediate, shift by immediate, indexed
            if bit(insn, 10) {
                if field(insn, 19, 4) == 0 {
                    if scalar {
                        return Err(Event::undefined());
                    }
                    return self.mod_imm(insn);
                }
                return self.shift_imm(insn, scalar);
            }
            return self.indexed(insn, scalar);
        }
        if bit(insn, 21) {
            if bit(insn, 10) {
                return self.three_same(insn, scalar);
            }
            if field(insn, 10, 2) == 0 {
                return self.three_diff(insn, scalar);
            }
            return match field(insn, 17, 4) {
                0b0000 => self.two_misc(insn, scalar),
                0b1000 => {
                    if scalar {
                        self.scalar_pairwise(insn)
                    } else {
                        self.across(insn)
                    }
                },
                0b0100 if insn & 0xfffe_0c00 == 0x4e28_0800 => self.crypto_aes(insn),
                0b0100 if insn & 0xfffe_0c00 == 0x5e28_0800 => self.crypto_sha2(insn),
                _ => Err(Event::undefined()), // later extensions
            };
        }
        if insn & 0xffe0_8c00 == 0x5e00_0000 {
            return self.crypto_sha3(insn);
        }
        if insn & 0x9fe0_8400 == 0x0e00_0400 || insn & 0xdfe0_8400 == 0x5e00_0400 {
            return self.copy(insn, scalar);
        }
        if scalar {
            return Err(Event::undefined());
        }
        if insn & 0xbf20_8c00 == 0x0e00_0800 {
            return self.permute(insn);
        }
        if insn & 0xbf20_8400 == 0x2e00_0000 {
            return self.ext(insn);
        }
        if insn & 0xbf20_8c00 == 0x0e00_0000 {
            return self.tbl(insn);
        }
        Err(Event::undefined())
    }

    /// AESE, AESD, AESMC, AESIMC.
    fn crypto_aes(&mut self, insn: u32) -> Res<()> {
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let (d, n) = (self.v(rd), self.v(rn));
        self.vreg[rd as usize] = match field(insn, 12, 5) {
            0b00100 => crypto::aese(d, n),
            0b00101 => crypto::aesd(d, n),
            0b00110 => crypto::aesmc(n),
            0b00111 => crypto::aesimc(n),
            _ => return Err(Event::undefined()),
        };
        Ok(())
    }

    /// SHA1H, SHA1SU1, SHA256SU0.
    fn crypto_sha2(&mut self, insn: u32) -> Res<()> {
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let (d, n) = (self.v(rd), self.v(rn));
        self.vreg[rd as usize] = match field(insn, 12, 5) {
            0b00000 => crypto::sha1h(n as u32) as u128,
            0b00001 => crypto::sha1su1(d, n),
            0b00010 => crypto::sha256su0(d, n),
            _ => return Err(Event::undefined()),
        };
        Ok(())
    }

    /// SHA1C, SHA1P, SHA1M, SHA1SU0, SHA256H, SHA256H2, SHA256SU1.
    fn crypto_sha3(&mut self, insn: u32) -> Res<()> {
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let (d, n, m) = (self.v(rd), self.v(rn), self.v(rm));
        self.vreg[rd as usize] = match field(insn, 12, 3) {
            op @ 0..=2 => crypto::sha1_rounds(op, d, n as u32, m),
            3 => crypto::sha1su0(d, n, m),
            4 => crypto::sha256_hash(d, n, m, true),
            5 => crypto::sha256_hash(n, d, m, false),
            6 => crypto::sha256su1(d, n, m),
            _ => return Err(Event::undefined()),
        };
        Ok(())
    }

    #[inline(always)]
    fn v(&self, r: u32) -> u128 {
        self.vreg[r as usize]
    }

    #[inline(always)]
    fn vset(&mut self, rd: u32, v: u128, q: bool) {
        self.vreg[rd as usize] = if q { v } else { v & u64::MAX as u128 };
    }

    fn fpst_run<R>(&mut self, f: impl FnOnce(&mut Fpst) -> R) -> R {
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

    // ============================================================ three same

    fn three_same(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30) || scalar;
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let opcode = field(insn, 11, 5);
        if opcode >= 0b11000 {
            return self.three_same_fp(insn, scalar);
        }
        let (a, b, d) = (self.v(rn), self.v(rm), self.v(rd));
        // Bitwise ops work on the whole register.
        if opcode == 0b00011 {
            if scalar {
                return Err(Event::undefined());
            }
            let r = match (u, size) {
                (false, 0) => a & b,
                (false, 1) => a & !b,
                (false, 2) => a | b,
                (false, _) => a | !b,
                (true, 0) => a ^ b,
                (true, 1) => (d & a) | (!d & b), // BSL
                (true, 2) => (d & !b) | (a & b), // BIT
                (true, _) => (d & b) | (a & !b), // BIF
            };
            self.vset(rd, r, q);
            return Ok(());
        }
        let esize = 8u32 << size;
        if size == 3 && !q {
            return Err(Event::undefined());
        }
        // Halving, min/max, absolute difference: no 64-bit element forms.
        if size == 3
            && matches!(
                opcode,
                0b00000 | 0b00010 | 0b00100 | 0b01100 | 0b01101 | 0b01110 | 0b01111
            )
        {
            return Err(Event::undefined());
        }
        if scalar {
            let ok = match opcode {
                0b00001 | 0b00101 | 0b01001 | 0b01011 => true,
                0b00110 | 0b00111 | 0b01000 | 0b01010 | 0b10000 | 0b10001 => size == 3,
                0b10110 => size == 1 || size == 2,
                _ => false,
            };
            if !ok {
                return Err(Event::undefined());
            }
        }
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        // Pairwise ops
        if matches!(opcode, 0b10100 | 0b10101 | 0b10111) {
            if opcode == 0b10111 && u {
                return Err(Event::undefined());
            }
            if size == 3 && opcode != 0b10111 {
                return Err(Event::undefined());
            }
            let mut r = 0u128;
            for i in 0..lanes {
                let (src, j) = if i < lanes / 2 { (a, 2 * i) } else { (b, 2 * (i - lanes / 2)) };
                let x = elem(src, j, esize);
                let y = elem(src, j + 1, esize);
                let v = match opcode {
                    0b10111 => x.wrapping_add(y),
                    0b10100 => {
                        if ext(x, esize, u) >= ext(y, esize, u) {
                            x
                        } else {
                            y
                        }
                    },
                    _ => {
                        if ext(x, esize, u) <= ext(y, esize, u) {
                            x
                        } else {
                            y
                        }
                    },
                };
                set_elem(&mut r, i, esize, v);
            }
            self.vset(rd, r, q);
            return Ok(());
        }
        let mut r = 0u128;
        let mut qc = false;
        for i in 0..lanes {
            let x = elem(a, i, esize);
            let y = elem(b, i, esize);
            let dd = elem(d, i, esize);
            let (xs, ys) = (ext(x, esize, u), ext(y, esize, u));
            let v: u64 = match opcode {
                0b00000 => ((xs + ys) >> 1) as u64,
                0b00001 => {
                    let (v, s) = sat(xs + ys, esize, u);
                    qc |= s;
                    v
                },
                0b00010 => ((xs + ys + 1) >> 1) as u64,
                0b00100 => ((xs - ys) >> 1) as u64,
                0b00101 => {
                    let (v, s) = sat(xs - ys, esize, u);
                    qc |= s;
                    v
                },
                0b00110 => {
                    if xs > ys {
                        u64::MAX
                    } else {
                        0
                    }
                },
                0b00111 => {
                    if xs >= ys {
                        u64::MAX
                    } else {
                        0
                    }
                },
                0b01000..=0b01011 => {
                    let shift = (y as u8 as i8) as i64;
                    let round = opcode & 2 != 0;
                    let saturating = opcode & 1 != 0;
                    let wide = shl_signed(xs, shift, round, esize);
                    if saturating {
                        let (v, s) = sat(wide, esize, u);
                        qc |= s;
                        v
                    } else {
                        wide as u64
                    }
                },
                0b01100 => {
                    if xs >= ys {
                        x
                    } else {
                        y
                    }
                },
                0b01101 => {
                    if xs <= ys {
                        x
                    } else {
                        y
                    }
                },
                0b01110 => (xs - ys).unsigned_abs() as u64,
                0b01111 => dd.wrapping_add((xs - ys).unsigned_abs() as u64),
                0b10000 => {
                    if u {
                        x.wrapping_sub(y)
                    } else {
                        x.wrapping_add(y)
                    }
                },
                0b10001 => {
                    let t = if u {
                        x & ones(esize) == y & ones(esize)
                    } else {
                        x & y & ones(esize) != 0
                    };
                    if t {
                        u64::MAX
                    } else {
                        0
                    }
                },
                0b10010 => {
                    if size == 3 {
                        return Err(Event::undefined());
                    }
                    let p = x.wrapping_mul(y);
                    if u {
                        dd.wrapping_sub(p)
                    } else {
                        dd.wrapping_add(p)
                    }
                },
                0b10011 => {
                    if size == 3 || (u && size != 0) {
                        return Err(Event::undefined());
                    }
                    if u {
                        poly_mul(x, y, 8) as u64
                    } else {
                        x.wrapping_mul(y)
                    }
                },
                0b10110 => {
                    if size == 0 || size == 3 {
                        return Err(Event::undefined());
                    }
                    let (xs, ys) = (sx(x, esize), sx(y, esize));
                    let round = if u { 1i128 << (esize - 1) } else { 0 };
                    let (v, s) = sat((2 * xs * ys + round) >> esize, esize, false);
                    qc |= s;
                    v
                },
                _ => return Err(Event::undefined()),
            };
            set_elem(&mut r, i, esize, v);
        }
        if qc {
            self.sys.fpsr |= fp::QC;
        }
        self.vset(rd, r, q);
        Ok(())
    }

    fn three_same_fp(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let a_bit = bit(insn, 23);
        let sz = bit(insn, 22);
        let esize = if sz { 64 } else { 32 };
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let opcode = field(insn, 11, 5);
        if !scalar && sz && !q {
            return Err(Event::undefined());
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Op {
            MaxNm,
            MinNm,
            MaxNmP,
            MinNmP,
            Fmla,
            Fmls,
            Add,
            Sub,
            AddP,
            Abd,
            Mulx,
            Mul,
            Cmeq,
            Cmge,
            Cmgt,
            Acge,
            Acgt,
            Max,
            Min,
            MaxP,
            MinP,
            Recps,
            Rsqrts,
            Div,
        }
        use Op::*;
        let op = match (opcode, u, a_bit) {
            (0b11000, false, false) => MaxNm,
            (0b11000, false, true) => MinNm,
            (0b11000, true, false) => MaxNmP,
            (0b11000, true, true) => MinNmP,
            (0b11001, false, false) => Fmla,
            (0b11001, false, true) => Fmls,
            (0b11010, false, false) => Add,
            (0b11010, false, true) => Sub,
            (0b11010, true, false) => AddP,
            (0b11010, true, true) => Abd,
            (0b11011, false, false) => Mulx,
            (0b11011, true, false) => Mul,
            (0b11100, false, false) => Cmeq,
            (0b11100, true, false) => Cmge,
            (0b11100, true, true) => Cmgt,
            (0b11101, true, false) => Acge,
            (0b11101, true, true) => Acgt,
            (0b11110, false, false) => Max,
            (0b11110, false, true) => Min,
            (0b11110, true, false) => MaxP,
            (0b11110, true, true) => MinP,
            (0b11111, false, false) => Recps,
            (0b11111, false, true) => Rsqrts,
            (0b11111, true, false) => Div,
            _ => return Err(Event::undefined()),
        };
        if scalar
            && !matches!(
                op,
                Mulx | Cmeq | Cmge | Cmgt | Acge | Acgt | Recps | Rsqrts | Abd
            )
        {
            return Err(Event::undefined());
        }
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        let (a, b, d) = (self.v(rn), self.v(rm), self.v(rd));
        let pairwise = matches!(op, MaxNmP | MinNmP | AddP | MaxP | MinP);
        let r = self.fpst_run(|st| {
            let mut r = 0u128;
            for i in 0..lanes {
                let (x, y) = if pairwise {
                    let (src, j) =
                        if i < lanes / 2 { (a, 2 * i) } else { (b, 2 * (i - lanes / 2)) };
                    (elem(src, j, esize), elem(src, j + 1, esize))
                } else {
                    (elem(a, i, esize), elem(b, i, esize))
                };
                let dd = elem(d, i, esize);
                let v = if esize == 32 {
                    fp3::<f32>(st, op as u32, x, y, dd)
                } else {
                    fp3::<f64>(st, op as u32, x, y, dd)
                };
                set_elem(&mut r, i, esize, v);
            }
            r
        });
        self.vset(rd, r, q || scalar);
        if scalar {
            self.vreg[rd as usize] &= ones(esize) as u128;
        }
        Ok(())
    }

    // ======================================================= three different

    fn three_diff(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let opcode = field(insn, 12, 4);
        if size == 3 {
            if opcode != 0b1110 || u || scalar {
                return Err(Event::undefined());
            }
            // PMULL/PMULL2 (1Q), from the Cryptographic Extension
            let half = if q { 64 } else { 0 };
            let (a, b) = ((self.v(rn) >> half) as u64, (self.v(rm) >> half) as u64);
            self.vreg[rd as usize] = poly_mul(a, b, 64);
            return Ok(());
        }
        let esize = 8u32 << size;
        let wsize = esize * 2;
        if scalar && !(matches!(opcode, 0b1001 | 0b1011 | 0b1101) && !u) {
            return Err(Event::undefined());
        }
        if matches!(opcode, 0b1001 | 0b1011 | 0b1101) && (u || size == 0) {
            return Err(Event::undefined());
        }
        if opcode == 0b1110 && (u || size != 0) {
            return Err(Event::undefined());
        }
        if opcode == 0b1111 {
            return Err(Event::undefined());
        }
        let (a, b, d) = (self.v(rn), self.v(rm), self.v(rd));
        let half = if scalar {
            0
        } else if q {
            64
        } else {
            0
        };
        let lanes = if scalar { 1 } else { 64 / esize };
        let mut qc = false;
        let narrowing = matches!(opcode, 0b0100 | 0b0110);
        let mut r = if narrowing {
            if q {
                d & u64::MAX as u128
            } else {
                0
            }
        } else {
            0u128
        };
        for i in 0..lanes {
            let x = elem(a >> half, i, esize);
            let y = elem(b >> half, i, esize);
            let xw = elem(a, i, wsize);
            let dw = elem(d, i, wsize);
            let (xs, ys) = (ext(x, esize, u), ext(y, esize, u));
            let v: u64 = match opcode {
                0b0000 => (xs + ys) as u64,
                0b0001 => (ext(xw, wsize, u) + ys) as u64,
                0b0010 => (xs - ys) as u64,
                0b0011 => (ext(xw, wsize, u) - ys) as u64,
                0b0100 | 0b0110 => {
                    let (aw, bw) = (elem(a, i, wsize) as u128, elem(b, i, wsize) as u128);
                    let sum =
                        if opcode == 0b0100 { aw.wrapping_add(bw) } else { aw.wrapping_sub(bw) };
                    let round = if u { 1u128 << (esize - 1) } else { 0 };
                    let n = (sum.wrapping_add(round) >> esize) as u64;
                    set_elem(&mut r, i + if q { 64 / esize } else { 0 }, esize, n);
                    continue;
                },
                0b0101 => dw.wrapping_add((xs - ys).unsigned_abs() as u64),
                0b0111 => (xs - ys).unsigned_abs() as u64,
                0b1000 => dw.wrapping_add((xs * ys) as u64),
                0b1010 => dw.wrapping_sub((xs * ys) as u64),
                0b1100 => (xs * ys) as u64,
                0b1001 | 0b1011 | 0b1101 => {
                    let (p, s1) = sat(2 * xs * ys, wsize, false);
                    qc |= s1;
                    if opcode == 0b1101 {
                        p
                    } else {
                        let acc = sx(dw, wsize);
                        let pv = sx(p, wsize);
                        let (v, s2) = sat(
                            if opcode == 0b1001 { acc + pv } else { acc - pv },
                            wsize,
                            false,
                        );
                        qc |= s2;
                        v
                    }
                },
                0b1110 => poly_mul(x, y, 8) as u64,
                _ => return Err(Event::undefined()),
            };
            set_elem(&mut r, i, wsize, v);
        }
        if qc {
            self.sys.fpsr |= fp::QC;
        }
        if scalar {
            self.vreg[rd as usize] = r & ones(wsize) as u128;
        } else if narrowing {
            self.vset(rd, r, q);
        } else {
            self.vreg[rd as usize] = r;
        }
        Ok(())
    }

    // ============================================================= two-reg misc

    fn two_misc(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let opcode = field(insn, 12, 5);
        let a = self.v(rn);
        let d = self.v(rd);
        let esize = 8u32 << size;
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        // FP forms: opcode 01100..01111 with size<1>=1, and 1xxxx (except narrowing ones)
        let fp_form = (opcode >= 0b11000) || (opcode & 0b11100 == 0b01100 && size & 2 != 0);
        if matches!(opcode, 0b10110 | 0b10111) {
            return self.fcvt_narrow_long(insn, scalar);
        }
        if fp_form {
            return self.two_misc_fp(insn, scalar);
        }
        let mut qc = false;
        let mut r = 0u128;
        match opcode {
            0b00000 | 0b00001 => {
                if scalar {
                    return Err(Event::undefined());
                }
                // REV64 / REV32 / REV16: container = 64 >> (u + 2*op0) bits
                let container = match (opcode, u) {
                    (0, false) => 64,
                    (0, true) => 32,
                    (1, false) => 16,
                    _ => return Err(Event::undefined()),
                };
                if esize >= container {
                    return Err(Event::undefined());
                }
                let per = container / esize;
                for i in 0..lanes {
                    let base = i / per * per;
                    let src = base + (per - 1 - i % per);
                    set_elem(&mut r, i, esize, elem(a, src, esize));
                }
            },
            0b00010 | 0b00110 => {
                // SADDLP/UADDLP, SADALP/UADALP
                if scalar || size == 3 {
                    return Err(Event::undefined());
                }
                let w = esize * 2;
                for i in 0..lanes / 2 {
                    let s = ext(elem(a, 2 * i, esize), esize, u)
                        + ext(elem(a, 2 * i + 1, esize), esize, u);
                    let acc = if opcode == 0b00110 { elem(d, i, w) as i128 } else { 0 };
                    set_elem(&mut r, i, w, (s + acc) as u64);
                }
            },
            0b00011 => {
                // SUQADD / USQADD
                if size == 3 && !q && !scalar {
                    return Err(Event::undefined());
                }
                for i in 0..lanes {
                    let x = elem(a, i, esize);
                    let dd = elem(d, i, esize);
                    let (v, s) = if u {
                        sat(ux(dd, esize) + sx(x, esize), esize, true)
                    } else {
                        sat(sx(dd, esize) + ux(x, esize), esize, false)
                    };
                    qc |= s;
                    set_elem(&mut r, i, esize, v);
                }
            },
            0b00100 => {
                // CLS / CLZ
                if scalar || size == 3 {
                    return Err(Event::undefined());
                }
                for i in 0..lanes {
                    let x = elem(a, i, esize);
                    let v = if u {
                        (x << (64 - esize)).leading_zeros().min(esize) as u64
                    } else {
                        // leading sign bits, not counting the sign bit itself
                        let s = sx(x, esize) as i64;
                        let t = (s ^ (s >> 1)) as u64;
                        (t << (64 - esize)).leading_zeros().min(esize) as u64 - 1
                    };
                    set_elem(&mut r, i, esize, v);
                }
            },
            0b00101 => {
                if scalar {
                    return Err(Event::undefined());
                }
                match (u, size) {
                    (false, 0) => {
                        for i in 0..lanes {
                            set_elem(&mut r, i, 8, elem(a, i, 8).count_ones() as u64);
                        }
                    },
                    (true, 0) => r = !a,
                    (true, 1) => {
                        let lanes8 = if q { 16 } else { 8 };
                        for i in 0..lanes8 {
                            set_elem(&mut r, i, 8, (elem(a, i, 8) as u8).reverse_bits() as u64);
                        }
                    },
                    _ => return Err(Event::undefined()),
                }
            },
            0b00111 | 0b01011 => {
                // SQABS/SQNEG, ABS/NEG
                if size == 3 && !q && !scalar {
                    return Err(Event::undefined());
                }
                if opcode == 0b01011 && scalar && size != 3 {
                    return Err(Event::undefined());
                }
                for i in 0..lanes {
                    let x = sx(elem(a, i, esize), esize);
                    let v = if u { -x } else { x.abs() };
                    let out = if opcode == 0b00111 {
                        let (o, s) = sat(v, esize, false);
                        qc |= s;
                        o
                    } else {
                        v as u64
                    };
                    set_elem(&mut r, i, esize, out);
                }
            },
            0b01000 | 0b01001 | 0b01010 => {
                // CMGT/CMGE #0, CMEQ/CMLE #0, CMLT #0
                if (scalar && size != 3) || (size == 3 && !q && !scalar) || (opcode == 0b01010 && u)
                {
                    return Err(Event::undefined());
                }
                for i in 0..lanes {
                    let x = sx(elem(a, i, esize), esize);
                    let t = match (opcode, u) {
                        (0b01000, false) => x > 0,
                        (0b01000, true) => x >= 0,
                        (0b01001, false) => x == 0,
                        (0b01001, true) => x <= 0,
                        _ => x < 0,
                    };
                    set_elem(&mut r, i, esize, if t { u64::MAX } else { 0 });
                }
            },
            0b10010 | 0b10100 => {
                // XTN/SQXTN/UQXTN/SQXTUN (narrow into lower or upper half)
                if size == 3 {
                    return Err(Event::undefined());
                }
                if opcode == 0b10010 && !u && scalar {
                    return Err(Event::undefined()); // no scalar XTN
                }
                let w = esize * 2;
                let n = if scalar { 1 } else { 64 / esize };
                let base = if q && !scalar { 64 / esize } else { 0 };
                r = if q && !scalar { d & u64::MAX as u128 } else { 0 };
                for i in 0..n {
                    let x = elem(a, i, w);
                    let v = match (opcode, u) {
                        (0b10010, false) => x & ones(esize),
                        (0b10010, true) => {
                            let (v, s) = sat(sx(x, w), esize, true);
                            qc |= s;
                            v
                        },
                        (_, false) => {
                            let (v, s) = sat(sx(x, w), esize, false);
                            qc |= s;
                            v
                        },
                        (_, true) => {
                            let (v, s) = sat(ux(x, w), esize, true);
                            qc |= s;
                            v
                        },
                    };
                    set_elem(&mut r, base + i, esize, v);
                }
                if qc {
                    self.sys.fpsr |= fp::QC;
                }
                if scalar {
                    self.vreg[rd as usize] = r & ones(esize) as u128;
                } else {
                    self.vset(rd, r, q);
                }
                return Ok(());
            },
            0b10011 => {
                // SHLL/SHLL2
                if !u || scalar || size == 3 {
                    return Err(Event::undefined());
                }
                let w = esize * 2;
                let half = if q { 64 } else { 0 };
                for i in 0..64 / esize {
                    set_elem(&mut r, i, w, elem(a >> half, i, esize) << esize);
                }
                self.vreg[rd as usize] = r;
                return Ok(());
            },
            _ => return Err(Event::undefined()),
        }
        if qc {
            self.sys.fpsr |= fp::QC;
        }
        if scalar {
            self.vreg[rd as usize] = r & ones(esize) as u128;
        } else {
            self.vset(rd, r, q);
        }
        Ok(())
    }

    fn two_misc_fp(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let a_bit = bit(insn, 23);
        let sz = bit(insn, 22);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let opcode = field(insn, 12, 5);
        let esize = if sz { 64 } else { 32 };
        if sz && !q && !scalar {
            return Err(Event::undefined());
        }
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        #[derive(Clone, Copy)]
        enum M {
            Rint(Round, bool),
            RintCur(bool),
            ToInt(Round, bool),
            FromInt(bool),
            Cmp(u32),
            Abs,
            Neg,
            Sqrt,
            Recpe,
            Rsqrte,
            Urecpe,
            Ursqrte,
            Recpx,
        }
        use M::*;
        let m = match (u, a_bit, opcode) {
            (false, false, 0b11000) => Rint(Round::Nearest, false),
            (false, false, 0b11001) => Rint(Round::MinusInf, false),
            (false, false, 0b11010) => ToInt(Round::Nearest, false),
            (false, false, 0b11011) => ToInt(Round::MinusInf, false),
            (false, false, 0b11100) => ToInt(Round::TiesAway, false),
            (false, false, 0b11101) => FromInt(false),
            (false, true, 0b01100) => Cmp(0), // GT 0
            (false, true, 0b01101) => Cmp(1), // EQ 0
            (false, true, 0b01110) => Cmp(2), // LT 0
            (false, true, 0b01111) => Abs,
            (false, true, 0b11000) => Rint(Round::PlusInf, false),
            (false, true, 0b11001) => Rint(Round::Zero, false),
            (false, true, 0b11010) => ToInt(Round::PlusInf, false),
            (false, true, 0b11011) => ToInt(Round::Zero, false),
            (false, true, 0b11100) => Urecpe,
            (false, true, 0b11101) => Recpe,
            (false, true, 0b11111) => Recpx,
            (true, false, 0b11000) => Rint(Round::TiesAway, false),
            (true, false, 0b11001) => RintCur(true),
            (true, false, 0b11010) => ToInt(Round::Nearest, true),
            (true, false, 0b11011) => ToInt(Round::MinusInf, true),
            (true, false, 0b11100) => ToInt(Round::TiesAway, true),
            (true, false, 0b11101) => FromInt(true),
            (true, true, 0b01100) => Cmp(3), // GE 0
            (true, true, 0b01101) => Cmp(4), // LE 0
            (true, true, 0b01111) => Neg,
            (true, true, 0b11001) => RintCur(false),
            (true, true, 0b11010) => ToInt(Round::PlusInf, true),
            (true, true, 0b11011) => ToInt(Round::Zero, true),
            (true, true, 0b11100) => Ursqrte,
            (true, true, 0b11101) => Rsqrte,
            (true, true, 0b11111) => Sqrt,
            _ => return Err(Event::undefined()),
        };
        // Scalar availability
        if scalar
            && matches!(
                m,
                Rint(..) | RintCur(_) | Abs | Neg | Sqrt | Urecpe | Ursqrte
            )
        {
            return Err(Event::undefined());
        }
        if !scalar && matches!(m, Recpx) {
            return Err(Event::undefined());
        }
        if matches!(m, Urecpe | Ursqrte) && sz {
            return Err(Event::undefined());
        }
        let a = self.v(rn);
        let r = self.fpst_run(|st| {
            let mut r = 0u128;
            for i in 0..lanes {
                let x = elem(a, i, esize);
                let v = match m {
                    Urecpe | Ursqrte => {
                        let x = x as u32;
                        (if matches!(m, Urecpe) { urecpe(x) } else { ursqrte(x) }) as u64
                    },
                    _ => {
                        if esize == 32 {
                            fp_misc::<f32>(st, m_code(&m), x)
                        } else {
                            fp_misc::<f64>(st, m_code(&m), x)
                        }
                    },
                };
                set_elem(&mut r, i, esize, v);
            }
            r
        });
        fn m_code(m: &M) -> MiscOp {
            match *m {
                M::Rint(r, x) => MiscOp::Rint(r, x),
                M::RintCur(x) => MiscOp::RintCur(x),
                M::ToInt(r, u) => MiscOp::ToInt(r, u),
                M::FromInt(u) => MiscOp::FromInt(u),
                M::Cmp(c) => MiscOp::Cmp(c),
                M::Abs => MiscOp::Abs,
                M::Neg => MiscOp::Neg,
                M::Sqrt => MiscOp::Sqrt,
                M::Recpe => MiscOp::Recpe,
                M::Rsqrte => MiscOp::Rsqrte,
                M::Recpx => MiscOp::Recpx,
                _ => MiscOp::Abs,
            }
        }
        if scalar {
            self.vreg[rd as usize] = r & ones(esize) as u128;
        } else {
            self.vset(rd, r, q);
        }
        Ok(())
    }

    /// FCVTN/FCVTN2, FCVTXN/FCVTXN2, FCVTL/FCVTL2.
    fn fcvt_narrow_long(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let sz = size & 1 != 0;
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let opcode = field(insn, 12, 5);
        if size & 2 != 0 {
            return Err(Event::undefined());
        }
        let a = self.v(rn);
        let d = self.v(rd);
        let ahp = self.sys.fpcr & (1 << 26) != 0;
        if opcode == 0b10110 {
            // narrowing: FCVTN (u=0) or FCVTXN (u=1, sz must be 1)
            if u && !sz {
                return Err(Event::undefined());
            }
            if scalar && !u {
                return Err(Event::undefined());
            }
            let (wsize, nsize) = if sz { (64, 32) } else { (32, 16) };
            let n = if scalar { 1 } else { 64 / nsize };
            let base = if q && !scalar { 64 / nsize } else { 0 };
            let mut r = if q && !scalar { d & u64::MAX as u128 } else { 0 };
            self.fpst_run(|st| {
                for i in 0..n {
                    let x = elem(a, i, wsize);
                    let v = if u {
                        fcvt_round_odd(st, f64::from_bits(x)) as u64
                    } else if sz {
                        fp::f64_to_f32(st, f64::from_bits(x)).to_bits() as u64
                    } else {
                        let f = f32::from_bits(x as u32);
                        let w = fp::f32_to_f64(st, f);
                        fp::f64_to_f16(st, w, ahp) as u64
                    };
                    set_elem(&mut r, base + i, nsize, v);
                }
            });
            if scalar {
                self.vreg[rd as usize] = r & ones(nsize) as u128;
            } else {
                self.vset(rd, r, q);
            }
            return Ok(());
        }
        // FCVTL: widen
        if u || scalar {
            return Err(Event::undefined());
        }
        let (nsize, wsize) = if sz { (32, 64) } else { (16, 32) };
        let half = if q { 64 } else { 0 };
        let mut r = 0u128;
        self.fpst_run(|st| {
            for i in 0..64 / nsize {
                let x = elem(a >> half, i, nsize);
                let v = if sz {
                    fp::f32_to_f64(st, f32::from_bits(x as u32)).to_bits()
                } else {
                    let w = fp::f16_to_f64(st, x as u16, ahp);
                    // exact; NaN payloads are preserved through f64
                    fp::f64_to_f32(st, w).to_bits() as u64
                };
                set_elem(&mut r, i, wsize, v);
            }
        });
        self.vreg[rd as usize] = r;
        Ok(())
    }

    // ============================================================ across lanes

    fn across(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let opcode = field(insn, 12, 5);
        let a = self.v(rn);
        if opcode == 0b01100 || opcode == 0b01111 {
            // FMAXNMV/FMINNMV, FMAXV/FMINV: 4S only in ARMv8.0
            if !u || size & 1 != 0 || !q {
                return Err(Event::undefined());
            }
            let max = size & 2 == 0;
            let num = opcode == 0b01100;
            let r = self.fpst_run(|st| {
                let f = |st: &mut Fpst, x: u64, y: u64| -> u64 {
                    let (x, y) = (f32::from_bits64(x), f32::from_bits64(y));
                    if num {
                        fp::maxmin_num(st, x, y, max).bits()
                    } else {
                        fp::maxmin(st, x, y, max).bits()
                    }
                };
                let lo = f(st, elem(a, 0, 32), elem(a, 1, 32));
                let hi = f(st, elem(a, 2, 32), elem(a, 3, 32));
                f(st, lo, hi)
            });
            self.vreg[rd as usize] = r as u128;
            return Ok(());
        }
        if size == 3 || (size == 2 && !q) {
            return Err(Event::undefined());
        }
        let esize = 8u32 << size;
        let lanes = (if q { 128 } else { 64 }) / esize;
        let vals: Vec<i128> = (0..lanes)
            .map(|i| ext(elem(a, i, esize), esize, u))
            .collect();
        let (v, osize) = match (opcode, u) {
            (0b00011, _) => (vals.iter().sum::<i128>(), esize * 2),
            (0b01010, _) => (*vals.iter().max().unwrap(), esize),
            (0b11010, _) => (*vals.iter().min().unwrap(), esize),
            (0b11011, false) => (vals.iter().sum::<i128>(), esize),
            _ => return Err(Event::undefined()),
        };
        self.vreg[rd as usize] = ((v as u64) & ones(osize)) as u128;
        Ok(())
    }

    fn scalar_pairwise(&mut self, insn: u32) -> Res<()> {
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let opcode = field(insn, 12, 5);
        let a = self.v(rn);
        if !u {
            if opcode != 0b11011 || size != 3 {
                return Err(Event::undefined());
            }
            self.vreg[rd as usize] = (elem(a, 0, 64).wrapping_add(elem(a, 1, 64))) as u128;
            return Ok(());
        }
        let sz = size & 1 != 0;
        let a_bit = size & 2 != 0;
        let esize = if sz { 64 } else { 32 };
        let (x, y) = (elem(a, 0, esize), elem(a, 1, esize));
        let code = match (opcode, a_bit) {
            (0b01100, false) => 0, // FMAXNMP
            (0b01100, true) => 1,  // FMINNMP
            (0b01101, false) => 2, // FADDP
            (0b01111, false) => 3, // FMAXP
            (0b01111, true) => 4,  // FMINP
            _ => return Err(Event::undefined()),
        };
        let r = self.fpst_run(|st| {
            if esize == 32 {
                pair_op::<f32>(st, code, x, y)
            } else {
                pair_op::<f64>(st, code, x, y)
            }
        });
        self.vreg[rd as usize] = r as u128;
        Ok(())
    }

    // ==================================================================== copy

    fn copy(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let op = bit(insn, 29);
        let imm5 = field(insn, 16, 5);
        let imm4 = field(insn, 11, 4);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let size = imm5.trailing_zeros();
        if size > 3 {
            return Err(Event::undefined());
        }
        let esize = 8u32 << size;
        let index = imm5 >> (size + 1);
        if scalar {
            if op || imm4 != 0 {
                return Err(Event::undefined());
            }
            self.vreg[rd as usize] = elem(self.v(rn), index, esize) as u128;
            return Ok(());
        }
        if op {
            // INS (element)
            if !q {
                return Err(Event::undefined());
            }
            let src_index = imm4 >> size;
            let v = elem(self.v(rn), src_index, esize);
            let mut d = self.v(rd);
            set_elem(&mut d, index, esize, v);
            self.vreg[rd as usize] = d;
            return Ok(());
        }
        match imm4 {
            0b0000 | 0b0001 => {
                // DUP (element / general)
                if size == 3 && !q {
                    return Err(Event::undefined());
                }
                let v = if imm4 == 0 {
                    elem(self.v(rn), index, esize)
                } else {
                    self.xr(rn) & ones(esize)
                };
                let lanes = (if q { 128 } else { 64 }) / esize;
                let mut r = 0u128;
                for i in 0..lanes {
                    set_elem(&mut r, i, esize, v);
                }
                self.vset(rd, r, q);
            },
            0b0011 => {
                // INS (general)
                if !q {
                    return Err(Event::undefined());
                }
                let mut d = self.v(rd);
                set_elem(&mut d, index, esize, self.xr(rn));
                self.vreg[rd as usize] = d;
            },
            0b0101 => {
                // SMOV
                if (q && size >= 3) || (!q && size >= 2) {
                    return Err(Event::undefined());
                }
                let v = crate::bits::sext(elem(self.v(rn), index, esize), esize) as u64;
                self.wr(rd, if q { v } else { v & 0xffff_ffff });
            },
            0b0111 => {
                // UMOV
                if (q && size != 3) || (!q && size == 3) {
                    return Err(Event::undefined());
                }
                let v = elem(self.v(rn), index, esize);
                self.wr(rd, v);
            },
            _ => return Err(Event::undefined()),
        }
        Ok(())
    }

    // ====================================================== modified immediate

    fn mod_imm(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let op = bit(insn, 29);
        let cmode = field(insn, 12, 4);
        if bit(insn, 11) {
            return Err(Event::undefined());
        }
        let rd = insn & 31;
        let imm8 = ((field(insn, 16, 3) << 5) | field(insn, 5, 5)) as u64;
        let rep = |v: u64, w: u32| -> u64 {
            let mut r = 0u64;
            let mut i = 0;
            while i < 64 {
                r |= v << i;
                i += w;
            }
            r
        };
        let imm64: u64 = match cmode >> 1 {
            0 => rep(imm8, 32),
            1 => rep(imm8 << 8, 32),
            2 => rep(imm8 << 16, 32),
            3 => rep(imm8 << 24, 32),
            4 => rep(imm8, 16),
            5 => rep(imm8 << 8, 16),
            6 => {
                if cmode & 1 == 0 {
                    rep((imm8 << 8) | 0xff, 32)
                } else {
                    rep((imm8 << 16) | 0xffff, 32)
                }
            },
            _ => match (cmode & 1, op) {
                (0, false) => rep(imm8, 8),
                (0, true) => {
                    let mut r = 0u64;
                    for b in 0..8 {
                        if imm8 & (1 << b) != 0 {
                            r |= 0xff << (8 * b);
                        }
                    }
                    r
                },
                (_, false) => rep(vfp_expand_imm(imm8, 32), 32),
                (_, true) => {
                    if !q {
                        return Err(Event::undefined());
                    }
                    vfp_expand_imm(imm8, 64)
                },
            },
        };
        let imm = (imm64 as u128) | (imm64 as u128) << 64;
        let d = self.v(rd);
        let orr_bic = cmode & 1 == 1 && cmode < 0b1100;
        let r = if orr_bic {
            if op {
                d & !imm
            } else {
                d | imm
            }
        } else {
            let mvn = op && cmode != 0b1110 && cmode != 0b1111;
            if mvn {
                !imm
            } else {
                imm
            }
        };
        self.vset(rd, r, q);
        Ok(())
    }

    // ======================================================= shift by immediate

    fn shift_imm(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let immh = field(insn, 19, 4);
        let immb = field(insn, 16, 3);
        let opcode = field(insn, 11, 5);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let hsb = 31 - immh.leading_zeros();
        let esize = 8u32 << hsb;
        let immhb = (immh << 3) | immb;
        let a = self.v(rn);
        let d = self.v(rd);
        let narrowing = matches!(opcode, 0b10000..=0b10011);
        let long = opcode == 0b10100;
        if (narrowing || long) && immh & 8 != 0 {
            return Err(Event::undefined());
        }
        if !scalar && !narrowing && !long && immh & 8 != 0 && !q {
            return Err(Event::undefined());
        }
        // Fixed-point conversions
        if opcode == 0b11100 || opcode == 0b11111 {
            if immh & 0b1100 == 0 {
                return Err(Event::undefined()); // FP16 forms
            }
            let fbits = 2 * esize - immhb;
            let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
            let r = self.fpst_run(|st| {
                let mut r = 0u128;
                for i in 0..lanes {
                    let x = elem(a, i, esize);
                    let v = if opcode == 0b11100 {
                        let xv = if u { x } else { crate::bits::sext(x, esize) as u64 };
                        if esize == 32 {
                            fp::from_int::<f32>(st, xv, !u, fbits).bits()
                        } else {
                            fp::from_int::<f64>(st, xv, !u, fbits).bits()
                        }
                    } else {
                        if esize == 32 {
                            fp::to_int(st, f32::from_bits64(x), esize, u, fbits, Round::Zero)
                        } else {
                            fp::to_int(st, f64::from_bits64(x), esize, u, fbits, Round::Zero)
                        }
                    };
                    set_elem(&mut r, i, esize, v);
                }
                r
            });
            if scalar {
                self.vreg[rd as usize] = r & ones(esize) as u128;
            } else {
                self.vset(rd, r, q);
            }
            return Ok(());
        }
        let rshift = 2 * esize - immhb; // for right shifts: 1..esize
        let lshift = immhb - esize; // for left shifts: 0..esize-1
        let mut qc = false;
        if narrowing {
            // SHRN/RSHRN/SQSHRN/UQSHRN/SQRSHRN/UQRSHRN/SQSHRUN/SQRSHRUN
            let round = opcode & 1 != 0;
            let (src_unsigned, dst_unsigned, saturate) = match (opcode >> 1 & 1, u) {
                (0, false) => (false, false, false), // SHRN/RSHRN
                (0, true) => (false, true, true),    // SQSHRUN/SQRSHRUN
                (_, false) => (false, false, true),  // SQSHRN/SQRSHRN
                (_, true) => (true, true, true),     // UQSHRN/UQRSHRN
            };
            if scalar && !saturate {
                return Err(Event::undefined());
            }
            let w = esize * 2;
            let n = if scalar { 1 } else { 64 / esize };
            let base = if q && !scalar { 64 / esize } else { 0 };
            let mut r = if q && !scalar { d & u64::MAX as u128 } else { 0 };
            for i in 0..n {
                let x = ext(elem(a, i, w), w, src_unsigned);
                let v = shl_signed(x, -(rshift as i64), round, w);
                let out = if saturate {
                    let (o, s) = sat(v, esize, dst_unsigned);
                    qc |= s;
                    o
                } else {
                    (v as u64) & ones(esize)
                };
                set_elem(&mut r, base + i, esize, out);
            }
            if qc {
                self.sys.fpsr |= fp::QC;
            }
            if scalar {
                self.vreg[rd as usize] = r & ones(esize) as u128;
            } else {
                self.vset(rd, r, q);
            }
            return Ok(());
        }
        if long {
            // SSHLL/USHLL
            if scalar {
                return Err(Event::undefined());
            }
            let w = esize * 2;
            let half = if q { 64 } else { 0 };
            let mut r = 0u128;
            for i in 0..64 / esize {
                let x = ext(elem(a >> half, i, esize), esize, u);
                set_elem(&mut r, i, w, (x << lshift) as u64);
            }
            self.vreg[rd as usize] = r;
            return Ok(());
        }
        if scalar && immh & 8 == 0 && !matches!(opcode, 0b01110 | 0b01100) {
            return Err(Event::undefined());
        }
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        let mut r = 0u128;
        for i in 0..lanes {
            let x = elem(a, i, esize);
            let dd = elem(d, i, esize);
            let v: u64 = match opcode {
                0b00000 | 0b00010 | 0b00100 | 0b00110 => {
                    // [U|S][R]SHR, [U|S][R]SRA
                    let round = opcode & 4 != 0;
                    let acc = opcode & 2 != 0;
                    let s = shl_signed(ext(x, esize, u), -(rshift as i64), round, esize) as u64;
                    if acc {
                        dd.wrapping_add(s)
                    } else {
                        s
                    }
                },
                0b01000 => {
                    // SRI
                    if !u {
                        return Err(Event::undefined());
                    }
                    if rshift >= esize {
                        dd
                    } else {
                        let mask = ones(esize) >> rshift;
                        (dd & !mask) | ((x & ones(esize)) >> rshift)
                    }
                },
                0b01010 => {
                    // SHL / SLI
                    let s = x << lshift;
                    if u {
                        let mask = ones(esize) << lshift;
                        (dd & !mask) | (s & mask)
                    } else {
                        s
                    }
                },
                0b01100 | 0b01110 => {
                    // SQSHLU / SQSHL / UQSHL (immediate)
                    let (src_u, dst_u) = match (opcode, u) {
                        (0b01100, true) => (false, true),
                        (0b01100, false) => return Err(Event::undefined()),
                        (_, uu) => (uu, uu),
                    };
                    let (o, s) = sat(ext(x, esize, src_u) << lshift, esize, dst_u);
                    qc |= s;
                    o
                },
                _ => return Err(Event::undefined()),
            };
            set_elem(&mut r, i, esize, v);
        }
        if qc {
            self.sys.fpsr |= fp::QC;
        }
        if scalar {
            self.vreg[rd as usize] = r & ones(esize) as u128;
        } else {
            self.vset(rd, r, q);
        }
        Ok(())
    }

    // ========================================================= indexed element

    fn indexed(&mut self, insn: u32, scalar: bool) -> Res<()> {
        let q = bit(insn, 30);
        let u = bit(insn, 29);
        let size = field(insn, 22, 2);
        let l = field(insn, 21, 1);
        let m = field(insn, 20, 1);
        let rm4 = field(insn, 16, 4);
        let opcode = field(insn, 12, 4);
        let h = field(insn, 11, 1);
        let (rd, rn) = (insn & 31, field(insn, 5, 5));
        let fp_op = matches!(
            (u, opcode),
            (false, 0b0001) | (false, 0b0101) | (false, 0b1001) | (true, 0b1001)
        );
        if fp_op {
            if size & 2 == 0 {
                return Err(Event::undefined()); // FP16
            }
            let sz = size & 1 != 0;
            let (index, rm) = if sz {
                if l != 0 {
                    return Err(Event::undefined());
                }
                (h, m << 4 | rm4)
            } else {
                (h << 1 | l, m << 4 | rm4)
            };
            let esize = if sz { 64 } else { 32 };
            if sz && !q && !scalar {
                return Err(Event::undefined());
            }
            let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
            let (a, d) = (self.v(rn), self.v(rd));
            let y = elem(self.v(rm), index, esize);
            let code = match (u, opcode) {
                (false, 0b0001) => 4u32, // FMLA
                (false, 0b0101) => 5,    // FMLS
                (false, 0b1001) => 11,   // FMUL
                _ => 10,                 // FMULX
            };
            let r = self.fpst_run(|st| {
                let mut r = 0u128;
                for i in 0..lanes {
                    let x = elem(a, i, esize);
                    let dd = elem(d, i, esize);
                    let v = if esize == 32 {
                        fp3::<f32>(st, code, x, y, dd)
                    } else {
                        fp3::<f64>(st, code, x, y, dd)
                    };
                    set_elem(&mut r, i, esize, v);
                }
                r
            });
            if scalar {
                self.vreg[rd as usize] = r & ones(esize) as u128;
            } else {
                self.vset(rd, r, q);
            }
            return Ok(());
        }
        let (index, rm) = match size {
            1 => (h << 2 | l << 1 | m, rm4),
            2 => (h << 1 | l, m << 4 | rm4),
            _ => return Err(Event::undefined()),
        };
        let esize = 8u32 << size;
        let y = elem(self.v(rm), index, esize);
        let (a, d) = (self.v(rn), self.v(rd));
        let long = matches!(opcode, 0b0010 | 0b0011 | 0b0110 | 0b0111 | 0b1010 | 0b1011);
        let sat_dbl = matches!(opcode, 0b0011 | 0b0111 | 0b1011);
        if sat_dbl && u {
            return Err(Event::undefined());
        }
        if scalar && !(sat_dbl || ((opcode == 0b1100 || opcode == 0b1101) && !u)) {
            return Err(Event::undefined());
        }
        let mut qc = false;
        if long {
            let w = esize * 2;
            let half = if q && !scalar { 64 } else { 0 };
            let n = if scalar { 1 } else { 64 / esize };
            let mut r = 0u128;
            for i in 0..n {
                let x = elem(a >> half, i, esize);
                let dw = elem(d, i, w);
                let p = ext(x, esize, u) * ext(y, esize, u);
                let v = match opcode {
                    0b0010 => dw.wrapping_add(p as u64),
                    0b0110 => dw.wrapping_sub(p as u64),
                    0b1010 => p as u64,
                    _ => {
                        let (pp, s1) = sat(2 * p, w, false);
                        qc |= s1;
                        if opcode == 0b1011 {
                            pp
                        } else {
                            let acc = sx(dw, w);
                            let pv = sx(pp, w);
                            let (o, s2) =
                                sat(if opcode == 0b0011 { acc + pv } else { acc - pv }, w, false);
                            qc |= s2;
                            o
                        }
                    },
                };
                set_elem(&mut r, i, w, v);
            }
            if qc {
                self.sys.fpsr |= fp::QC;
            }
            self.vreg[rd as usize] = if scalar { r & ones(w) as u128 } else { r };
            return Ok(());
        }
        let lanes = if scalar { 1 } else { (if q { 128 } else { 64 }) / esize };
        let mut r = 0u128;
        for i in 0..lanes {
            let x = elem(a, i, esize);
            let dd = elem(d, i, esize);
            let v = match (u, opcode) {
                (true, 0b0000) => dd.wrapping_add(x.wrapping_mul(y)),
                (true, 0b0100) => dd.wrapping_sub(x.wrapping_mul(y)),
                (false, 0b1000) => x.wrapping_mul(y),
                (false, 0b1100) | (false, 0b1101) => {
                    let round = if opcode == 0b1101 { 1i128 << (esize - 1) } else { 0 };
                    let (o, s) = sat(
                        (2 * sx(x, esize) * sx(y, esize) + round) >> esize,
                        esize,
                        false,
                    );
                    qc |= s;
                    o
                },
                _ => return Err(Event::undefined()),
            };
            set_elem(&mut r, i, esize, v);
        }
        if qc {
            self.sys.fpsr |= fp::QC;
        }
        if scalar {
            self.vreg[rd as usize] = r & ones(esize) as u128;
        } else {
            self.vset(rd, r, q);
        }
        Ok(())
    }

    // ======================================================= permute / ext / tbl

    fn permute(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let size = field(insn, 22, 2);
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let opcode = field(insn, 12, 3);
        if size == 3 && !q {
            return Err(Event::undefined());
        }
        let esize = 8u32 << size;
        let lanes = (if q { 128 } else { 64 }) / esize;
        let (a, b) = (self.v(rn), self.v(rm));
        let pairs = lanes / 2;
        let part = opcode >> 2; // 0: *1, 1: *2
        let mut r = 0u128;
        match opcode & 3 {
            1 => {
                // UZP
                for i in 0..lanes {
                    let j = 2 * i + part;
                    let v = if j < lanes { elem(a, j, esize) } else { elem(b, j - lanes, esize) };
                    set_elem(&mut r, i, esize, v);
                }
            },
            2 => {
                // TRN
                for p in 0..pairs {
                    set_elem(&mut r, 2 * p, esize, elem(a, 2 * p + part, esize));
                    set_elem(&mut r, 2 * p + 1, esize, elem(b, 2 * p + part, esize));
                }
            },
            3 => {
                // ZIP
                let base = part * pairs;
                for p in 0..pairs {
                    set_elem(&mut r, 2 * p, esize, elem(a, base + p, esize));
                    set_elem(&mut r, 2 * p + 1, esize, elem(b, base + p, esize));
                }
            },
            _ => return Err(Event::undefined()),
        }
        self.vset(rd, r, q);
        Ok(())
    }

    fn ext(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let imm4 = field(insn, 11, 4);
        if field(insn, 22, 2) != 0 || (!q && imm4 >= 8) {
            return Err(Event::undefined());
        }
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let bytes = if q { 16 } else { 8 };
        let (a, b) = (self.v(rn), self.v(rm));
        let mut r = 0u128;
        for i in 0..bytes {
            let j = i + imm4;
            let v = if j < bytes { elem(a, j, 8) } else { elem(b, j - bytes, 8) };
            set_elem(&mut r, i, 8, v);
        }
        self.vset(rd, r, q);
        Ok(())
    }

    fn tbl(&mut self, insn: u32) -> Res<()> {
        let q = bit(insn, 30);
        let (rd, rn, rm) = (insn & 31, field(insn, 5, 5), field(insn, 16, 5));
        let len = field(insn, 13, 2) + 1;
        let tbx = bit(insn, 12);
        if field(insn, 22, 2) != 0 {
            return Err(Event::undefined());
        }
        let idx = self.v(rm);
        let d = self.v(rd);
        let bytes = if q { 16 } else { 8 };
        let mut r = 0u128;
        for i in 0..bytes {
            let k = elem(idx, i, 8) as u32;
            let v = if k < 16 * len {
                let reg = (rn + k / 16) % 32;
                elem(self.v(reg), k % 16, 8)
            } else if tbx {
                elem(d, i, 8)
            } else {
                0
            };
            set_elem(&mut r, i, 8, v);
        }
        self.vset(rd, r, q);
        Ok(())
    }
}

/// Per-lane FP three-same operations, by the `Op` discriminant order used in
/// `three_same_fp` (and 4/5/10/11 reused by the indexed forms).
fn fp3<F: Float>(st: &mut Fpst, op: u32, x: u64, y: u64, d: u64) -> u64 {
    let (a, b, acc) = (F::from_bits64(x), F::from_bits64(y), F::from_bits64(d));
    let all = |t: bool| if t { ones(F::BITS) } else { 0 };
    match op {
        0 | 2 => fp::maxmin_num(st, a, b, true).bits(), // MaxNm, MaxNmP
        1 | 3 => fp::maxmin_num(st, a, b, false).bits(), // MinNm, MinNmP
        4 => fp::muladd(st, acc, a, b).bits(),          // FMLA
        5 => fp::muladd(st, acc, F::from_bits64(a.bits() ^ F::SIGN), b).bits(), // FMLS
        6 | 8 => fp::add(st, a, b).bits(),              // Add, AddP
        7 => fp::sub(st, a, b).bits(),
        9 => fp::sub(st, a, b).bits() & !F::SIGN, // FABD
        10 => fp::mulx(st, a, b).bits(),
        11 => fp::mul(st, a, b).bits(),
        12 => all(fp::cmp_eq(st, a, b)),
        13 => all(fp::cmp_ge(st, a, b)),
        14 => all(fp::cmp_gt(st, a, b)),
        15 => all(fp::cmp_ge(st, a.absf(), b.absf())),
        16 => all(fp::cmp_gt(st, a.absf(), b.absf())),
        17 | 19 => fp::maxmin(st, a, b, true).bits(),
        18 | 20 => fp::maxmin(st, a, b, false).bits(),
        21 => fp::recip_step(st, a, b).bits(),
        22 => fp::rsqrt_step(st, a, b).bits(),
        _ => fp::div(st, a, b).bits(),
    }
}

fn pair_op<F: Float>(st: &mut Fpst, code: u32, x: u64, y: u64) -> u64 {
    let (a, b) = (F::from_bits64(x), F::from_bits64(y));
    match code {
        0 => fp::maxmin_num(st, a, b, true).bits(),
        1 => fp::maxmin_num(st, a, b, false).bits(),
        2 => fp::add(st, a, b).bits(),
        3 => fp::maxmin(st, a, b, true).bits(),
        _ => fp::maxmin(st, a, b, false).bits(),
    }
}

#[derive(Clone, Copy)]
enum MiscOp {
    Rint(Round, bool),
    RintCur(bool),
    ToInt(Round, bool),
    FromInt(bool),
    Cmp(u32),
    Abs,
    Neg,
    Sqrt,
    Recpe,
    Rsqrte,
    Recpx,
}

fn fp_misc<F: Float>(st: &mut Fpst, op: MiscOp, x: u64) -> u64 {
    let a = F::from_bits64(x);
    let all = |t: bool| if t { ones(F::BITS) } else { 0 };
    match op {
        MiscOp::Rint(r, exact) => fp::round_int(st, a, r, exact).bits(),
        MiscOp::RintCur(exact) => {
            let m = st.rmode();
            fp::round_int(st, a, m, exact).bits()
        },
        MiscOp::ToInt(r, unsigned) => fp::to_int(st, a, F::BITS, unsigned, 0, r),
        MiscOp::FromInt(unsigned) => {
            let v = if unsigned { x } else { crate::bits::sext(x, F::BITS) as u64 };
            fp::from_int::<F>(st, v, !unsigned, 0).bits()
        },
        MiscOp::Cmp(c) => {
            let z = F::zero();
            all(match c {
                0 => fp::cmp_gt(st, a, z),
                1 => fp::cmp_eq(st, a, z),
                2 => fp::cmp_gt(st, z, a),
                3 => fp::cmp_ge(st, a, z),
                _ => fp::cmp_ge(st, z, a),
            })
        },
        MiscOp::Abs => a.bits() & !F::SIGN,
        MiscOp::Neg => a.bits() ^ F::SIGN,
        MiscOp::Sqrt => fp::sqrt(st, a).bits(),
        MiscOp::Recpe => fp::recip_est(st, a).bits(),
        MiscOp::Rsqrte => fp::rsqrt_est(st, a).bits(),
        MiscOp::Recpx => fp::recpx(st, a).bits(),
    }
}

/// URECPE: unsigned reciprocal estimate (32-bit lanes).
fn urecpe(x: u32) -> u32 {
    if x & 0x8000_0000 == 0 {
        return 0xffff_ffff;
    }
    let a = (x >> 23) as u64; // 9 bits: 256..511
    let a = a * 2 + 1;
    let b = (1u64 << 19) / a;
    let est = (b + 1) / 2;
    ((est as u32) & 0x1ff) << 23
}

/// URSQRTE: unsigned reciprocal square root estimate (32-bit lanes).
fn ursqrte(x: u32) -> u32 {
    if x & 0xc000_0000 == 0 {
        return 0xffff_ffff;
    }
    let a = (x >> 23) as u64; // 9 bits: 128..511
    let a = if a < 256 { a * 2 + 1 } else { (((a >> 1) << 1) + 1) * 2 };
    let mut b = 512u64;
    while a * (b + 1) * (b + 1) < (1u64 << 28) {
        b += 1;
    }
    let est = (b + 1) / 2;
    ((est as u32) & 0x1ff) << 23
}

/// FCVTXN: f64 -> f32 rounding to odd.
fn fcvt_round_odd(st: &mut Fpst, a: f64) -> u32 {
    if a.is_nan() {
        return fp::f64_to_f32(st, a).to_bits();
    }
    if a.is_infinite() || a == 0.0 {
        return (a as f32).to_bits();
    }
    // Truncate toward zero, then set the lowest bit if inexact.
    let mut tmp = 0u64;
    let mut st2 = Fpst {
        fpcr: (st.fpcr & !(3 << 22)) | (3 << 22),
        fpsr: &mut tmp,
    };
    let r = fp::f64_to_f32(&mut st2, a);
    *st.fpsr |= tmp & !fp::QC;
    let mut bits = r.to_bits();
    if tmp & fp::IXC != 0 && !r.is_infinite() {
        bits |= 1;
    }
    bits
}
