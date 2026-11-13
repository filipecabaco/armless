use super::*;
use std::arch::asm;

const FLAGS: u64 = IOC | DZC | OFC | UFC | IXC | IDC;

/// Rounding modes, FZ, DN and AHP, alone and together.
const FPCRS: [u64; 9] = [
    0,
    1 << 22,
    2 << 22,
    3 << 22,
    1 << 24,
    1 << 25,
    3 << 24,
    1 << 26,
    (1 << 26) | (3 << 24) | (2 << 22),
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn fpcr(&mut self) -> u64 {
        FPCRS[self.below(FPCRS.len() as u64) as usize]
    }
}

/// Operand bits: any bits, else ordinary numbers or the edges (signed zeros, infinities,
/// NaNs of both kinds, subnormals, the extremes, and numbers near where results under- or
/// overflow).
fn operand(rng: &mut Rng, double: bool) -> u64 {
    let (exp_bits, frac_bits) = if double { (11, 52) } else { (8, 23) };
    let sign = rng.below(2) << (exp_bits + frac_bits);
    let frac = rng.next() & ((1 << frac_bits) - 1);
    let max_exp = (1u64 << exp_bits) - 1;
    let bias = max_exp / 2;
    let exp = match rng.below(8) {
        0 => return rng.next() & crate::bits::ones(exp_bits + frac_bits + 1),
        1 => 0,                          // zero, subnormal
        2 => max_exp,                    // infinity, NaN
        3 => 1 + rng.below(3),           // smallest normals
        4 => max_exp - 1 - rng.below(3), // largest finite
        5 => bias - 1 + rng.below(3),    // near 1
        _ => bias - 30 + rng.below(60),  // moderate
    };
    let frac = match rng.below(4) {
        0 => 0,
        1 => frac & !((1 << (frac_bits - 3)) - 1), // few significant bits: exact results
        _ => frac,
    };
    sign | (exp << frac_bits) | frac
}

/// One instruction on the host FPU with FPCR set: (result, cumulative flags it raised).
macro_rules! host {
    ($fpcr:expr, $insn:literal, $ty:ty, $($name:ident = $val:expr),*) => {{
        let r: $ty;
        let fpsr: u64;
        // SAFETY: only FPCR and FPSR change, and FPCR is restored; FPSR's flags are this
        // thread's and cumulative, so clearing them first hides nothing the test relies on.
        unsafe {
            asm!(
                "mrs {saved}, fpcr",
                "msr fpcr, {fpcr}",
                "msr fpsr, xzr",
                $insn,
                "mrs {fpsr}, fpsr",
                "msr fpcr, {saved}",
                fpcr = in(reg) $fpcr,
                saved = out(reg) _,
                fpsr = out(reg) fpsr,
                r = out(vreg) r,
                $($name = in(vreg) $val,)*
                options(nostack),
            );
        }
        (r, fpsr & FLAGS)
    }};
}

/// The interpreter's (result, flags).
fn ours<T>(fpcr: u64, f: impl FnOnce(&mut Fpst) -> T) -> (T, u64) {
    let mut fpsr = 0;
    let r = f(&mut Fpst {
        fpcr,
        fpsr: &mut fpsr,
    });
    (r, fpsr & FLAGS)
}

const CASES: usize = 20_000;

/// Run `check` on CASES random (FPCR, operands) and report the first mismatch with them.
fn each(seed: u64, mut check: impl FnMut(&mut Rng) -> Option<String>) {
    let mut rng = Rng(seed);
    for case in 0..CASES {
        if let Some(m) = check(&mut rng) {
            panic!("case {case}: {m}");
        }
    }
}

macro_rules! unary {
    ($name:ident, $seed:expr, $f:ty, $double:expr, $insn:literal, $ours:expr) => {
        #[test]
        fn $name() {
            each($seed, |rng| {
                let fpcr = rng.fpcr();
                let a = <$f>::from_bits(operand(rng, $double) as _);
                let (h, hf) = host!(fpcr, $insn, $f, a = a);
                let (o, of) = ours(fpcr, |st| $ours(st, a));
                (h.to_bits() != o.to_bits() || hf != of).then(|| {
                    format!(
                        "fpcr {fpcr:#x} a {:#x}: host {:#x} {hf:#x}, ours {:#x} {of:#x}",
                        a.to_bits(),
                        h.to_bits(),
                        o.to_bits()
                    )
                })
            })
        }
    };
}

macro_rules! binary {
    ($name:ident, $seed:expr, $f:ty, $double:expr, $insn:literal, $ours:expr) => {
        #[test]
        fn $name() {
            each($seed, |rng| {
                let fpcr = rng.fpcr();
                let a = <$f>::from_bits(operand(rng, $double) as _);
                let b = <$f>::from_bits(operand(rng, $double) as _);
                let (h, hf) = host!(fpcr, $insn, $f, a = a, b = b);
                let (o, of) = ours(fpcr, |st| $ours(st, a, b));
                (h.to_bits() != o.to_bits() || hf != of).then(|| {
                    format!("fpcr {fpcr:#x} a {:#x} b {:#x}: host {:#x} {hf:#x}, ours {:#x} {of:#x}",
                        a.to_bits(), b.to_bits(), h.to_bits(), o.to_bits())
                })
            })
        }
    };
}

// The estimates and steps: no cargo test reached them before, only qemu in CI
unary!(
    frecpe_s,
    1,
    f32,
    false,
    "frecpe {r:s}, {a:s}",
    recip_est::<f32>
);
unary!(
    frecpe_d,
    2,
    f64,
    true,
    "frecpe {r:d}, {a:d}",
    recip_est::<f64>
);
unary!(
    frsqrte_s,
    3,
    f32,
    false,
    "frsqrte {r:s}, {a:s}",
    rsqrt_est::<f32>
);
unary!(
    frsqrte_d,
    4,
    f64,
    true,
    "frsqrte {r:d}, {a:d}",
    rsqrt_est::<f64>
);
unary!(frecpx_s, 5, f32, false, "frecpx {r:s}, {a:s}", recpx::<f32>);
unary!(frecpx_d, 6, f64, true, "frecpx {r:d}, {a:d}", recpx::<f64>);
binary!(
    frecps_s,
    7,
    f32,
    false,
    "frecps {r:s}, {a:s}, {b:s}",
    recip_step::<f32>
);
binary!(
    frecps_d,
    8,
    f64,
    true,
    "frecps {r:d}, {a:d}, {b:d}",
    recip_step::<f64>
);
binary!(
    frsqrts_s,
    9,
    f32,
    false,
    "frsqrts {r:s}, {a:s}, {b:s}",
    rsqrt_step::<f32>
);
binary!(
    frsqrts_d,
    10,
    f64,
    true,
    "frsqrts {r:d}, {a:d}, {b:d}",
    rsqrt_step::<f64>
);
// Precision conversions
unary!(fcvt_d_to_s, 11, f64, true, "fcvt {r:s}, {a:d}", |st, a| {
    f64::from_bits(f64_to_f32(st, a).to_bits() as u64)
});
// The arithmetic the JITs inline and the interpreter backs
binary!(
    fadd_d,
    13,
    f64,
    true,
    "fadd {r:d}, {a:d}, {b:d}",
    add::<f64>
);
binary!(
    fsub_s,
    14,
    f32,
    false,
    "fsub {r:s}, {a:s}, {b:s}",
    sub::<f32>
);
binary!(
    fmul_d,
    15,
    f64,
    true,
    "fmul {r:d}, {a:d}, {b:d}",
    mul::<f64>
);
binary!(
    fdiv_d,
    16,
    f64,
    true,
    "fdiv {r:d}, {a:d}, {b:d}",
    div::<f64>
);
binary!(
    fdiv_s,
    17,
    f32,
    false,
    "fdiv {r:s}, {a:s}, {b:s}",
    div::<f32>
);
binary!(
    fmulx_d,
    18,
    f64,
    true,
    "fmulx {r:d}, {a:d}, {b:d}",
    mulx::<f64>
);
unary!(fsqrt_d, 19, f64, true, "fsqrt {r:d}, {a:d}", sqrt::<f64>);
unary!(fsqrt_s, 20, f32, false, "fsqrt {r:s}, {a:s}", sqrt::<f32>);

#[test]
fn fmadd_d() {
    each(21, |rng| {
        let fpcr = rng.fpcr();
        let [a, b, c] = std::array::from_fn(|_| f64::from_bits(operand(rng, true)));
        let (h, hf) = host!(
            fpcr,
            "fmadd {r:d}, {a:d}, {b:d}, {c:d}",
            f64,
            a = a,
            b = b,
            c = c
        );
        let (o, of) = ours(fpcr, |st| muladd(st, c, a, b));
        (h.to_bits() != o.to_bits() || hf != of).then(|| {
            format!(
                "fpcr {fpcr:#x} {a:e} * {b:e} + {c:e}: host {:#x} {hf:#x}, ours {:#x} {of:#x}",
                h.to_bits(),
                o.to_bits()
            )
        })
    })
}

#[test]
fn fcvt_s_to_d() {
    each(12, |rng| {
        let fpcr = rng.fpcr();
        let a = f32::from_bits(operand(rng, false) as u32);
        let (h, hf) = host!(fpcr, "fcvt {r:d}, {a:s}", f64, a = a);
        let (o, of) = ours(fpcr, |st| f32_to_f64(st, a));
        (h.to_bits() != o.to_bits() || hf != of).then(|| {
            format!(
                "fpcr {fpcr:#x} {:#x}: host {:#x} {hf:#x}, ours {:#x} {of:#x}",
                a.to_bits(),
                h.to_bits(),
                o.to_bits()
            )
        })
    })
}

/// Results just around the largest finite number, in every rounding mode: between "max plus
/// half an ulp" and the next power of two, rounding toward zero gives max without overflow.
#[test]
fn near_overflow() {
    let mut rng = Rng(23);
    // A number with the top exponent, and one close to 1 (above or below)
    let big = |rng: &mut Rng, double: bool| {
        let (frac, top) = if double { (52, 0x7fe) } else { (23, 0xfe) };
        (rng.below(2) << (frac + if double { 11 } else { 8 }))
            | ((top - rng.below(2)) << frac)
            | (rng.next() & ((1 << frac) - 1))
    };
    let near_one = |rng: &mut Rng, double: bool| {
        let (frac, bias) = if double { (52, 0x3ff) } else { (23, 0x7f) };
        ((bias - rng.below(2)) << frac)
            | (rng.next() & ((1 << frac) - 1)) & !(rng.below(2) * ((1 << (frac - 4)) - 1))
    };
    // Exact results in [max + ulp/2, 2^(emax+1)), and both signs of each
    let max = f64::MAX.to_bits();
    let crafted: [(&str, [u64; 3]); 4] = [
        ("fadd", [max, 0x7c90_0000_0000_0000, 0]), // max + ulp/2
        ("fmul", [max - 1, 0x3ff0_0000_0000_0001, 0]), // (max - ulp)(1 + 2^-52)
        ("fdiv", [max - 1, 0x3fef_ffff_ffff_ffff, 0]), // (max - ulp)/(1 - 2^-53)
        (
            "fmadd",
            [
                0x7fe0_0000_0000_0000,
                0x4000_0000_0000_0000,
                0xc000_0000_0000_0000,
            ],
        ), // 2^1024 - 2
    ];
    for (op, [x, y, z]) in crafted {
        for sign in [0, 1u64 << 63] {
            let (a, b, c) = (
                f64::from_bits(x ^ sign),
                f64::from_bits(y),
                f64::from_bits(z ^ sign),
            );
            for fpcr in [0, 1 << 22, 2 << 22, 3 << 22] {
                let ((h, hf), (o, of)) = match op {
                    "fadd" => (
                        host!(
                            fpcr,
                            "fadd {r:d}, {a:d}, {b:d}",
                            f64,
                            a = a,
                            b = f64::from_bits(y ^ sign)
                        ),
                        ours(fpcr, |st| add(st, a, f64::from_bits(y ^ sign))),
                    ),
                    "fmul" => (
                        host!(fpcr, "fmul {r:d}, {a:d}, {b:d}", f64, a = a, b = b),
                        ours(fpcr, |st| mul(st, a, b)),
                    ),
                    "fdiv" => (
                        host!(fpcr, "fdiv {r:d}, {a:d}, {b:d}", f64, a = a, b = b),
                        ours(fpcr, |st| div(st, a, b)),
                    ),
                    _ => (
                        host!(
                            fpcr,
                            "fmadd {r:d}, {a:d}, {b:d}, {c:d}",
                            f64,
                            a = a,
                            b = b,
                            c = c
                        ),
                        ours(fpcr, |st| muladd(st, c, a, b)),
                    ),
                };
                assert!(h.to_bits() == o.to_bits() && hf == of,
                    "{op} sign {sign:#x} fpcr {fpcr:#x}: host {:#x} {hf:#x}, ours {:#x} {of:#x}", h.to_bits(), o.to_bits());
            }
        }
    }
    for case in 0..CASES {
        let fpcr =
            [0, 1 << 22, 2 << 22, 3 << 22][case & 3] | if case & 4 != 0 { 3 << 24 } else { 0 };
        let check = |what: &str, h: u64, hf: u64, o: u64, of: u64, ops: &[u64]| {
            assert!(h == o && hf == of,
                "case {case} {what}, fpcr {fpcr:#x} {ops:#x?}: host {h:#x} {hf:#x}, ours {o:#x} {of:#x}");
        };
        let (a, b, c) = (
            f64::from_bits(big(&mut rng, true)),
            f64::from_bits(near_one(&mut rng, true)),
            f64::from_bits(big(&mut rng, true)),
        );
        let ops = [a.to_bits(), b.to_bits(), c.to_bits()];
        let (h, hf) = host!(fpcr, "fadd {r:d}, {a:d}, {c:d}", f64, a = a, c = c);
        let (o, of) = ours(fpcr, |st| add(st, a, c));
        check("fadd", h.to_bits(), hf, o.to_bits(), of, &ops);
        let (h, hf) = host!(fpcr, "fmul {r:d}, {a:d}, {b:d}", f64, a = a, b = b);
        let (o, of) = ours(fpcr, |st| mul(st, a, b));
        check("fmul", h.to_bits(), hf, o.to_bits(), of, &ops);
        let (h, hf) = host!(fpcr, "fdiv {r:d}, {a:d}, {b:d}", f64, a = a, b = b);
        let (o, of) = ours(fpcr, |st| div(st, a, b));
        check("fdiv", h.to_bits(), hf, o.to_bits(), of, &ops);
        let (h, hf) = host!(
            fpcr,
            "fmadd {r:d}, {a:d}, {b:d}, {c:d}",
            f64,
            a = a,
            b = b,
            c = c
        );
        let (o, of) = ours(fpcr, |st| muladd(st, c, a, b));
        check("fmadd", h.to_bits(), hf, o.to_bits(), of, &ops);
        let (a, b) = (
            f32::from_bits(big(&mut rng, false) as u32),
            f32::from_bits(near_one(&mut rng, false) as u32),
        );
        let ops = [a.to_bits() as u64, b.to_bits() as u64];
        let (h, hf) = host!(fpcr, "fmul {r:s}, {a:s}, {b:s}", f32, a = a, b = b);
        let (o, of) = ours(fpcr, |st| mul(st, a, b));
        check(
            "fmul.s",
            h.to_bits() as u64,
            hf,
            o.to_bits() as u64,
            of,
            &ops,
        );
        let (h, hf) = host!(fpcr, "fadd {r:s}, {a:s}, {b:s}", f32, a = a, b = a);
        let (o, of) = ours(fpcr, |st| add(st, a, a));
        check(
            "fadd.s",
            h.to_bits() as u64,
            hf,
            o.to_bits() as u64,
            of,
            &ops,
        );
        // Narrowing a double just past f32's largest finite
        let d = f64::from(f32::MAX) * f64::from_bits(near_one(&mut rng, true)).abs();
        let (h, hf) = host!(fpcr, "fcvt {r:s}, {a:d}", f32, a = d);
        let (o, of) = ours(fpcr, |st| f64_to_f32(st, d));
        check(
            "fcvt",
            h.to_bits() as u64,
            hf,
            o.to_bits() as u64,
            of,
            &[d.to_bits()],
        );
    }
}

/// Half precision both ways, IEEE and the alternative format (FPCR.AHP).
#[test]
fn fcvt_half() {
    each(22, |rng| {
        let fpcr = rng.fpcr();
        let ahp = fpcr & (1 << 26) != 0;
        let a = f64::from_bits(operand(rng, true));
        let (h, hf): (f64, u64) = host!(fpcr, "fcvt {r:h}, {a:d}", f64, a = a);
        let (o, of) = ours(fpcr, |st| f64_to_f16(st, a, ahp));
        if h.to_bits() as u16 as u64 != o as u64 || hf != of {
            return Some(format!(
                "to half, fpcr {fpcr:#x} {:#x}: host {:#x} {hf:#x}, ours {o:#x} {of:#x}",
                a.to_bits(),
                h.to_bits() as u16
            ));
        }
        let half = rng.next() as u16;
        let x = f64::from_bits(half as u64);
        let (h, hf): (f64, u64) = host!(fpcr, "fcvt {r:d}, {a:h}", f64, a = x);
        let (o, of) = ours(fpcr, |st| f16_to_f64(st, half, ahp));
        (h.to_bits() != o.to_bits() || hf != of).then(|| {
            format!(
                "from half, fpcr {fpcr:#x} {half:#x}: host {:#x} {hf:#x}, ours {:#x} {of:#x}",
                h.to_bits(),
                o.to_bits()
            )
        })
    })
}
