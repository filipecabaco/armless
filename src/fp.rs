//! Floating-point semantics the Arm way: NaN propagation, default NaN, flush-
//! to-zero, cumulative exception flags (FPSR) and rounding modes (FPCR).
//!
//! Arithmetic is done with the host's IEEE-754 operations in round-to-nearest
//! and then corrected for the other rounding modes using exact error terms,
//! so results match the architecture without a full soft-float library. NaNs
//! are always handled explicitly before touching host arithmetic, so results
//! are identical on x86 and in WebAssembly (whose NaN payloads are
//! nondeterministic).

pub const IOC: u64 = 1 << 0;
pub const DZC: u64 = 1 << 1;
pub const OFC: u64 = 1 << 2;
pub const UFC: u64 = 1 << 3;
pub const IXC: u64 = 1 << 4;
pub const IDC: u64 = 1 << 7;
pub const QC: u64 = 1 << 27;

/// Rounding modes (FPCR.RMode encoding, plus ties-away for FCVTA*/FRINTA).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Round {
    Nearest,
    PlusInf,
    MinusInf,
    Zero,
    TiesAway,
}

impl Round {
    pub fn from_rmode(r: u64) -> Round {
        match r & 3 {
            0 => Round::Nearest,
            1 => Round::PlusInf,
            2 => Round::MinusInf,
            _ => Round::Zero,
        }
    }
}

/// FPCR/FPSR state threaded through operations.
pub struct Fpst<'a> {
    pub fpcr: u64,
    pub fpsr: &'a mut u64,
}

impl Fpst<'_> {
    #[inline]
    pub fn flag(&mut self, f: u64) {
        *self.fpsr |= f;
    }
    pub fn dn(&self) -> bool {
        self.fpcr & (1 << 25) != 0
    }
    pub fn fz(&self) -> bool {
        self.fpcr & (1 << 24) != 0
    }
    pub fn rmode(&self) -> Round {
        Round::from_rmode(self.fpcr >> 22)
    }
}

/// Operations needed from f32 / f64.
pub trait Float: Copy + PartialOrd + std::fmt::Debug {
    const BITS: u32;
    const MANT: u32;
    const DEFAULT_NAN: u64;
    const QUIET: u64;
    const SIGN: u64;
    const INF: u64;
    const MIN_NORMAL: Self;
    fn from_bits64(b: u64) -> Self;
    fn bits(self) -> u64;
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    fn mul(self, o: Self) -> Self;
    fn div(self, o: Self) -> Self;
    fn fma(self, b: Self, c: Self) -> Self;
    fn sqrt(self) -> Self;
    fn absf(self) -> Self;
    fn zero() -> Self;
    fn is_nan(self) -> bool;
    fn is_inf(self) -> bool;
    fn is_zero(self) -> bool;
    fn is_subnormal(self) -> bool;
    fn sign(self) -> bool {
        self.bits() & Self::SIGN != 0
    }
    fn next_up(self) -> Self;
    fn next_down(self) -> Self;
    fn floor(self) -> Self;
    fn ceil(self) -> Self;
    fn trunc(self) -> Self;
    fn round_even(self) -> Self;
    fn round_away(self) -> Self;
    fn to_f64(self) -> f64;
}

macro_rules! impl_float {
    ($t:ty, $ut:ty, $bits:expr, $mant:expr, $dn:expr, $fma:ident) => {
        impl Float for $t {
            const BITS: u32 = $bits;
            const MANT: u32 = $mant;
            const DEFAULT_NAN: u64 = $dn;
            const QUIET: u64 = 1 << ($mant - 1);
            const SIGN: u64 = 1 << ($bits - 1);
            const INF: u64 = ((1u64 << ($bits - 1 - $mant)) - 1) << $mant;
            const MIN_NORMAL: Self = <$t>::MIN_POSITIVE;
            #[inline]
            fn from_bits64(b: u64) -> Self {
                <$t>::from_bits(b as $ut)
            }
            #[inline]
            fn bits(self) -> u64 {
                self.to_bits() as u64
            }
            #[inline]
            fn add(self, o: Self) -> Self {
                self + o
            }
            #[inline]
            fn sub(self, o: Self) -> Self {
                self - o
            }
            #[inline]
            fn mul(self, o: Self) -> Self {
                self * o
            }
            #[inline]
            fn div(self, o: Self) -> Self {
                self / o
            }
            #[inline]
            fn fma(self, b: Self, c: Self) -> Self {
                $fma(self, b, c)
            }
            #[inline]
            fn sqrt(self) -> Self {
                <$t>::sqrt(self)
            }
            #[inline]
            fn absf(self) -> Self {
                <$t>::abs(self)
            }
            #[inline]
            fn zero() -> Self {
                0.0
            }
            #[inline]
            fn is_nan(self) -> bool {
                <$t>::is_nan(self)
            }
            #[inline]
            fn is_inf(self) -> bool {
                <$t>::is_infinite(self)
            }
            #[inline]
            fn is_zero(self) -> bool {
                self == 0.0
            }
            #[inline]
            fn is_subnormal(self) -> bool {
                <$t>::is_subnormal(self)
            }
            fn next_up(self) -> Self {
                <$t>::next_up(self)
            }
            fn next_down(self) -> Self {
                <$t>::next_down(self)
            }
            fn floor(self) -> Self {
                <$t>::floor(self)
            }
            fn ceil(self) -> Self {
                <$t>::ceil(self)
            }
            fn trunc(self) -> Self {
                <$t>::trunc(self)
            }
            fn round_even(self) -> Self {
                <$t>::round_ties_even(self)
            }
            fn round_away(self) -> Self {
                <$t>::round(self)
            }
            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    };
}

impl_float!(f32, u32, 32, 23, 0x7fc0_0000, fma32);
impl_float!(f64, u64, 64, 52, 0x7ff8_0000_0000_0000, fma64);

#[inline]
pub fn is_snan<F: Float>(x: F) -> bool {
    x.is_nan() && x.bits() & F::QUIET == 0
}

/// Flush a subnormal input to zero if FPCR.FZ is set (raising IDC).
#[inline]
pub fn flush_in<F: Float>(st: &mut Fpst, x: F) -> F {
    if st.fz() && x.is_subnormal() {
        st.flag(IDC);
        F::from_bits64(x.bits() & F::SIGN)
    } else {
        x
    }
}

fn default_nan<F: Float>() -> F {
    F::from_bits64(F::DEFAULT_NAN)
}

/// FPProcessNaN: quiet a NaN (IOC if signalling), or default NaN under FPCR.DN.
pub fn process_nan<F: Float>(st: &mut Fpst, x: F) -> F {
    if is_snan(x) {
        st.flag(IOC);
    }
    if st.dn() {
        default_nan()
    } else {
        F::from_bits64(x.bits() | F::QUIET)
    }
}

/// FPProcessNaNs: if either operand is a NaN, the result the architecture picks.
pub fn process_nans<F: Float>(st: &mut Fpst, a: F, b: F) -> Option<F> {
    if is_snan(a) {
        Some(process_nan(st, a))
    } else if is_snan(b) {
        Some(process_nan(st, b))
    } else if a.is_nan() {
        Some(process_nan(st, a))
    } else if b.is_nan() {
        Some(process_nan(st, b))
    } else {
        None
    }
}

pub fn process_nans3<F: Float>(st: &mut Fpst, a: F, b: F, c: F) -> Option<F> {
    if is_snan(a) {
        Some(process_nan(st, a))
    } else if is_snan(b) {
        Some(process_nan(st, b))
    } else if is_snan(c) {
        Some(process_nan(st, c))
    } else if a.is_nan() {
        Some(process_nan(st, a))
    } else if b.is_nan() {
        Some(process_nan(st, b))
    } else if c.is_nan() {
        Some(process_nan(st, c))
    } else {
        None
    }
}

fn invalid<F: Float>(st: &mut Fpst) -> F {
    st.flag(IOC);
    default_nan()
}

/// Round a result computed in round-to-nearest to the requested mode, given
/// the sign of the exact error (exact - r): +1, 0 or -1.
fn adjust<F: Float>(r: F, err: i32, mode: Round) -> F {
    if err == 0 || r.is_nan() {
        return r;
    }
    match mode {
        Round::Nearest | Round::TiesAway => r,
        Round::PlusInf => {
            if err > 0 {
                r.next_up()
            } else {
                r
            }
        },
        Round::MinusInf => {
            if err < 0 {
                r.next_down()
            } else {
                r
            }
        },
        Round::Zero => {
            if err > 0 && r.sign() {
                r.next_up()
            } else if err < 0 && !r.sign() {
                r.next_down()
            } else {
                r
            }
        },
    }
}

fn sign_of<F: Float>(e: F) -> i32 {
    if e.is_zero() || e.is_nan() {
        0
    } else if e.sign() {
        -1
    } else {
        1
    }
}

/// Overflowed result in a directed rounding mode: infinity or the largest finite.
fn overflow_result<F: Float>(negative: bool, mode: Round) -> F {
    let max = F::from_bits64(F::INF - 1);
    let to_inf = match mode {
        Round::Nearest | Round::TiesAway => true,
        Round::PlusInf => !negative,
        Round::MinusInf => negative,
        Round::Zero => false,
    };
    let mag = if to_inf { F::from_bits64(F::INF) } else { max };
    if negative {
        F::from_bits64(mag.bits() | F::SIGN)
    } else {
        mag
    }
}

/// The round-to-nearest result overflowed (the exact one is at least max + ulp/2). Rounded the
/// other ways it may not have: toward zero, an exact result below 2^(emax+1) rounds to the largest
/// finite number, inexact but no overflow. `reaches_2emax1` says whether it gets there.
fn overflowed<F: Float>(st: &mut Fpst, negative: bool, reaches_2emax1: impl FnOnce() -> bool) -> F {
    let mode = st.rmode();
    let toward_zero = match mode {
        Round::Nearest | Round::TiesAway => false,
        Round::PlusInf => negative,
        Round::MinusInf => !negative,
        Round::Zero => true,
    };
    st.flag(if toward_zero && !reaches_2emax1() { IXC } else { OFC | IXC });
    overflow_result(negative, mode)
}

/// `x` halved, when that is exact (any normal number above the smallest exponent). Smaller
/// numbers stay as they are: next to a result near overflow only their sign can count.
fn halve<F: Float>(x: F) -> F {
    let exp = (x.bits() >> F::MANT) & crate::bits::ones(F::BITS - 1 - F::MANT);
    if exp > 1 && !x.is_inf() && !x.is_nan() {
        x.mul(F::from_bits64(if F::BITS == 32 {
            0x3f00_0000
        } else {
            0x3fe0_0000_0000_0000
        }))
    } else {
        x
    }
}

/// Whether an operation's result, rounded toward zero, reaches 2^emax: given halved operands, whether
/// the exact result of the operation on the originals reaches 2^(emax+1) (a power of two, so
/// rounding toward zero keeps which side of it a number is).
fn reaches_2emax<F: Float>(op: impl FnOnce(&mut Fpst) -> F) -> bool {
    let mut fpsr = 0;
    let r = op(&mut Fpst {
        fpcr: 3 << 22,
        fpsr: &mut fpsr,
    });
    // 2^emax: the largest exponent, nothing below it
    r.absf().bits() >= (F::INF - 1) & !crate::bits::ones(F::MANT)
}

/// Common tail: flags for an inexact or tiny result, rounding mode adjustment and output flushing.
fn finish<F: Float>(st: &mut Fpst, r: F, err: i32) -> F {
    let mode = st.rmode();
    // Tininess is judged on the exact (unrounded) value: |exact| < MIN_NORMAL.
    // `r` is the round-to-nearest result and `err` the sign of exact - r.
    let toward_zero = (err < 0 && !r.sign()) || (err > 0 && r.sign());
    let tiny =
        r.is_subnormal() || (r.is_zero() && err != 0) || (r.absf() == F::MIN_NORMAL && toward_zero);
    // Flush-to-zero: a tiny result becomes a signed zero with only UFC set.
    if st.fz() && tiny {
        st.flag(UFC);
        let negative = if r.is_zero() { err < 0 } else { r.sign() };
        return F::from_bits64(if negative { F::SIGN } else { 0 });
    }
    let mut r = r;
    if err != 0 {
        st.flag(IXC);
        if tiny {
            st.flag(UFC);
        }
        r = adjust(r, err, mode);
        if r.is_inf() {
            // Directed rounding stepped past the largest finite value.
            st.flag(OFC);
        }
    }
    r
}

/// Sign of (exact - r) for the operations below. f32 operations are checked
/// exactly in f64; f64 operations use error-free transformations, scaled
/// away from the subnormal range when necessary.
fn cmp_exact_f64(exact_minus_r: f64) -> i32 {
    if exact_minus_r == 0.0 {
        0
    } else if exact_minus_r > 0.0 {
        1
    } else {
        -1
    }
}

fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bb = s - a;
    (s, (a - (s - bb)) + (b - bb))
}

// Fused multiply-add. A host with an FMA instruction uses it. WebAssembly has none, and Rust's
// `mul_add` there is libm's integer emulation, which the benchmark guest's floating point spent
// most of its time in; so the browser builds compute it from plain double operations instead
// (`two_prod_split`, `fma32_odd`, `fma64_odd`: exact, checked against the host's FMA in the
// tests), and use `mul_add` only for what those leave out.

/// Below this magnitude an f64 operand may be split (`two_prod_split`) without overflowing.
const SPLIT_MAX: f64 = 6.0e299; // < 2^996
/// What the error-free transformations need to stay exact: no partial product nor error term
/// falls below the normal range (a product at least this big keeps 2^-106 of itself normal).
const EFT_MIN: f64 = 1e-280;

/// a*b == p + e exactly, with p = a*b rounded to nearest.
#[inline]
fn two_prod(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    #[cfg(target_arch = "wasm32")]
    if let Some(e) = two_prod_split(a, b, p) {
        return (p, e);
    }
    (p, a.mul_add(b, -p))
}

/// Dekker's product error a*b - p for p = a*b rounded to nearest, without an FMA: exact for
/// operands and products below `SPLIT_MAX`, products at least `EFT_MIN` (else None).
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
#[inline]
fn two_prod_split(a: f64, b: f64, p: f64) -> Option<f64> {
    if !(a.abs() < SPLIT_MAX && b.abs() < SPLIT_MAX && p.abs() >= EFT_MIN && p.abs() < SPLIT_MAX) {
        return None;
    }
    let split = |x: f64| {
        let t = 134_217_729.0 * x; // 2^27 + 1
        let hi = t - (t - x);
        (hi, x - hi)
    };
    let (ah, al) = split(a);
    let (bh, bl) = split(b);
    Some(((ah * bh - p) + ah * bl + al * bh) + al * bl)
}

/// The f64 next to `s` toward `s + e` when that is odd and `e` (the error of rounding to `s`)
/// is not zero: rounding to odd, which a later rounding to nearest cannot round twice.
#[inline]
fn round_odd(s: f64, e: f64) -> f64 {
    let b = s.to_bits();
    if e == 0.0 || b & 1 != 0 {
        return s;
    }
    // s is not zero (a sum rounds to zero only when it is zero)
    f64::from_bits(if (e > 0.0) == (s > 0.0) { b + 1 } else { b - 1 })
}

/// a*b + c for f32, correctly rounded: the product is exact in f64, the sum is rounded to odd
/// there and then to nearest in f32 (53 >= 2*24 + 2 bits: Boldo and Melquiond). Finite operands.
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
#[inline]
fn fma32_odd(a: f32, b: f32, c: f32) -> Option<f32> {
    if !(a.is_finite() && b.is_finite() && c.is_finite()) {
        return None;
    }
    let (s, e) = two_sum(a as f64 * b as f64, c as f64);
    Some(round_odd(s, e) as f32)
}

/// a*b + c for f64, correctly rounded with Boldo and Melquiond's emulation of an FMA by rounding
/// to odd ("Emulation of FMA and correctly rounded sums: proved algorithms using rounding to
/// odd", 2008): uh + ul = a*b, th + tl = c + uh, RN(th + RO(tl + ul)). Proved without underflow
/// or overflow, so for operands and partial results within the bounds checked (else None).
#[cfg_attr(not(any(target_arch = "wasm32", test)), allow(dead_code))]
#[inline]
fn fma64_odd(a: f64, b: f64, c: f64) -> Option<f64> {
    let p = a * b;
    if p == 0.0 || !(c == 0.0 || (c.abs() >= EFT_MIN && c.abs() < SPLIT_MAX)) {
        // Products of zero (exactly c, or a signed zero) and the extremes stay with `mul_add`
        return None;
    }
    let ul = two_prod_split(a, b, p)?;
    let (th, tl) = two_sum(c, p);
    let (v, ve) = two_sum(tl, ul);
    let tiny = |x: f64| x != 0.0 && x.abs() < EFT_MIN;
    if tiny(th) || tiny(v) || !th.is_finite() {
        return None;
    }
    let r = th + round_odd(v, ve);
    (r.is_finite() && !tiny(r)).then_some(r)
}

#[inline]
fn fma32(a: f32, b: f32, c: f32) -> f32 {
    #[cfg(target_arch = "wasm32")]
    if let Some(r) = fma32_odd(a, b, c) {
        return r;
    }
    a.mul_add(b, c)
}

#[inline]
fn fma64(a: f64, b: f64, c: f64) -> f64 {
    #[cfg(target_arch = "wasm32")]
    if let Some(r) = fma64_odd(a, b, c) {
        return r;
    }
    a.mul_add(b, c)
}

/// Decompose a finite f64 into (negative, mantissa, exponent) with
/// value = mantissa * 2^exponent.
fn decompose(x: f64) -> (bool, u128, i32) {
    let b = x.to_bits();
    let neg = b >> 63 != 0;
    let e = ((b >> 52) & 0x7ff) as i32;
    let f = b & ((1u64 << 52) - 1);
    if e == 0 {
        (neg, f as u128, -1074)
    } else {
        (neg, (f | 1 << 52) as u128, e - 1075)
    }
}

/// Exact sign of a sum of terms mantissa * 2^exponent (big-integer fallback
/// for the cases where error-free transformations over/underflow).
fn exact_sign(terms: &[(bool, u128, i32)]) -> i32 {
    let terms: Vec<_> = terms.iter().filter(|t| t.1 != 0).copied().collect();
    if terms.is_empty() {
        return 0;
    }
    let min_e = terms.iter().map(|t| t.2).min().unwrap();
    let max_bits = terms
        .iter()
        .map(|t| (t.2 - min_e) as usize + 128)
        .max()
        .unwrap();
    let limbs = max_bits / 64 + 2;
    let mut pos = vec![0u64; limbs];
    let mut negs = vec![0u64; limbs];
    for &(neg, m, e) in &terms {
        let acc = if neg { &mut negs } else { &mut pos };
        let shift = (e - min_e) as usize;
        let (word, bitoff) = (shift / 64, shift % 64);
        // add m << bitoff at limb `word`
        let lo = m << bitoff;
        let hi = if bitoff == 0 { 0 } else { m >> (128 - bitoff) };
        let parts = [lo as u64, (lo >> 64) as u64, hi as u64, (hi >> 64) as u64];
        let mut carry = 0u128;
        let mut i = word;
        let mut k = 0;
        while i < limbs && (k < 4 || carry != 0) {
            let add = if k < 4 { parts[k] as u128 } else { 0 };
            let sum = acc[i] as u128 + add + carry;
            acc[i] = sum as u64;
            carry = sum >> 64;
            i += 1;
            k += 1;
        }
    }
    for i in (0..limbs).rev() {
        if pos[i] != negs[i] {
            return if pos[i] > negs[i] { 1 } else { -1 };
        }
    }
    0
}

fn product_term(neg: bool, a: f64, b: f64) -> (bool, u128, i32) {
    let (na, ma, ea) = decompose(a);
    let (nb, mb, eb) = decompose(b);
    (neg ^ na ^ nb, ma * mb, ea + eb)
}

fn term(neg: bool, a: f64) -> (bool, u128, i32) {
    let (n, m, e) = decompose(a);
    (neg ^ n, m, e)
}

/// Magnitudes where error-free transformations are exact.
fn safe(x: f64) -> bool {
    x == 0.0 || (x.abs() > 1e-280 && x.abs() < 1e280)
}

/// sign(a*b - r) for f64 operands.
fn err_mul64(a: f64, b: f64, r: f64) -> i32 {
    if safe(a * b) && safe(r) && a * b != 0.0 {
        let (p, e) = two_prod(a, b);
        return cmp_exact_f64((p - r) + e);
    }
    exact_sign(&[product_term(false, a, b), term(true, r)])
}

/// sign(a/b - r) for f64 operands.
fn err_div64(a: f64, b: f64, r: f64) -> i32 {
    let s = if safe(r * b) && safe(r) && safe(a) && r * b != 0.0 {
        // r*b is within a factor of 2 of a: p - a is exact (Sterbenz)
        let (p, e) = two_prod(r, b);
        cmp_exact_f64(-((p - a) + e))
    } else {
        exact_sign(&[term(false, a), product_term(true, r, b)])
    };
    if b < 0.0 {
        -s
    } else {
        s
    }
}

/// sign(sqrt(a) - r) for f64.
fn err_sqrt64(a: f64, r: f64) -> i32 {
    if safe(r * r) && safe(a) && r * r != 0.0 {
        let (p, e) = two_prod(r, r);
        return cmp_exact_f64(-((p - a) + e));
    }
    exact_sign(&[term(false, a), product_term(true, r, r)])
}

/// sign(b*c + a - r) for f64.
fn err_fma64(a: f64, b: f64, c: f64, r: f64) -> i32 {
    let p = b * c;
    if safe(p) && safe(a) && safe(r) && (p != 0.0 || b == 0.0 || c == 0.0) {
        let p_lo = two_prod(b, c).1;
        let (s, e1) = two_sum(p, a);
        let t = s - r;
        let low = e1 + p_lo;
        return if t != 0.0 { cmp_exact_f64(t + low) } else { cmp_exact_f64(low) };
    }
    exact_sign(&[product_term(false, b, c), term(false, a), term(true, r)])
}

pub fn add<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if a.is_inf() && b.is_inf() && a.sign() != b.sign() {
        return invalid(st);
    }
    let r = a.add(b);
    if r.is_inf() && !a.is_inf() && !b.is_inf() {
        return overflowed(st, r.sign(), || {
            reaches_2emax(|h| add(h, halve(a), halve(b)))
        });
    }
    // TwoSum error term
    let bb = r.sub(a);
    let err = a.sub(r.sub(bb)).add(b.sub(bb));
    let r = if r.is_zero() && err.is_zero() {
        // Exact zero: -0 when both operands are -0, or for any exact zero
        // sum of opposite-signed operands when rounding toward -inf.
        let both_negative = a.sign() && b.sign();
        let rm_negative = st.rmode() == Round::MinusInf && (a.sign() != b.sign() || a.sign());
        if both_negative || rm_negative {
            F::from_bits64(F::SIGN)
        } else {
            F::zero()
        }
    } else {
        r
    };
    finish(st, r, sign_of(err))
}

pub fn sub<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    // a - b == a + (-b), including NaN handling order (b's sign flip keeps NaN-ness).
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    let nb = F::from_bits64(b.bits() ^ F::SIGN);
    add(st, a, nb)
}

pub fn mul<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        return invalid(st);
    }
    let r = a.mul(b);
    if r.is_inf() && !a.is_inf() && !b.is_inf() {
        // Halving the larger operand halves the product exactly
        let (a, b) = if a.absf().bits() >= b.absf().bits() { (halve(a), b) } else { (a, halve(b)) };
        return overflowed(st, r.sign(), || reaches_2emax(|h| mul(h, a, b)));
    }
    let err = if a.is_zero() || b.is_zero() || r.is_inf() {
        0
    } else if F::BITS == 32 {
        cmp_exact_f64(a.to_f64() * b.to_f64() - r.to_f64())
    } else {
        err_mul64(a.to_f64(), b.to_f64(), r.to_f64())
    };
    finish(st, r, err)
}

/// FMULX: like FMUL but 0 x inf = 2.0 with the XOR of the signs.
pub fn mulx<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        let two = if F::BITS == 32 { 0x4000_0000u64 } else { 0x4000_0000_0000_0000 };
        let s = (a.bits() ^ b.bits()) & F::SIGN;
        return F::from_bits64(two | s);
    }
    mul(st, a, b)
}

pub fn div<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if (a.is_inf() && b.is_inf()) || (a.is_zero() && b.is_zero()) {
        return invalid(st);
    }
    if b.is_zero() && !a.is_inf() {
        st.flag(DZC);
    }
    let r = a.div(b);
    if r.is_inf() && !a.is_inf() && !b.is_zero() {
        return overflowed(st, r.sign(), || reaches_2emax(|h| div(h, halve(a), b)));
    }
    let err = if r.is_inf() || a.is_zero() || b.is_inf() {
        0 // x/0, 0/x and x/inf are exact
    } else if F::BITS == 32 {
        // a - r*b is exact in f64 up to one final rounding, which keeps the sign
        let s = cmp_exact_f64(a.to_f64() - r.to_f64() * b.to_f64());
        if b.sign() {
            -s
        } else {
            s
        }
    } else {
        err_div64(a.to_f64(), b.to_f64(), r.to_f64())
    };
    finish(st, r, err)
}

pub fn sqrt<F: Float>(st: &mut Fpst, a: F) -> F {
    let a = flush_in(st, a);
    if a.is_nan() {
        return process_nan(st, a);
    }
    if a.sign() && !a.is_zero() {
        return invalid(st);
    }
    let r = a.sqrt();
    if r.is_zero() || r.is_inf() {
        return r;
    }
    let err = if F::BITS == 32 {
        cmp_exact_f64(a.to_f64() - r.to_f64() * r.to_f64())
    } else {
        err_sqrt64(a.to_f64(), r.to_f64())
    };
    finish(st, r, err)
}

/// FMADD-style fused multiply-add: a + b*c (the addend is the first operand,
/// matching FPMulAdd's NaN priority).
pub fn muladd<F: Float>(st: &mut Fpst, addend: F, b: F, c: F) -> F {
    let (addend, b, c) = (flush_in(st, addend), flush_in(st, b), flush_in(st, c));
    let inf_times_zero = (b.is_inf() && c.is_zero()) || (b.is_zero() && c.is_inf());
    if let Some(n) = process_nans3(st, addend, b, c) {
        // A quiet-NaN addend with an invalid product is still invalid.
        if inf_times_zero && addend.is_nan() && !is_snan(addend) {
            return invalid(st);
        }
        return n;
    }
    if inf_times_zero {
        return invalid(st);
    }
    let prod_inf = b.is_inf() || c.is_inf();
    if prod_inf && addend.is_inf() && (b.sign() ^ c.sign()) != addend.sign() {
        return invalid(st);
    }
    let r = b.fma(c, addend);
    if r.is_inf() && !prod_inf && !addend.is_inf() {
        let (hb, hc) =
            if b.absf().bits() >= c.absf().bits() { (halve(b), c) } else { (b, halve(c)) };
        return overflowed(st, r.sign(), || {
            reaches_2emax(|h| muladd(h, halve(addend), hb, hc))
        });
    }
    // Exact zero result sign
    if r.is_zero() {
        let psign = b.sign() ^ c.sign();
        let both_zero = (b.is_zero() || c.is_zero()) && addend.is_zero();
        if both_zero {
            let s = if psign == addend.sign() { psign } else { st.rmode() == Round::MinusInf };
            return F::from_bits64(if s { F::SIGN } else { 0 });
        }
    }
    // Round to nearest with Inexact already raised, and a result well inside the normal range:
    // what the error term would decide (Inexact, Underflow, a directed adjustment, the sign of
    // an exact zero) is already settled, so the exact error is not worth computing.
    if !r.is_inf()
        && st.rmode() == Round::Nearest
        && *st.fpsr & IXC != 0
        && r.absf().bits() > F::MIN_NORMAL.bits()
    {
        return r;
    }
    let err = if r.is_inf() {
        0
    } else if F::BITS == 32 {
        // b*c is exact in f64; TwoSum makes the addition exact too.
        let p = b.to_f64() * c.to_f64();
        let (s, e) = two_sum(p, addend.to_f64());
        let t = s - r.to_f64();
        if t != 0.0 {
            cmp_exact_f64(t + e)
        } else {
            cmp_exact_f64(e)
        }
    } else {
        err_fma64(addend.to_f64(), b.to_f64(), c.to_f64(), r.to_f64())
    };
    // An exact zero from terms that cancel is -0 when rounding toward minus infinity (the
    // round-to-nearest fma made it +0)
    let r = if r.is_zero() && err == 0 && st.rmode() == Round::MinusInf {
        F::from_bits64(F::SIGN)
    } else {
        r
    };
    finish(st, r, err)
}

/// FPCompare: NZCV for FCMP/FCCMP. `signal_all` makes quiet NaNs invalid too (FCMPE).
pub fn compare<F: Float>(st: &mut Fpst, a: F, b: F, signal_all: bool) -> u64 {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if a.is_nan() || b.is_nan() {
        if signal_all || is_snan(a) || is_snan(b) {
            st.flag(IOC);
        }
        return 0b0011;
    }
    if a == b {
        0b0110
    } else if a < b {
        0b1000
    } else {
        0b0010
    }
}

/// Comparisons for FCMEQ/FCMGE/FCMGT/FACGE/FACGT (vector compare results).
pub fn cmp_eq<F: Float>(st: &mut Fpst, a: F, b: F) -> bool {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if is_snan(a) || is_snan(b) {
        st.flag(IOC);
    }
    !a.is_nan() && !b.is_nan() && a == b
}

pub fn cmp_ge<F: Float>(st: &mut Fpst, a: F, b: F) -> bool {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if a.is_nan() || b.is_nan() {
        st.flag(IOC);
        return false;
    }
    a >= b
}

pub fn cmp_gt<F: Float>(st: &mut Fpst, a: F, b: F) -> bool {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if a.is_nan() || b.is_nan() {
        st.flag(IOC);
        return false;
    }
    a > b
}

/// FMAX/FMIN (`max` selects which). +0 > -0.
pub fn maxmin<F: Float>(st: &mut Fpst, a: F, b: F, max: bool) -> F {
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if a.is_zero() && b.is_zero() && a.sign() != b.sign() {
        // max(+0,-0) = +0; min = -0
        return F::from_bits64(if max { 0 } else { F::SIGN });
    }
    let pick_a = if max { a > b } else { a < b };
    let r = if pick_a { a } else { b };
    if st.fz() && r.is_subnormal() {
        return F::from_bits64(r.bits() & F::SIGN);
    }
    r
}

/// FMAXNM/FMINNM: a quiet NaN loses against a number.
pub fn maxmin_num<F: Float>(st: &mut Fpst, a: F, b: F, max: bool) -> F {
    let qa = a.is_nan() && !is_snan(a);
    let qb = b.is_nan() && !is_snan(b);
    let inf = F::from_bits64(F::INF | if max { F::SIGN } else { 0 });
    let (a, b) = if qa && !b.is_nan() {
        (inf, b)
    } else if qb && !a.is_nan() {
        (a, inf)
    } else {
        (a, b)
    };
    maxmin(st, a, b, max)
}

/// FRINT*: round to an integral value in floating point.
pub fn round_int<F: Float>(st: &mut Fpst, a: F, mode: Round, exact: bool) -> F {
    let a = flush_in(st, a);
    if a.is_nan() {
        return process_nan(st, a);
    }
    if a.is_inf() || a.is_zero() {
        return a;
    }
    let r = match mode {
        Round::Nearest => a.round_even(),
        Round::PlusInf => a.ceil(),
        Round::MinusInf => a.floor(),
        Round::Zero => a.trunc(),
        Round::TiesAway => a.round_away(),
    };
    if exact && r != a {
        st.flag(IXC);
    }
    // Keep the sign of a zero result from a negative input.
    if r.is_zero() && a.sign() {
        F::from_bits64(F::SIGN)
    } else {
        r
    }
}

/// Float to signed/unsigned integer of `bits` width, saturating, with `fbits`
/// fractional bits (fixed point). Returns the integer bit pattern.
pub fn to_int<F: Float>(
    st: &mut Fpst,
    a: F,
    bits: u32,
    unsigned: bool,
    fbits: u32,
    mode: Round,
) -> u64 {
    let a = flush_in(st, a);
    if a.is_nan() {
        st.flag(IOC);
        return 0;
    }
    // Scale by 2^fbits exactly in f64 (fbits <= 64; f32 values are exact in f64).
    let v = a.to_f64() * (2f64).powi(fbits as i32);
    let r = match mode {
        Round::Nearest => v.round_ties_even(),
        Round::PlusInf => v.ceil(),
        Round::MinusInf => v.floor(),
        Round::Zero => v.trunc(),
        Round::TiesAway => v.round(),
    };
    let (lo, hi) = if unsigned {
        (0f64, (2f64).powi(bits as i32))
    } else {
        (-(2f64).powi(bits as i32 - 1), (2f64).powi(bits as i32 - 1))
    };
    if r < lo || r >= hi {
        st.flag(IOC);
        return if unsigned {
            if r < lo {
                0
            } else {
                crate::bits::ones(bits)
            }
        } else if r < lo {
            (1u64 << (bits - 1)) & crate::bits::ones(bits)
        } else {
            crate::bits::ones(bits - 1)
        };
    }
    if r != v {
        st.flag(IXC);
    }
    if unsigned {
        if bits == 64 {
            r as u64
        } else {
            (r as u64) & crate::bits::ones(bits)
        }
    } else {
        (r as i64 as u64) & crate::bits::ones(bits)
    }
}

/// Integer (signed or unsigned, `fbits` fractional bits) to float.
pub fn from_int<F: Float>(st: &mut Fpst, v: u64, signed: bool, fbits: u32) -> F {
    let neg = signed && (v as i64) < 0;
    let mag: u64 = if neg { (v as i64).unsigned_abs() } else { v };
    let r = round_u64_to::<F>(st, mag, fbits, neg);
    if neg {
        F::from_bits64(r.bits() | F::SIGN)
    } else {
        r
    }
}

/// Round |value| = mag / 2^fbits to F; `negative` selects the directed
/// rounding direction for the signed result.
fn round_u64_to<F: Float>(st: &mut Fpst, mag: u64, fbits: u32, negative: bool) -> F {
    if mag == 0 {
        return F::zero();
    }
    let width = 64 - mag.leading_zeros();
    let prec = F::MANT + 1;
    let (mut mant, mut exp_shift, inexact, round_up) = if width <= prec {
        (mag, 0i32, false, false)
    } else {
        let drop = width - prec;
        let kept = mag >> drop;
        let rem = mag & crate::bits::ones(drop);
        let half = 1u64 << (drop - 1);
        let inexact = rem != 0;
        let up = match st.rmode() {
            Round::Nearest => rem > half || (rem == half && kept & 1 == 1),
            Round::TiesAway => rem >= half,
            Round::PlusInf => inexact && !negative,
            Round::MinusInf => inexact && negative,
            Round::Zero => false,
        };
        (kept, drop as i32, inexact, up)
    };
    if round_up {
        mant += 1;
        if mant >> prec != 0 {
            mant >>= 1;
            exp_shift += 1;
        }
    }
    if inexact {
        st.flag(IXC);
    }
    let m = mant as f64 * (2f64).powi(exp_shift - fbits as i32);
    from_f64_exact::<F>(m)
}

fn from_f64_exact<F: Float>(m: f64) -> F {
    if F::BITS == 64 {
        F::from_bits64(m.to_bits())
    } else {
        F::from_bits64((m as f32).to_bits() as u64)
    }
}

// ---------------------------------------------------------- precision changes

/// f64 -> f32 with the FPCR rounding mode.
pub fn f64_to_f32(st: &mut Fpst, a: f64) -> f32 {
    let a = flush_in(st, a);
    if a.is_nan() {
        if is_snan(a) {
            st.flag(IOC);
        }
        if st.dn() {
            return f32::from_bits(0x7fc0_0000);
        }
        let b = a.to_bits();
        let sign = ((b >> 63) as u32) << 31;
        let payload = ((b >> 29) & 0x003f_ffff) as u32;
        return f32::from_bits(sign | 0x7fc0_0000 | payload);
    }
    if a.is_infinite() || a == 0.0 {
        return a as f32;
    }
    if st.fz() && a.abs() < f32::MIN_POSITIVE as f64 {
        st.flag(UFC);
        return f32::from_bits(if a < 0.0 { 0x8000_0000 } else { 0 });
    }
    let r = a as f32; // round-to-nearest-even
    if r.is_infinite() {
        return overflowed(st, a < 0.0, || a.abs() >= 2f64.powi(128));
    }
    let err = a - r as f64;
    let s = if err == 0.0 {
        0
    } else if err > 0.0 {
        1
    } else {
        -1
    };
    // Tininess before rounding
    if s != 0 && a.abs() < f32::MIN_POSITIVE as f64 {
        st.flag(UFC);
    }
    let r = if s != 0 {
        st.flag(IXC);
        adjust(r, s, st.rmode())
    } else {
        r
    };
    if st.fz() && r.is_subnormal() {
        st.flag(UFC);
        return f32::from_bits(r.to_bits() & 0x8000_0000);
    }
    r
}

/// f32 -> f64 (always exact).
pub fn f32_to_f64(st: &mut Fpst, a: f32) -> f64 {
    let a = flush_in(st, a);
    if a.is_nan() {
        if is_snan(a) {
            st.flag(IOC);
        }
        if st.dn() {
            return f64::from_bits(0x7ff8_0000_0000_0000);
        }
        let b = a.to_bits() as u64;
        let sign = (b >> 31) << 63;
        let payload = (b & 0x003f_ffff) << 29;
        return f64::from_bits(sign | 0x7ff8_0000_0000_0000 | payload);
    }
    a as f64
}

/// IEEE half precision -> f64 (exact). `ahp` = alternative half precision.
pub fn f16_to_f64(st: &mut Fpst, h: u16, ahp: bool) -> f64 {
    let sign = (h >> 15) as u64;
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as u64;
    if exp == 0x1f && !ahp {
        if mant == 0 {
            return f64::from_bits((sign << 63) | 0x7ff0_0000_0000_0000);
        }
        if mant & 0x200 == 0 {
            st.flag(IOC);
        }
        if st.dn() {
            return f64::from_bits(0x7ff8_0000_0000_0000);
        }
        return f64::from_bits((sign << 63) | 0x7ff8_0000_0000_0000 | (mant << 42));
    }
    let v = if exp == 0 {
        mant as f64 * (2f64).powi(-24)
    } else {
        (1024 + mant) as f64 * (2f64).powi(exp - 25)
    };
    if sign == 1 {
        -v
    } else {
        v
    }
}

/// f64 -> IEEE half precision with the FPCR rounding mode.
pub fn f64_to_f16(st: &mut Fpst, a: f64, ahp: bool) -> u16 {
    // FZ flushes a subnormal double (FPUnpack), as for any other conversion
    let a = flush_in(st, a);
    let b = a.to_bits();
    let sign = ((b >> 63) as u16) << 15;
    if a.is_nan() {
        if is_snan(a) {
            st.flag(IOC);
        }
        if ahp {
            st.flag(IOC);
            return sign;
        }
        if st.dn() {
            return 0x7e00;
        }
        return sign | 0x7e00 | ((b >> 42) & 0x1ff) as u16;
    }
    if a.is_infinite() {
        if ahp {
            st.flag(IOC);
            return sign | 0x7fff;
        }
        return sign | 0x7c00;
    }
    if a == 0.0 {
        return sign;
    }
    let mag = a.abs();
    // Scale so that the value is an integer number of half-precision ulps.
    let e = mag.log2().floor() as i32; // unbiased exponent (approximate, fixed below)
    let e = if (2f64).powi(e) > mag {
        e - 1
    } else if (2f64).powi(e + 1) <= mag {
        e + 1
    } else {
        e
    };
    let min_exp = -14;
    let ulp_exp = if e < min_exp { min_exp - 10 } else { e - 10 };
    let scaled = mag * (2f64).powi(-ulp_exp); // exact (power of two scaling)
    let floor = scaled.floor();
    let rem = scaled - floor;
    let mut q = floor as u64;
    let inexact = rem != 0.0;
    let neg = sign != 0;
    let up = match st.rmode() {
        Round::Nearest => rem > 0.5 || (rem == 0.5 && q & 1 == 1),
        Round::TiesAway => rem >= 0.5,
        Round::PlusInf => inexact && !neg,
        Round::MinusInf => inexact && neg,
        Round::Zero => false,
    };
    if up {
        q += 1;
    }
    // Reassemble: value = q * 2^ulp_exp
    let mut exp_field: i32;
    let mut mant: u64;
    if ulp_exp == min_exp - 10 && q < 1024 {
        exp_field = 0;
        mant = q;
    } else {
        // normalise q into [1024, 2048)
        let mut ue = ulp_exp;
        let mut qq = q;
        while qq >= 2048 {
            qq >>= 1;
            ue += 1;
        }
        exp_field = ue + 10 + 15;
        mant = qq & 0x3ff;
        if qq < 1024 {
            exp_field = 0;
            mant = qq;
        }
    }
    let max_exp = if ahp { 31 } else { 30 };
    if exp_field > max_exp && ahp {
        // The alternative format has no infinity: the largest number, an invalid operation and
        // nothing else (not inexact)
        st.flag(IOC);
        return sign | 0x7fff;
    }
    if inexact {
        st.flag(IXC);
        if e < min_exp {
            st.flag(UFC);
        }
    }
    if exp_field > max_exp {
        st.flag(OFC | IXC);
        let to_inf = match st.rmode() {
            Round::Nearest | Round::TiesAway => true,
            Round::PlusInf => !neg,
            Round::MinusInf => neg,
            Round::Zero => false,
        };
        exp_field = if to_inf { 31 } else { 30 };
        mant = if to_inf { 0 } else { 0x3ff };
    }
    if st.fz() && exp_field == 0 && !ahp {
        // FZ does not apply to half precision outputs in ARMv8.0 (FZ16 does);
        // keep the denormal.
    }
    sign | ((exp_field as u16) << 10) | mant as u16
}

// ------------------------------------------------- reciprocal estimates

/// FRECPE estimate for a 9-bit input (UnsignedRecipEstimate / RecipEstimate).
fn recip_estimate(a: u64) -> u64 {
    // a in [256, 511] representing 0.5 <= x < 1.0
    let a = a * 2 + 1;
    let b = (1u64 << 19) / a;
    b.div_ceil(2) // in [256, 511]
}

/// FRECPE (single/double).
pub fn recip_est<F: Float>(st: &mut Fpst, x: F) -> F {
    let x = flush_in(st, x);
    if x.is_nan() {
        return process_nan(st, x);
    }
    let sign = x.bits() & F::SIGN;
    if x.is_inf() {
        return F::from_bits64(sign);
    }
    if x.is_zero() {
        st.flag(DZC);
        return F::from_bits64(sign | F::INF);
    }
    let ebits = F::BITS - 1 - F::MANT;
    let bias = (1i64 << (ebits - 1)) - 1;
    let bits = x.bits();
    let mut exp = ((bits >> F::MANT) & crate::bits::ones(ebits)) as i64;
    let mut frac = bits & crate::bits::ones(F::MANT);
    // Very small inputs overflow to infinity / max.
    let tiny_limit = if F::BITS == 32 { 1u64 << 21 } else { 1u64 << 50 };
    if exp == 0 && frac < tiny_limit {
        st.flag(OFC | IXC);
        let r: F = overflow_result(sign != 0, st.rmode());
        return r;
    }
    if st.fz() && exp >= 2 * bias - 1 {
        // The result would be subnormal: flushed to zero under FZ.
        st.flag(UFC);
        return F::from_bits64(sign);
    }
    if exp == 0 {
        if frac & (1u64 << (F::MANT - 1)) == 0 {
            exp -= 1;
            frac = (frac << 2) & crate::bits::ones(F::MANT);
        } else {
            frac = (frac << 1) & crate::bits::ones(F::MANT);
        }
    }
    let scaled = (1u64 << 8) | (frac >> (F::MANT - 8)); // 9 bits: 1.fraction[top 8]
    let result_exp = 2 * bias - 1 - exp;
    let est = recip_estimate(scaled); // in [256, 511]
    let mut rfrac = (est & 0xff) << (F::MANT - 8);
    let mut rexp = result_exp;
    if rexp == 0 {
        rfrac = ((1u64 << F::MANT) | rfrac) >> 1;
    } else if rexp == -1 {
        rfrac = ((1u64 << F::MANT) | rfrac) >> 2;
        rexp = 0;
    }
    if st.fz() && rexp == 0 {
        st.flag(UFC);
        return F::from_bits64(sign);
    }
    F::from_bits64(sign | ((rexp as u64) << F::MANT) | rfrac)
}

/// FRSQRTE (single/double).
pub fn rsqrt_est<F: Float>(st: &mut Fpst, x: F) -> F {
    let x = flush_in(st, x);
    if x.is_nan() {
        return process_nan(st, x);
    }
    if x.is_zero() {
        st.flag(DZC);
        return F::from_bits64((x.bits() & F::SIGN) | F::INF);
    }
    if x.sign() {
        return invalid(st);
    }
    if x.is_inf() {
        return F::zero();
    }
    let ebits = F::BITS - 1 - F::MANT;
    let bias = (1i64 << (ebits - 1)) - 1;
    let bits = x.bits();
    let mut exp = ((bits >> F::MANT) & crate::bits::ones(ebits)) as i64;
    let mut frac = bits & crate::bits::ones(F::MANT);
    if exp == 0 {
        while frac & (1u64 << (F::MANT - 1)) == 0 {
            frac <<= 1;
            exp -= 1;
        }
        frac = (frac << 1) & crate::bits::ones(F::MANT);
    }
    let scaled = if exp & 1 == 0 {
        (1u64 << 8) | (frac >> (F::MANT - 8))
    } else {
        (1u64 << 7) | (frac >> (F::MANT - 7))
    };
    let result_exp = (3 * bias - 1 - exp) / 2;
    let est = rsqrt_estimate_exact(scaled);
    let rfrac = (est & 0xff) << (F::MANT - 8);
    F::from_bits64(((result_exp as u64) << F::MANT) | rfrac)
}

/// RecipSqrtEstimate as in the Arm ARM (integer form).
fn rsqrt_estimate_exact(a: u64) -> u64 {
    // Units of 1/512 below 0.5, of 1/256 (bottom bit dropped) above.
    let a = if a < 256 { a * 2 + 1 } else { (((a >> 1) << 1) + 1) * 2 };
    let mut b = 512u64;
    while a * (b + 1) * (b + 1) < (1u64 << 28) {
        b += 1;
    }
    b.div_ceil(2)
}

/// FRECPX: reciprocal exponent.
pub fn recpx<F: Float>(st: &mut Fpst, x: F) -> F {
    let x = flush_in(st, x);
    if x.is_nan() {
        return process_nan(st, x);
    }
    let ebits = F::BITS - 1 - F::MANT;
    let sign = x.bits() & F::SIGN;
    let exp = (x.bits() >> F::MANT) & crate::bits::ones(ebits);
    let max_exp = crate::bits::ones(ebits) - 1;
    let e = if exp == 0 { max_exp } else { !exp & crate::bits::ones(ebits) };
    F::from_bits64(sign | (e << F::MANT))
}

/// FRECPS: 2 - a*b (fused). The first operand is negated before NaN
/// processing, as in FPRecipStepFused.
pub fn recip_step<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let a = F::from_bits64(a.bits() ^ F::SIGN);
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    let two = F::from_bits64(if F::BITS == 32 { 0x4000_0000 } else { 0x4000_0000_0000_0000 });
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        return two;
    }
    muladd(st, two, a, b)
}

/// FRSQRTS: (3 - a*b) / 2 (fused), first operand negated as in FPRSqrtStepFused.
pub fn rsqrt_step<F: Float>(st: &mut Fpst, a: F, b: F) -> F {
    let a = F::from_bits64(a.bits() ^ F::SIGN);
    let (a, b) = (flush_in(st, a), flush_in(st, b));
    let one_half = F::from_bits64(if F::BITS == 32 { 0x3fc0_0000 } else { 0x3ff8_0000_0000_0000 });
    let three = F::from_bits64(if F::BITS == 32 { 0x4040_0000 } else { 0x4008_0000_0000_0000 });
    if let Some(n) = process_nans(st, a, b) {
        return n;
    }
    if (a.is_inf() && b.is_zero()) || (a.is_zero() && b.is_inf()) {
        return one_half;
    }
    // (3 - a*b) / 2 is one fused operation: compute 1.5 - (a/2)*b, halving
    // whichever operand halves exactly (a normal number above the minimum
    // exponent), so overflow and rounding happen once, on the final value.
    let ebits = F::BITS - 1 - F::MANT;
    let exp = |x: F| (x.bits() >> F::MANT) & crate::bits::ones(ebits);
    let half = F::from_bits64(if F::BITS == 32 { 0x3f00_0000 } else { 0x3fe0_0000_0000_0000 });
    let mut tmp = 0u64;
    let mut exact = Fpst {
        fpcr: 0,
        fpsr: &mut tmp,
    };
    if exp(a) > 1 && !a.is_inf() {
        let ah = mul(&mut exact, a, half);
        return muladd(st, one_half, ah, b);
    }
    if exp(b) > 1 && !b.is_inf() {
        let bh = mul(&mut exact, b, half);
        return muladd(st, one_half, a, bh);
    }
    // Both operands tiny: the product cannot affect overflow.
    let r = muladd(st, three, a, b);
    if r.is_nan() || r.is_inf() || r.is_zero() {
        return r;
    }
    mul(st, r, half)
}

#[cfg(test)]
mod tests {
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
}

/// The interpreter's floating point is the oracle the JITs are fuzzed against: here it is checked
/// against the host's own FPU (an AArch64 host is the architecture), bit for bit and flag for
/// flag, over random operands and every FPCR mode Linux or a program can set.
#[cfg(all(test, target_arch = "aarch64"))]
#[allow(unsafe_code)]
mod host_fpu {
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
}
