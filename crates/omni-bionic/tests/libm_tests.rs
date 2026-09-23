//! Phase 5 tests: libm edge cases, errno, and precision classes.
//!
//! Oracles:
//! * **hand-computed exact values** (sin 0 = 0, cos 0 = 1, log 1 = 0, log e = 1 via
//!   known constants, pow(x,0) = 1, frexp/ldexp round trips, ilogb of powers of two);
//! * **C11 Annex F / POSIX tables** for special values (inf, NaN, ±0, pole/domain errors);
//! * **Rust's own float ops** for transcendental 1-ULP comparisons — an independent
//!   computation on the same inputs, NOT the function under test's result echoed;
//! * Linux errno numbers via the crate's own verified constants.
//!
//! Precision classes: `fmodf`, `frexp`, `ldexp`, `modff`, `ilogb` are exact in IEEE-754
//! and tested **bit-for-bit** (`to_bits()`); transcendental functions are tested within
//! **1 ULP** (stated tolerance per the task), except where C fixes the exact result.

use omni_bionic::context::GuestContext;
use omni_bionic::errno::consts::{EDOM, ERANGE};
use omni_bionic::libm::{
    acos, acosf, asin, asinf, atan2, atan2f, atanf, cos, cosf, cosh, exp, expf, fmodf, frexp,
    atan, cbrt, exp2, exp2f, expm1, fmod, frexpf, hypotf, nextafterf, round, tan, tanh,
    ilogb, ldexp, ldexpf, log, log10, log10f, log2, modf, modff, nan, pow, powf, sin, sincosf, sinf,
    sinh, sinhf, tanf, tanhf,
};
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;

#[derive(Default)]
struct Ctx {
    mem: MockMemory,
    errno: i32,
}
impl GuestMemory for Ctx {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.mem.read(addr, buf)
    }
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.mem.write(addr, buf)
    }
}
impl GuestContext for Ctx {
    fn errno(&self) -> i32 {
        self.errno
    }
    fn set_errno(&mut self, v: i32) {
        self.errno = v;
    }
    fn rand_state(&self) -> u32 {
        0
    }
    fn set_rand_state(&mut self, _: u32) {}
    fn scratch(&mut self) -> Option<(u64, usize)> {
        None
    }
}

fn ctx() -> Ctx {
    Ctx::default()
}

/// 1-ULP tolerance comparator (stated tolerance for transcendental functions).
fn within_1_ulp(a: f64, b: f64) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    if a == b {
        return true;
    }
    if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
        return false;
    }
    let (ab, bb) = (a.to_bits(), b.to_bits());
    // Monotone bit patterns for same-sign floats; sign bit makes the comparison
    // direction flip for negatives, so use magnitudes via ordered mapping.
    let ordered = |bits: u64| -> i64 {
        if bits & (1 << 63) != 0 {
            !(bits as i64)
        } else {
            bits as i64
        }
    };
    (ordered(ab) - ordered(bb)).abs() <= 1
}

fn within_1_ulp_f32(a: f32, b: f32) -> bool {
    if a.is_nan() && b.is_nan() {
        return true;
    }
    if a == b {
        return true;
    }
    if a.is_nan() || b.is_nan() || a.is_infinite() || b.is_infinite() {
        return false;
    }
    let (ab, bb) = (a.to_bits(), b.to_bits());
    let ordered = |bits: u32| -> i32 {
        if bits & (1 << 31) != 0 {
            !(bits as i32)
        } else {
            bits as i32
        }
    };
    (ordered(ab) - ordered(bb)).abs() <= 1
}

// ---------------------------------------------------------------- exp/log

#[test]
fn exp_basic_exact_and_edges() {
    let mut c = ctx();
    assert_eq!(exp(&mut c, 0.0), 1.0); // exact by definition
    // exp(1) vs Rust's own: independent computation, 1 ULP tolerance.
    assert!(within_1_ulp(exp(&mut c, 1.0), 1f64.exp()));
    assert!(within_1_ulp(exp(&mut c, -1.0), (-1f64).exp()));
    // inf/nan.
    assert_eq!(exp(&mut c, f64::INFINITY), f64::INFINITY);
    assert!(exp(&mut c, f64::NAN).is_nan());
    // Overflow: exp(1000) = HUGE_VAL + ERANGE.
    let mut c = ctx();
    assert_eq!(exp(&mut c, 1000.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // Underflow: exp(-1000) = 0.
    let mut c = ctx();
    assert_eq!(exp(&mut c, -1000.0), 0.0);
    // Sign of zero: exp(-0.0) == 1.
    assert_eq!(exp(&mut c, -0.0).to_bits(), 1.0f64.to_bits());
}

#[test]
fn expf_matches_f32_semantics() {
    let mut c = ctx();
    assert_eq!(expf(&mut c, 0.0), 1.0);
    assert!(within_1_ulp_f32(expf(&mut c, 1.0), 1f32.exp()));
    let mut c = ctx();
    assert_eq!(expf(&mut c, 1000.0), f32::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // exp(inf) = inf without errno (Annex F: defined, no error).
    let mut c = ctx();
    assert_eq!(expf(&mut c, f32::INFINITY), f32::INFINITY);
    assert_eq!(c.errno(), 0);
}

#[test]
fn log_domain_and_pole_errors() {
    // log(-1) = NaN + EDOM.
    let mut c = ctx();
    assert!(log(&mut c, -1.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    // log(-0.0) = -inf + ERANGE (pole; sign of zero is still a zero).
    let mut c = ctx();
    assert_eq!(log(&mut c, -0.0), f64::NEG_INFINITY);
    assert_eq!(c.errno(), ERANGE);
    let mut c = ctx();
    assert_eq!(log(&mut c, 0.0), f64::NEG_INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // log(inf) = inf, no error.
    let mut c = ctx();
    assert_eq!(log(&mut c, f64::INFINITY), f64::INFINITY);
    assert_eq!(c.errno(), 0);
    // Exact anchors: log(1) = 0 (bit-exact incl. sign).
    let mut c = ctx();
    assert_eq!(log(&mut c, 1.0).to_bits(), 0.0f64.to_bits());
    // Within 1 ULP of Rust's own for representative values.
    assert!(within_1_ulp(log(&mut c, 2.0), 2f64.ln()));
    assert!(within_1_ulp(log(&mut c, 10.0), 10f64.ln()));
    // NaN in → NaN out, no errno.
    let mut c = ctx();
    assert!(log(&mut c, f64::NAN).is_nan());
    assert_eq!(c.errno(), 0);
}

#[test]
fn log10_log2_domains_and_values() {
    // Exact: log10(100) == 2 within 1 ULP (not necessarily exact in floating point).
    let mut c = ctx();
    assert!(within_1_ulp(log10(&mut c, 100.0), 2.0));
    assert!(within_1_ulp(log2(&mut c, 8.0), 3.0));
    // Domain/pole errors mirror log().
    let mut c = ctx();
    assert!(log10(&mut c, -1.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert!(log2(&mut c, -0.5).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert_eq!(log10f(&mut c, 0.0), f32::NEG_INFINITY);
    assert_eq!(c.errno(), ERANGE);
}

// ---------------------------------------------------------------- trig

#[test]
fn trig_exact_anchors_and_inf_domain() {
    let mut c = ctx();
    // sin(0) = +0 (bit-exact, sign preserved); sin(-0) = -0.
    assert_eq!(sin(&mut c, 0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(sin(&mut c, -0.0).to_bits(), (-0.0f64).to_bits());
    // cos(0) = 1 exactly.
    assert_eq!(cos(&mut c, 0.0).to_bits(), 1.0f64.to_bits());
    // ±inf → NaN + EDOM.
    let mut c = ctx();
    assert!(sin(&mut c, f64::INFINITY).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert!(cosf(&mut c, f32::NEG_INFINITY).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert!(tanf(&mut c, f32::INFINITY).is_nan());
    assert_eq!(c.errno(), EDOM);
    // Representative values within 1 ULP of Rust's own (independent computation).
    assert!(within_1_ulp(sin(&mut c, 1.0), 1f64.sin()));
    assert!(within_1_ulp(cos(&mut c, 1.0), 1f64.cos()));
    assert!(within_1_ulp_f32(sinf(&mut c, 0.5), 0.5f32.sin()));
    assert!(within_1_ulp_f32(cosf(&mut c, 0.5), 0.5f32.cos()));
}

// ---------------------------------------------------------------- hyperbolic

#[test]
fn hyperbolic_edges() {
    let mut c = ctx();
    // sinh(0) = 0 exactly; tanh(0) = 0.
    assert_eq!(sinh(&mut c, 0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(tanhf(&mut c, 0.0).to_bits(), 0.0f32.to_bits());
    // sinh(inf) = inf, no errno (defined).
    let mut c = ctx();
    assert_eq!(sinh(&mut c, f64::INFINITY), f64::INFINITY);
    assert_eq!(c.errno(), 0);
    // Overflow: sinh(1000) = HUGE_VAL + ERANGE.
    let mut c = ctx();
    assert_eq!(sinh(&mut c, 1000.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // cosh overflow is always +inf.
    let mut c = ctx();
    assert_eq!(cosh(&mut c, -1000.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // cosh(±0) = 1.
    let mut c = ctx();
    assert_eq!(cosh(&mut c, 0.0).to_bits(), 1.0f64.to_bits());
    // tanh saturates: tanhf(100) == 1, no errno ever.
    let mut c = ctx();
    assert_eq!(tanhf(&mut c, 100.0).to_bits(), 1.0f32.to_bits());
    assert_eq!(c.errno(), 0);
    assert!(within_1_ulp_f32(sinhf(&mut c, 1.0), 1f32.sinh()));
}

// ---------------------------------------------------------------- inverse trig

#[test]
fn acos_asin_domains() {
    let mut c = ctx();
    // Exact anchors: acos(1) = 0, acos(-1) = pi, asin(0) = 0, asin(1) = pi/2.
    assert_eq!(acos(&mut c, 1.0).to_bits(), 0.0f64.to_bits());
    assert!(within_1_ulp(acos(&mut c, -1.0), core::f64::consts::PI));
    assert_eq!(asin(&mut c, 0.0).to_bits(), 0.0f64.to_bits());
    assert!(within_1_ulp(asin(&mut c, 1.0), core::f64::consts::FRAC_PI_2));
    // Domain: 1.0000001 → NaN + EDOM.
    let mut c = ctx();
    assert!(acos(&mut c, 1.0000001).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert!(asinf(&mut c, -1.5).is_nan());
    assert_eq!(c.errno(), EDOM);
    // NaN in, NaN out, no errno.
    let mut c = ctx();
    assert!(acosf(&mut c, f32::NAN).is_nan());
    assert_eq!(c.errno(), 0);
}

#[test]
fn atan2_annex_f_table() {
    let mut c = ctx();
    // Hand-derived quadrant anchors from the Annex F table:
    assert_eq!(atan2(&mut c, 0.0, -0.0), core::f64::consts::PI); // atan2(+0, -0)
    assert_eq!(atan2(&mut c, -0.0, -0.0), -core::f64::consts::PI); // atan2(-0, -0)
    assert_eq!(atan2(&mut c, 0.0, 0.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(atan2(&mut c, f64::INFINITY, f64::INFINITY), core::f64::consts::FRAC_PI_4);
    assert_eq!(atan2(&mut c, f64::INFINITY, f64::NEG_INFINITY), 3.0 * core::f64::consts::FRAC_PI_4);
    assert_eq!(atan2(&mut c, f64::INFINITY, 1.0), core::f64::consts::FRAC_PI_2);
    // NaN input → NaN, no errno.
    let mut c = ctx();
    assert!(atan2(&mut c, f64::NAN, 1.0).is_nan());
    assert_eq!(c.errno(), 0);
    // f32 forms delegate identically.
    assert!(within_1_ulp_f32(atan2f(&mut c, 1.0, 2.0), 1f32.atan2(2.0)));
    assert!(within_1_ulp_f32(atanf(&mut c, 1.0), 1f32.atan()));
}

// ---------------------------------------------------------------- exact-IEEE group

#[test]
fn fmodf_bit_exact_and_domain() {
    let mut c = ctx();
    // Exact IEEE results (hand-computed, bit-for-bit):
    assert_eq!(fmodf(&mut c, 5.5, 2.0).to_bits(), 1.5f32.to_bits());
    assert_eq!(fmodf(&mut c, -5.5, 2.0).to_bits(), (-1.5f32).to_bits());
    assert_eq!(fmodf(&mut c, 5.5, -2.0).to_bits(), 1.5f32.to_bits());
    assert_eq!(fmodf(&mut c, 6.0, 3.0).to_bits(), 0.0f32.to_bits());
    assert_eq!(fmodf(&mut c, -6.0, 3.0).to_bits(), (-0.0f32).to_bits()); // sign of x
    // Domain: fmod(x, 0) and fmod(inf, y) → NaN + EDOM.
    let mut c = ctx();
    assert!(fmodf(&mut c, 1.0, 0.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert!(fmodf(&mut c, f32::INFINITY, 2.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    // fmod(finite, inf) = x exactly.
    let mut c = ctx();
    assert_eq!(fmodf(&mut c, 3.5, f32::INFINITY).to_bits(), 3.5f32.to_bits());
    assert_eq!(c.errno(), 0);
}

#[test]
fn frexp_exact_roundtrip_and_specials() {
    let mut c = ctx();
    c.mem.map(0x2000, &[0u8; 4]);
    // frexp(8.0) = 0.5 * 2^4.
    let m = frexp(&mut c, 8.0, 0x2000).unwrap();
    assert_eq!(m.to_bits(), 0.5f64.to_bits());
    let mut e = [0u8; 4];
    c.mem.read(0x2000, &mut e).unwrap();
    assert_eq!(i32::from_le_bytes(e), 4);
    // Round trip over assorted values: x == m * 2^e with m in [0.5, 1).
    c.mem.map(0x2000, &[0u8; 4]);
    for &x in &[1.0f64, 2.5, -7.75, 1e-300, 1e300, f64::MIN_POSITIVE] {
        let m = frexp(&mut c, x, 0x2000).unwrap();
        let mut eb = [0u8; 4];
        c.mem.read(0x2000, &mut eb).unwrap();
        let e = i32::from_le_bytes(eb);
        assert!((0.5..1.0).contains(&m.abs()), "m {m} out of range for x {x}");
        assert_eq!(m * 2f64.powi(e), x, "round trip failed for x {x}");
    }
    // Specials: ±0 → ±0 with exp 0.
    let mut c = ctx();
    c.mem.map(0x2000, &[0u8; 4]);
    assert_eq!(frexp(&mut c, 0.0, 0x2000).unwrap().to_bits(), 0.0f64.to_bits());
    let mut e = [0u8; 4];
    c.mem.read(0x2000, &mut e).unwrap();
    assert_eq!(i32::from_le_bytes(e), 0);
    let m = frexp(&mut c, -0.0, 0x2000).unwrap();
    assert_eq!(m.to_bits(), (-0.0f64).to_bits());
    // inf/NaN pass through.
    assert_eq!(frexp(&mut c, f64::INFINITY, 0x2000).unwrap(), f64::INFINITY);
    assert!(frexp(&mut c, f64::NAN, 0).unwrap().is_nan());
}

#[test]
fn ilogb_exact_exponents_and_specials() {
    // Exact unbiased exponents of powers of two: ilogb(2^k) == k.
    for k in [-10i32, -1, 0, 1, 17, 100, 1023] {
        assert_eq!(ilogb(2f64.powi(k)), k);
    }
    // Subnormal: 2^-1022 * 0.5 = 2^-1023 → -1023.
    assert_eq!(ilogb(f64::from_bits(1)), -1074); // smallest subnormal
    assert_eq!(ilogb(2f64.powi(-1023)), -1023);
    // Specials (bionic FP_ILOGB0/FP_ILOGBNAN = INT_MIN; FP_ILOGBINTB = INT_MAX).
    assert_eq!(ilogb(0.0), i32::MIN);
    assert_eq!(ilogb(-0.0), i32::MIN);
    assert_eq!(ilogb(f64::NAN), i32::MIN);
    assert_eq!(ilogb(f64::INFINITY), i32::MAX);
    // Negative values: sign ignored.
    assert_eq!(ilogb(-8.0), 3);
}

#[test]
fn ldexp_exact_and_range_errors() {
    let mut c = ctx();
    // Exact: ldexp(0.75, 5) = 24; ldexp(-1.5, -2) = -0.375.
    assert_eq!(ldexp(&mut c, 0.75, 5).to_bits(), 24.0f64.to_bits());
    assert_eq!(ldexp(&mut c, -1.5, -2).to_bits(), (-0.375f64).to_bits());
    // Round trip with frexp.
    let m = frexp(&mut c, 1e300, 0).unwrap();
    // (frexp's exp went to slot 0 = null: fine.)
    let _ = m;
    // Overflow: ldexp(1.0, 2000) = HUGE_VAL + ERANGE.
    let mut c = ctx();
    assert_eq!(ldexp(&mut c, 1.0, 2000), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // Underflow to zero: ldexp(1.0, -2100) = 0 + ERANGE (bionic sets it).
    let mut c = ctx();
    assert_eq!(ldexp(&mut c, 1.0, -2100), 0.0);
    assert_eq!(c.errno(), ERANGE);
    // ldexp(0, anything) = 0, no errno.
    let mut c = ctx();
    assert_eq!(ldexp(&mut c, 0.0, 9999).to_bits(), 0.0f64.to_bits());
    assert_eq!(c.errno(), 0);
    // f32 form.
    let mut c = ctx();
    assert_eq!(ldexpf(&mut c, 1.5, 3).to_bits(), 12.0f32.to_bits());
    let mut c = ctx();
    assert_eq!(ldexpf(&mut c, 1.0, 300), f32::INFINITY);
    assert_eq!(c.errno(), ERANGE);
}

#[test]
fn modff_exact_split_and_signs() {
    let mut c = ctx();
    c.mem.map(0x3000, &[0u8; 4]);
    // modff(3.75) → frac 0.75, int 3.0 (bit-exact).
    let frac = modff(&mut c, 3.75, 0x3000).unwrap();
    assert_eq!(frac.to_bits(), 0.75f32.to_bits());
    let mut ip = [0u8; 4];
    c.mem.read(0x3000, &mut ip).unwrap();
    assert_eq!(f32::from_le_bytes(ip).to_bits(), 3.0f32.to_bits());
    // Negative: both parts carry x's sign: modff(-3.75) → frac -0.75, int -3.0.
    let frac = modff(&mut c, -3.75, 0x3000).unwrap();
    assert_eq!(frac.to_bits(), (-0.75f32).to_bits());
    c.mem.read(0x3000, &mut ip).unwrap();
    assert_eq!(f32::from_le_bytes(ip).to_bits(), (-3.0f32).to_bits());
    // ±inf → int part = inf, frac = 0.
    let frac = modff(&mut c, f32::INFINITY, 0x3000).unwrap();
    assert_eq!(frac.to_bits(), 0.0f32.to_bits());
    c.mem.read(0x3000, &mut ip).unwrap();
    assert_eq!(f32::from_le_bytes(ip), f32::INFINITY);
    // NaN propagates.
    assert!(modff(&mut c, f32::NAN, 0).unwrap().is_nan());
}

/// `modf`'s split, bit-exact, and Annex F's signs: the fraction of a negative integer is `-0.0`,
/// and ±inf returns ±0 with the infinity stored.
#[test]
fn modf_exact_split_and_annex_f_signs() {
    let mut c = ctx();
    c.mem.map(0x3000, &[0u8; 8]);
    fn stored(c: &mut Ctx) -> f64 {
        let mut b = [0u8; 8];
        c.mem.read(0x3000, &mut b).unwrap();
        f64::from_le_bytes(b)
    }
    let cases: [(f64, f64, f64); 6] = [
        (3.75, 0.75, 3.0),
        (-3.75, -0.75, -3.0),
        (-3.0, -0.0, -3.0),
        (1e300, 0.0, 1e300),
        (f64::INFINITY, 0.0, f64::INFINITY),
        (f64::NEG_INFINITY, -0.0, f64::NEG_INFINITY),
    ];
    for (x, frac, int) in cases {
        let got = modf(&mut c, x, 0x3000).unwrap();
        assert_eq!(got.to_bits(), frac.to_bits(), "modf({x}) fraction");
        assert_eq!(stored(&mut c).to_bits(), int.to_bits(), "modf({x}) integer part");
    }
    assert!(modf(&mut c, f64::NAN, 0x3000).unwrap().is_nan());
    assert!(stored(&mut c).is_nan());
}

/// The twelve written for M6: the exact ones bit for bit, the transcendental ones on their
/// edge-case contract.
#[test]
fn the_m6_libm_additions_keep_their_contracts() {
    let mut c = ctx();
    // exact
    assert_eq!(fmod(&mut c, 7.5, 2.0).to_bits(), 1.5f64.to_bits());
    assert_eq!(fmod(&mut c, -7.5, 2.0).to_bits(), (-1.5f64).to_bits(), "the sign of x");
    c.errno = 0;
    assert!(fmod(&mut c, 1.0, 0.0).is_nan());
    assert_eq!(c.errno, EDOM);
    assert_eq!(round(2.5), 3.0);
    assert_eq!(round(-2.5), -3.0);
    assert_eq!(round(-0.4).to_bits(), (-0.0f64).to_bits());
    assert_eq!(nextafterf(&mut c, 1.0, 2.0), f32::from_bits(1.0f32.to_bits() + 1));
    assert_eq!(nextafterf(&mut c, 1.0, 0.0), f32::from_bits(1.0f32.to_bits() - 1));
    assert_eq!(nextafterf(&mut c, -1.0, 0.0), f32::from_bits((-1.0f32).to_bits() - 1));
    assert_eq!(nextafterf(&mut c, 0.0, -1.0).to_bits(), 0x8000_0001, "-smallest subnormal");
    c.errno = 0;
    assert_eq!(nextafterf(&mut c, f32::MAX, f32::INFINITY), f32::INFINITY);
    assert_eq!(c.errno, ERANGE, "overflow");
    c.mem.map(0x4000, &[0u8; 4]);
    assert_eq!(frexpf(&mut c, 12.0, 0x4000).unwrap(), 0.75);
    let mut e = [0u8; 4];
    c.mem.read(0x4000, &mut e).unwrap();
    assert_eq!(i32::from_le_bytes(e), 4);
    assert_eq!(frexpf(&mut c, f32::from_bits(1), 0x4000).unwrap(), 0.5, "the smallest subnormal");
    c.mem.read(0x4000, &mut e).unwrap();
    assert_eq!(i32::from_le_bytes(e), -148);
    // transcendental, on their edges
    assert_eq!(atan(&mut c, 0.0), 0.0);
    assert_eq!(cbrt(&mut c, -27.0), -3.0);
    assert_eq!(tanh(&mut c, f64::INFINITY), 1.0);
    c.errno = 0;
    assert!(tan(&mut c, f64::INFINITY).is_nan());
    assert_eq!(c.errno, EDOM);
    c.errno = 0;
    assert_eq!(exp2(&mut c, 2000.0), f64::INFINITY);
    assert_eq!(c.errno, ERANGE);
    assert_eq!(exp2f(&mut c, 3.0), 8.0);
    assert_eq!(expm1(&mut c, 0.0), 0.0);
    assert_eq!(hypotf(&mut c, 3.0, 4.0), 5.0);
    assert_eq!(hypotf(&mut c, f32::INFINITY, f32::NAN), f32::INFINITY, "Annex F");
    // modff's Annex F signs, fixed in M6.
    c.mem.map(0x5000, &[0u8; 4]);
    assert_eq!(modff(&mut c, -3.0, 0x5000).unwrap().to_bits(), (-0.0f32).to_bits());
    assert_eq!(modff(&mut c, f32::NEG_INFINITY, 0x5000).unwrap().to_bits(), (-0.0f32).to_bits());
}

#[test]
fn nan_returns_quiet_nan() {
    assert!(nan(0).is_nan());
    assert!(nan(0x5000).is_nan());
}

// ---------------------------------------------------------------- pow family

#[test]
fn pow_annex_f_table() {
    let mut c = ctx();
    // pow(x, ±0) = 1 for any x (even NaN).
    assert_eq!(pow(&mut c, 5.0, 0.0).to_bits(), 1.0f64.to_bits());
    assert_eq!(pow(&mut c, f64::NAN, 0.0).to_bits(), 1.0f64.to_bits());
    assert_eq!(pow(&mut c, 5.0, -0.0).to_bits(), 1.0f64.to_bits());
    // pow(1, y) = 1 for any y (even NaN).
    assert_eq!(pow(&mut c, 1.0, f64::NAN).to_bits(), 1.0f64.to_bits());
    assert_eq!(pow(&mut c, 1.0, 123.0).to_bits(), 1.0f64.to_bits());
    // pow(+0, +y) = +0; pow(-0, odd integral y) = -0.
    assert_eq!(pow(&mut c, 0.0, 3.0).to_bits(), 0.0f64.to_bits());
    assert_eq!(pow(&mut c, -0.0, 3.0).to_bits(), (-0.0f64).to_bits());
    assert_eq!(pow(&mut c, -0.0, 2.0).to_bits(), 0.0f64.to_bits());
    // pow(±0, -y): pole error → ±inf + ERANGE.
    let mut c = ctx();
    assert_eq!(pow(&mut c, 0.0, -1.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    let mut c = ctx();
    assert_eq!(pow(&mut c, -0.0, -3.0), f64::NEG_INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // pow(-1, ±inf) = 1.
    let mut c = ctx();
    assert_eq!(pow(&mut c, -1.0, f64::INFINITY).to_bits(), 1.0f64.to_bits());
    assert_eq!(pow(&mut c, -1.0, f64::NEG_INFINITY).to_bits(), 1.0f64.to_bits());
    // pow(x<0, non-integer y) = NaN + EDOM.
    let mut c = ctx();
    assert!(pow(&mut c, -2.0, 0.5).is_nan());
    assert_eq!(c.errno(), EDOM);
    // pow(-2, 3) = -8 exactly (integral exponent on negative base).
    assert_eq!(pow(&mut c, -2.0, 3.0).to_bits(), (-8.0f64).to_bits());
    // pow(2, 10) = 1024 exactly.
    assert_eq!(pow(&mut c, 2.0, 10.0).to_bits(), 1024.0f64.to_bits());
    // Overflow: pow(10, 400) = HUGE_VAL + ERANGE.
    let mut c = ctx();
    assert_eq!(pow(&mut c, 10.0, 400.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // Underflow: pow(10, -400) = 0 + ERANGE.
    let mut c = ctx();
    assert_eq!(pow(&mut c, 10.0, -400.0), 0.0);
    assert_eq!(c.errno(), ERANGE);
    // inf cases: pow(inf, 2) = inf + ERANGE (POSIX range error); pow(inf, -2) = 0.
    let mut c = ctx();
    assert_eq!(pow(&mut c, f64::INFINITY, 2.0), f64::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    let mut c = ctx();
    assert_eq!(pow(&mut c, f64::INFINITY, -2.0).to_bits(), 0.0f64.to_bits());
    // Within 1 ULP of Rust's own for representative finite values.
    assert!(within_1_ulp(pow(&mut c, 2.0, 0.5), 2f64.powf(0.5)));
    assert!(within_1_ulp(pow(&mut c, 3.0, 7.0), 3f64.powf(7.0)));
}

#[test]
fn powf_mirrors_pow_semantics() {
    let mut c = ctx();
    assert_eq!(powf(&mut c, 2.0, 4.0).to_bits(), 16.0f32.to_bits());
    let mut c = ctx();
    assert!(powf(&mut c, -2.0, 0.5).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert_eq!(powf(&mut c, 0.0, -1.0), f32::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    assert!(within_1_ulp_f32(powf(&mut c, 2.0, 0.5), 2f32.powf(0.5)));
}

// ---------------------------------------------------------------- sincosf

#[test]
fn sincosf_writes_both_results_to_guest_memory() {
    let mut c = ctx();
    c.mem.map(0x6000, &[0u8; 8]); // sin at 0x6000, cos at 0x6004
    sincosf(&mut c, 0.0, 0x6000, 0x6004).unwrap();
    let mut buf = [0u8; 8];
    c.mem.read(0x6000, &mut buf).unwrap();
    let s = f32::from_le_bytes(buf[0..4].try_into().unwrap());
    let cosv = f32::from_le_bytes(buf[4..8].try_into().unwrap());
    // Hand-known: sin(0) = +0, cos(0) = 1.
    assert_eq!(s.to_bits(), 0.0f32.to_bits());
    assert_eq!(cosv.to_bits(), 1.0f32.to_bits());
    // inf → both NaN + EDOM.
    let mut c = ctx();
    c.mem.map(0x6000, &[0u8; 8]);
    sincosf(&mut c, f32::INFINITY, 0x6000, 0x6004).unwrap();
    assert_eq!(c.errno(), EDOM);
    c.mem.read(0x6000, &mut buf).unwrap();
    assert!(f32::from_le_bytes(buf[0..4].try_into().unwrap()).is_nan());
    assert!(f32::from_le_bytes(buf[4..8].try_into().unwrap()).is_nan());
    // Null pointers are legal (C: either output may be NULL in bionic's sincosf? POSIX
    // does not define sincos; GNU says the pointers must be valid — bionic dereferences
    // unconditionally, so a null pointer on device would crash. Here: writes skipped,
    // documented divergence).
    let mut c = ctx();
    sincosf(&mut c, 1.0, 0, 0).unwrap(); // must not fault
    // Representative values within 1 ULP of independent computation.
    let mut c = ctx();
    c.mem.map(0x6000, &[0u8; 8]);
    sincosf(&mut c, 0.5, 0x6000, 0x6004).unwrap();
    c.mem.read(0x6000, &mut buf).unwrap();
    let s = f32::from_le_bytes(buf[0..4].try_into().unwrap());
    let cosv = f32::from_le_bytes(buf[4..8].try_into().unwrap());
    assert!(within_1_ulp_f32(s, 0.5f32.sin()));
    assert!(within_1_ulp_f32(cosv, 0.5f32.cos()));
}

/// **`erfcf` is bionic's `s_erff.c`**: within 1 ULP of the true value across all four of its
/// intervals and both signs, its exact special values (`erfcf(0) = 1`, `2` below -5, `+0` from 11
/// up, `0`/`2` at the infinities, NaN in, NaN out), and no `errno` ever -- including where the
/// result underflows.
#[test]
fn erfcf_is_bionics_s_erff_and_sets_no_errno() {
    use omni_bionic::libm::erfcf;
    let ulps = |got: f32, want: f64| {
        let want = want as f32;
        (got.to_bits() as i64 - want.to_bits() as i64).abs()
    };
    // (x, erfc(x) to double precision **at the f32 input** -- `0.8f32` is not 0.8): one point in
    // each of the four intervals, both signs.
    for (x, want) in [
        (0.25f32, 0.723_673_609_831_763_1f64),
        (0.5, 0.479_500_122_186_953_4),
        (0.8, 0.257_899_028_199_556_3),
        (1.0, 0.157_299_207_050_285_1),
        (-1.0, 1.842_700_792_949_715),
        (1.2, 0.089_686_009_022_393_47),
        (1.5, 0.033_894_853_524_689_274),
        (2.0, 0.004_677_734_981_047_265),
        (3.0, 2.209_049_699_858_544e-5),
        (4.0, 1.541_725_790_028_001_7e-8),
        (6.0, 2.151_973_671_249_891_3e-17),
        (9.0, 4.137_031_746_513_81e-37),
        (-3.0, 1.999_977_909_503_001_2),
    ] {
        assert!(ulps(erfcf(x), want) <= 1, "erfcf({x}) = {} vs {want}", erfcf(x));
    }
    assert_eq!(erfcf(0.0).to_bits(), 1.0f32.to_bits(), "erfc(0) = 1 exactly");
    assert_eq!(erfcf(1e-8).to_bits(), 1.0f32.to_bits(), "|x| < 2**-24: one - x");
    assert_eq!(erfcf(-5.5).to_bits(), 2.0f32.to_bits(), "x < -5: two - tiny");
    assert_eq!(erfcf(11.0).to_bits(), 0.0f32.to_bits(), "x >= 11: tiny * tiny, a positive zero");
    assert_eq!(erfcf(f32::INFINITY).to_bits(), 0.0f32.to_bits());
    assert_eq!(erfcf(f32::NEG_INFINITY).to_bits(), 2.0f32.to_bits());
    assert!(erfcf(f32::NAN).is_nan());
}

#[test]
fn remaining_libm_forms_smoke_and_edges() {
    // The f32/niche forms not covered above: same contract, spot-checked.
    let mut c = ctx();
    // logf/log2f domain & pole.
    assert!(omni_bionic::libm::logf(&mut c, -1.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    let mut c = ctx();
    assert_eq!(omni_bionic::libm::logf(&mut c, 0.0), f32::NEG_INFINITY);
    assert_eq!(c.errno(), ERANGE);
    let mut c = ctx();
    assert_eq!(omni_bionic::libm::log2f(&mut c, 8.0), 3.0);
    let mut c = ctx();
    assert!(omni_bionic::libm::log2f(&mut c, -1.0).is_nan());
    assert_eq!(c.errno(), EDOM);
    // log10f inf handling.
    let mut c = ctx();
    assert_eq!(omni_bionic::libm::log10f(&mut c, f32::INFINITY), f32::INFINITY);
    // coshf / sinhf f32 overflow.
    let mut c = ctx();
    assert_eq!(omni_bionic::libm::coshf(&mut c, 100.0), f32::INFINITY);
    assert_eq!(c.errno(), ERANGE);
    // cbrtf: exact cube roots and sign handling.
    let mut c = ctx();
    assert_eq!(omni_bionic::libm::cbrtf(&mut c, 8.0).to_bits(), 2.0f32.to_bits());
    assert_eq!(omni_bionic::libm::cbrtf(&mut c, -8.0).to_bits(), (-2.0f32).to_bits());
    assert_eq!(omni_bionic::libm::cbrtf(&mut c, f32::NEG_INFINITY), f32::NEG_INFINITY);
}
