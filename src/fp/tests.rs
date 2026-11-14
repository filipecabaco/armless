use super::*;

fn st(fpsr: &mut u64) -> Fpst<'_> {
    Fpst { fpcr: 0, fpsr }
}

/// Operands for the FMA emulations: random magnitudes across the range, and the cases where
/// rounding is hard (an addend that cancels the product, or sits half an ulp from it).
fn fma_cases(n: usize, mut f: impl FnMut(f64, f64, f64)) {
    let mut s = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let val = |next: &mut dyn FnMut() -> u64, exp_span: u64| {
        let r = next();
        let exp = 1023 - exp_span / 2 + (r >> 11) % exp_span;
        f64::from_bits((r & (1 << 63)) | (exp << 52) | (next() & ((1 << 52) - 1)))
    };
    for i in 0..n {
        let span = [8, 64, 600, 2000][i % 4];
        let a = val(&mut next, span);
        let b = val(&mut next, span);
        let p = a * b;
        let lo = a.mul_add(b, -p);
        let ulp = f64::from_bits(p.abs().to_bits() + 1) - p.abs();
        let c = match i % 6 {
            0 => val(&mut next, span),
            1 => -p,
            2 => -p + lo * (next() % 5) as f64,
            3 => -(p + ulp * 0.5),
            4 => ulp * 0.5 * if next() & 1 == 0 { 1.0 } else { -1.0 },
            _ => val(&mut next, 40) * p,
        };
        f(a, b, c);
    }
}

#[test]
fn product_error_without_an_fma_is_exact() {
    let mut checked = 0;
    fma_cases(400_000, |a, b, _| {
        let p = a * b;
        if let Some(e) = two_prod_split(a, b, p) {
            assert_eq!(e.to_bits(), a.mul_add(b, -p).to_bits(), "{a:e} * {b:e}");
            checked += 1;
        }
    });
    assert!(checked > 300_000, "{checked}");
}

#[test]
fn f64_fma_by_rounding_to_odd_is_the_fma() {
    let mut checked = 0;
    fma_cases(600_000, |a, b, c| {
        if let Some(r) = fma64_odd(a, b, c) {
            let want = a.mul_add(b, c);
            assert_eq!(r.to_bits(), want.to_bits(), "{a:e} * {b:e} + {c:e}: {r:e}, not {want:e}");
            checked += 1;
        }
    });
    assert!(checked > 400_000, "{checked}");

    // Operands with few significant bits and exponents near each other, where p + c often
    // lands next to a midpoint that only the product's low part decides: rounding tl + ul
    // to nearest there (not to odd) gets about 1 in 20000 of these wrong, which this checks
    // the cases still catch.
    let mut s = 0x1234_5678_9abc_def1u64;
    let mut next = move || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let (mut ties, mut double_rounding_wrong) = (0, 0);
    for _ in 0..2_000_000 {
        let (ka, kb, kc) = (27 + next() % 26, 27 + next() % 26, 1 + next() % 53);
        let (xa, xb, xc, sc) = (next(), next(), next(), next());
        let ea = (next() % 40) as i32 - 20;
        let eb = (next() % 40) as i32 - 20;
        let ec = (next() % 120) as i32 - 60;
        let a = ((xa % (1 << ka)) | 1) as f64 * 2f64.powi(ea);
        let b = ((xb % (1 << kb)) | 1) as f64 * 2f64.powi(eb);
        let c = ((xc % (1 << kc)) | 1) as f64 * 2f64.powi(ec) * if sc & 1 == 0 { 1.0 } else { -1.0 };
        let want = a.mul_add(b, c);
        if let Some(r) = fma64_odd(a, b, c) {
            assert_eq!(r.to_bits(), want.to_bits(), "{a:e} * {b:e} + {c:e}: {r:e}, not {want:e}");
            ties += 1;
            let p = a * b;
            let (th, tl) = two_sum(c, p);
            if (th + (tl + a.mul_add(b, -p))).to_bits() != want.to_bits() {
                double_rounding_wrong += 1;
            }
        }
    }
    assert!(ties > 1_900_000, "{ties}");
    assert!(double_rounding_wrong > 20, "{double_rounding_wrong}");
}

#[test]
fn f32_fma_by_rounding_to_odd_is_the_fma() {
    let mut checked = 0;
    fma_cases(600_000, |a, b, c| {
        // The same shapes in f32, subnormals and overflow included
        let (a, b) = (a as f32, b as f32);
        let p = a as f64 * b as f64;
        let c = if c.abs() > 1e30 || c.abs() < 1e-30 { c as f32 } else { (c * (p / (a * b) as f64)) as f32 };
        for c in [c, -(p as f32), -(p as f32) * (1.0 + f32::EPSILON)] {
            if let Some(r) = fma32_odd(a, b, c) {
                let want = a.mul_add(b, c);
                assert!(
                    r.to_bits() == want.to_bits() || (r.is_nan() && want.is_nan()),
                    "{a:e} * {b:e} + {c:e}: {r:e}, not {want:e}"
                );
                checked += 1;
            }
        }
    });
    assert!(checked > 1_000_000, "{checked}");
}

#[test]
fn nan_propagation() {
    let mut f = 0;
    let mut s = st(&mut f);
    let snan = f32::from_bits(0x7fa0_0001);
    let qnan = f32::from_bits(0xffc1_2345);
    let r = add(&mut s, qnan, snan);
    assert_eq!(
        r.to_bits(),
        0x7fe0_0001,
        "signalling NaN wins and is quieted"
    );
    assert_eq!(f & IOC, IOC);
}

#[test]
fn inexact_and_rounding() {
    let mut f = 0;
    let mut s = st(&mut f);
    let r = add(&mut s, 1.0f32, 1e-10);
    assert_eq!(r, 1.0);
    assert_eq!(f & IXC, IXC);
    let mut f2 = 0;
    let mut s2 = Fpst {
        fpcr: 1 << 22,
        fpsr: &mut f2,
    }; // round to +inf
    let r = add(&mut s2, 1.0f32, 1e-10);
    assert_eq!(r, 1.0f32.next_up());
}

#[test]
fn conversions() {
    let mut f = 0;
    let mut s = st(&mut f);
    assert_eq!(to_int(&mut s, 2.5f64, 32, false, 0, Round::Nearest), 2);
    assert_eq!(
        to_int(&mut s, -2.5f64, 32, false, 0, Round::TiesAway),
        (-3i64 as u64) & 0xffff_ffff
    );
    assert_eq!(
        to_int(&mut s, 1e20f64, 32, false, 0, Round::Zero),
        0x7fff_ffff
    );
    assert_eq!(to_int(&mut s, -1.0f64, 32, true, 0, Round::Zero), 0);
    let v: f32 = from_int(&mut s, u64::MAX, false, 0);
    assert_eq!(v, 18446744073709551616.0f32);
    let h = f64_to_f16(&mut s, 1.0, false);
    assert_eq!(h, 0x3c00);
    assert_eq!(f16_to_f64(&mut s, 0x3c00, false), 1.0);
    assert_eq!(f64_to_f16(&mut s, 65504.0, false), 0x7bff);
    assert_eq!(f64_to_f16(&mut s, 5.960464477539063e-08, false), 0x0001);
}
