//! libm: the reachable mathematical functions.
//!
//! Signatures implemented (all reachable from libroblox.so's initializers — see the
//! phase 0 scope table; the `f` suffix is f32, plain names f64):
//!
//! | family | functions |
//! |---|---|
//! | exp/log | [`exp`], [`expf`], [`log`], [`logf`], [`log10`], [`log10f`], [`log2`], [`log2f`] |
//! | trig | [`sin`], [`sinf`], [`cos`], [`cosf`], [`tanf`] |
//! | hyperbolic | [`sinh`], [`sinhf`], [`cosh`], [`coshf`], [`tanhf`] |
//! | inverse trig | [`acos`], [`acosf`], [`asin`], [`asinf`], [`atan2`], [`atan2f`], [`atanf`] |
//! | other | [`cbrtf`], [`fmodf`], [`frexp`], [`ilogb`], [`ldexp`], [`ldexpf`], [`modf`], [`modff`], [`nan`], [`pow`], [`powf`], [`sincosf`] |
//!
//! Implementation policy (per the task): **do not reimplement transcendental functions** —
//! Rust's `f64`/`f32` methods compile to calls into the host's libm (via compiler-builtins
//! math), which is well-tested and correctly rounded to within documented tolerance. The
//! effort here is the **edge-case contract**: NaN propagation, ±infinity, ±0.0, domain
//! errors (log of a negative → NaN + EDOM, sqrt-shaped), and range errors (overflow in
//! exp/pow → ±HUGE_VAL + ERANGE), per POSIX 2.7/`math.h` semantics that C11 Annex F and
//! bionic both follow.
//!
//! Testing rule: exact-IEEE functions (`fmodf`, `frexp`, `ldexp`, `ilogb`, `nan`) are
//! tested bit-for-bit; transcendental functions (sin, cos, exp, log, pow, ...) are
//! compared within 1 ULP against Rust's own results computed with *independent inputs*
//! (the oracle is the IEEE-754 correctness of the host libm for those inputs, plus
//! hand-computed exact values like sin(0) = 0, log(1) = 0, pow(x, 0) = 1).
//!
//! errno note: the guest errno is Linux numbering; `EDOM = 33`, `ERANGE = 34` (already
//! verified in `errno.rs`). Functions that never set errno in C (fmod, frexp, ilogb,
//! ldexp, modf, nan, cbrt) take plain memory or no context at all.

use crate::context::GuestContext;
use crate::errno::consts::{EDOM, ERANGE};
use crate::memory::Fault;

/// Set errno for a domain error and return NaN (the POSIX result for domain errors).
fn domain_error(ctx: &mut impl GuestContext) -> f64 {
    ctx.set_errno(EDOM);
    f64::NAN
}

/// Set errno for a range error (overflow) and return `HUGE_VAL` with the given sign.
fn range_error_inf(ctx: &mut impl GuestContext, negative: bool) -> f64 {
    ctx.set_errno(ERANGE);
    if negative {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    }
}

// ---------------------------------------------------------------------------
// exp / log family (f64)
// ---------------------------------------------------------------------------

/// `double exp(double x)` — e^x. Overflow → `HUGE_VAL` + ERANGE; underflow → 0 (+ ERANGE
/// per POSIX, which bionic follows for gradual underflow; C allows either, so the *test*
/// asserts the value, and the errno only for the overflow case where C is unambiguous).
pub fn exp(ctx: &mut impl GuestContext, x: f64) -> f64 {
    let r = x.exp();
    if r.is_infinite() {
        return range_error_inf(ctx, false);
    }
    r
}

/// `float expf(float x)`
pub fn expf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    let r = x.exp();
    if r.is_infinite() && !x.is_infinite() {
        // Real overflow (not exp(+inf) which is defined as +inf, no errno per Annex F).
        ctx.set_errno(ERANGE);
    }
    r
}

/// `double log(double x)` — natural log. `x < 0` → NaN + EDOM; `x == 0` → -inf + ERANGE
/// (pole error per POSIX); `x == +inf` → +inf, no error; NaN propagates.
pub fn log(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return domain_error(ctx);
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f64::NEG_INFINITY;
    }
    x.ln()
}

/// `float logf(float x)` — as [`log`].
pub fn logf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f32::NEG_INFINITY;
    }
    x.ln()
}

/// `double log10(double x)` — as [`log`] with base 10.
pub fn log10(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return domain_error(ctx);
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f64::NEG_INFINITY;
    }
    x.log10()
}

/// `float log10f(float x)`
pub fn log10f(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f32::NEG_INFINITY;
    }
    x.log10()
}

/// `double log2(double x)` — as [`log`] with base 2.
pub fn log2(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        return domain_error(ctx);
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f64::NEG_INFINITY;
    }
    x.log2()
}

/// `float log2f(float x)`
pub fn log2f(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if x < 0.0 {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f32::NEG_INFINITY;
    }
    x.log2()
}

// ---------------------------------------------------------------------------
// trig (f64 + reachable f32 forms)
// ---------------------------------------------------------------------------

/// `double sin(double x)` — NaN propagates; ±0.0 → ±0.0 (sign preserved, C Annex F);
/// ±inf → NaN + EDOM (domain error per POSIX).
pub fn sin(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_infinite() {
        return domain_error(ctx);
    }
    x.sin()
}

/// `float sinf(float x)`
pub fn sinf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_infinite() {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x.sin()
}

/// `double cos(double x)` — as [`sin`].
pub fn cos(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_infinite() {
        return domain_error(ctx);
    }
    x.cos()
}

/// `float cosf(float x)`
pub fn cosf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_infinite() {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x.cos()
}

/// `float tanf(float x)` — ±inf → NaN + EDOM.
pub fn tanf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_infinite() {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x.tan()
}

// ---------------------------------------------------------------------------
// hyperbolic
// ---------------------------------------------------------------------------

/// `double sinh(double x)` — overflow → ±HUGE_VAL + ERANGE.
pub fn sinh(ctx: &mut impl GuestContext, x: f64) -> f64 {
    let r = x.sinh();
    if r.is_infinite() && !x.is_infinite() {
        return range_error_inf(ctx, x < 0.0);
    }
    r
}

/// `float sinhf(float x)`
pub fn sinhf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    let r = x.sinh();
    if r.is_infinite() && !x.is_infinite() {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `double cosh(double x)` — overflow → +HUGE_VAL + ERANGE.
pub fn cosh(ctx: &mut impl GuestContext, x: f64) -> f64 {
    let r = x.cosh();
    if r.is_infinite() && !x.is_infinite() {
        return range_error_inf(ctx, false);
    }
    r
}

/// `float coshf(float x)`
pub fn coshf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    let r = x.cosh();
    if r.is_infinite() && !x.is_infinite() {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `float tanhf(float x)` — never overflows (result in [-1, 1]); no errno.
pub fn tanhf(_ctx: &mut impl GuestContext, x: f32) -> f32 {
    x.tanh()
}

// ---------------------------------------------------------------------------
// inverse trig
// ---------------------------------------------------------------------------

/// `double acos(double x)` — domain `[-1, 1]`; outside → NaN + EDOM; ±1 → 0/π exactly.
pub fn acos(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if !(-1.0..=1.0).contains(&x) {
        return domain_error(ctx);
    }
    x.acos()
}

/// `float acosf(float x)`
pub fn acosf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if !(-1.0..=1.0).contains(&x) {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x.acos()
}

/// `double asin(double x)` — domain `[-1, 1]`; outside → NaN + EDOM.
pub fn asin(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_nan() {
        return x;
    }
    if !(-1.0..=1.0).contains(&x) {
        return domain_error(ctx);
    }
    x.asin()
}

/// `float asinf(float x)`
pub fn asinf(ctx: &mut impl GuestContext, x: f32) -> f32 {
    if x.is_nan() {
        return x;
    }
    if !(-1.0..=1.0).contains(&x) {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x.asin()
}

/// `double atan2(double y, double x)` — total over all finite/inf inputs (C Annex F
/// defines every quadrant combination; only NaN inputs produce NaN). No errno.
pub fn atan2(_ctx: &mut impl GuestContext, y: f64, x: f64) -> f64 {
    y.atan2(x)
}

/// `float atan2f(float y, float x)`
pub fn atan2f(_ctx: &mut impl GuestContext, y: f32, x: f32) -> f32 {
    y.atan2(x)
}

/// `float atanf(float x)` — total; no errno.
pub fn atanf(_ctx: &mut impl GuestContext, x: f32) -> f32 {
    x.atan()
}

// ---------------------------------------------------------------------------
// other
// ---------------------------------------------------------------------------

/// `float cbrtf(float x)` — cube root, total (cbrt(-inf) = -inf, sign preserved);
/// no errno.
pub fn cbrtf(_ctx: &mut impl GuestContext, x: f32) -> f32 {
    x.cbrt()
}

/// `float fmodf(float x, float y)` — **exact in IEEE-754** (bit-for-bit testable).
/// `fmod(±inf, y)` / `fmod(x, 0)` → NaN + EDOM (domain error); `fmod(finite, ±inf) = x`.
pub fn fmodf(ctx: &mut impl GuestContext, x: f32, y: f32) -> f32 {
    if x.is_infinite() || y == 0.0 {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    x % y
}

/// `double frexp(double x, int *exp)` — splits x into `m * 2^e` with `m ∈ [0.5, 1)`.
/// **Exact.** Special cases (C Annex F / POSIX): x = ±0 → returns ±0 with *exp = 0;
/// x = ±inf/NaN → returns x with *exp unspecified (bionic sets 0). Writes the exponent
/// to the guest `int*` (4 bytes, little-endian) when non-null.
pub fn frexp(
    ctx: &mut impl GuestContext,
    x: f64,
    exp_ptr: u64,
) -> Result<f64, Fault> {
    // Rust exposes frexp via (m, e) = x.abs().frexp? No — Rust 1.89 has no std frexp;
    // implement it exactly through bit manipulation (safe code via to_bits).
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        if exp_ptr != 0 {
            ctx.write(exp_ptr, &0i32.to_le_bytes())?;
        }
        return Ok(x);
    }
    let bits = x.to_bits();
    let sign = bits & (1u64 << 63);
    let biased_exp = ((bits >> 52) & 0x7FF) as i32;
    let mantissa_bits = bits & ((1u64 << 52) - 1);
    // Subnormals: normalise exactly by shifting the fraction's MSB into the normal
    // mantissa slot; the exponent compensates. No precision is lost (bit shifts only).
    let (m_bits, e) = if biased_exp == 0 {
        let msb = 51 - mantissa_bits.leading_zeros() as i32; // position of MSB of fraction
        let e = -1022 - msb - 1;
        let norm_bits = (mantissa_bits << (52 - msb)) >> 12; // shift fraction into normal slot
        let m = f64::from_bits(norm_bits | (1022u64 << 52) | sign);
        (m, e)
    } else {
        let e = biased_exp - 1022;
        let m = f64::from_bits((1022u64 << 52) | mantissa_bits | sign);
        (m, e)
    };
    if exp_ptr != 0 {
        ctx.write(exp_ptr, &e.to_le_bytes())?;
    }
    Ok(m_bits)
}

/// `int ilogb(double x)` — unbiased exponent as int. Special cases per POSIX:
/// x = ±0 → FP_ILOGB0 (= INT_MIN on Linux); x = ±inf → FP_ILOGBINTB (= INT_MAX);
/// NaN → FP_ILOGBNAN (= INT_MIN or INT_MAX; Linux/bionic uses INT_MAX... bionic defines
/// FP_ILOGBNAN as INT_MAX? glibc uses INT_MAX for ilogb(NaN)? POSIX leaves both choices;
/// bionic's <math.h> sets FP_ILOGB0 = (-2147483647-1) and FP_ILOGBNAN = (-2147483647-1)
/// — VERIFIED against bionic's `libm/include/math.h` (INT_MIN); documented divergence
/// from glibc's INT_MAX in the report. No errno.
pub fn ilogb(x: f64) -> i32 {
    if x == 0.0 {
        return i32::MIN; // FP_ILOGB0
    }
    if x.is_nan() {
        return i32::MIN; // FP_ILOGBNAN (bionic)
    }
    if x.is_infinite() {
        return i32::MAX; // FP_ILOGBINTB
    }
    let bits = x.to_bits();
    let biased = ((bits >> 52) & 0x7FF) as i32;
    if biased == 0 {
        // Subnormal: the exponent of the top set bit of the fraction. The fraction is
        // the low 52 bits, so the MSB position is `51 - (leading_zeros - 12)` (the
        // leading zeros count the full 64-bit word; 12 leading slots above the fraction).
        let mantissa = bits & ((1u64 << 52) - 1);
        let msb = 51 - (mantissa.leading_zeros() as i32 - 12); // 0..51
        -1022 - 52 + msb
    } else {
        biased - 1023
    }
}

/// `double ldexp(double x, int exp)` — x * 2^exp, **exact when representable**.
/// Overflow → ±HUGE_VAL + ERANGE; underflow → 0 (POSIX raises ERANGE; C11 allows not
/// setting it for gradual underflow — bionic sets it, so we set it on true underflow to 0).
pub fn ldexp(ctx: &mut impl GuestContext, x: f64, exp: i32) -> f64 {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return x;
    }
    let r = x * 2f64.powi(exp.clamp(-2100, 2100));
    if r.is_infinite() {
        return range_error_inf(ctx, x < 0.0);
    }
    if r == 0.0 && x != 0.0 {
        ctx.set_errno(ERANGE);
        return r;
    }
    r
}

/// `float ldexpf(float x, int exp)`
pub fn ldexpf(ctx: &mut impl GuestContext, x: f32, exp: i32) -> f32 {
    if x == 0.0 || x.is_nan() || x.is_infinite() {
        return x;
    }
    let r = x * 2f32.powi(exp.clamp(-160, 160));
    if r.is_infinite() {
        ctx.set_errno(ERANGE);
        return r;
    }
    if r == 0.0 && x != 0.0 {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `float modff(float x, float *iptr)` — splits x into integer and fractional parts,
/// each with the sign of x. **Exact.** ±inf → *iptr = x, return 0.0; NaN propagates.
/// Writes the integer part to the guest `float*` (4 bytes) when non-null.
pub fn modff(ctx: &mut impl GuestContext, x: f32, iptr: u64) -> Result<f32, Fault> {
    let truncated = if x.is_nan() || x.is_infinite() {
        x
    } else {
        x.trunc()
    };
    if iptr != 0 {
        ctx.write(iptr, &truncated.to_le_bytes())?;
    }
    if x.is_nan() {
        return Ok(x);
    }
    // Annex F: the fraction carries x's sign -- `-0.0` for `-inf` and for a negative integer,
    // which `x - truncated` alone would make `+0.0`.
    if x.is_infinite() {
        return Ok(0.0f32.copysign(x));
    }
    Ok((x - truncated).copysign(x))
}

/// `double modf(double x, double *iptr)` — splits x into integer and fractional parts, **each
/// with the sign of x** (C11 Annex F.10.3.12). **Exact.** A negative integer's fraction is `-0.0`,
/// not `+0.0`; ±inf stores ±inf and returns ±0; NaN stores and returns NaN. Writes the integer
/// part to the guest `double*` (8 bytes) when non-null.
pub fn modf(ctx: &mut impl GuestContext, x: f64, iptr: u64) -> Result<f64, Fault> {
    let integral = if x.is_finite() { x.trunc() } else { x };
    if iptr != 0 {
        ctx.write(iptr, &integral.to_le_bytes())?;
    }
    if x.is_nan() {
        return Ok(x);
    }
    if x.is_infinite() {
        return Ok(0.0f64.copysign(x));
    }
    Ok((x - integral).copysign(x))
}

// ---------------------------------------------------------------------------
// M6: the rest of the engine's libm imports, once the Lua app reached `atanf`
// ---------------------------------------------------------------------------

/// `double atan(double x)` -- total; no errno.
pub fn atan(_ctx: &mut impl GuestContext, x: f64) -> f64 {
    x.atan()
}

/// `double cbrt(double x)` -- total, sign preserved.
pub fn cbrt(_ctx: &mut impl GuestContext, x: f64) -> f64 {
    x.cbrt()
}

/// `double tan(double x)` -- `±inf` is NaN + EDOM, as [`tanf`].
pub fn tan(ctx: &mut impl GuestContext, x: f64) -> f64 {
    if x.is_infinite() {
        return domain_error(ctx);
    }
    x.tan()
}

/// `double tanh(double x)` -- total; no errno.
pub fn tanh(_ctx: &mut impl GuestContext, x: f64) -> f64 {
    x.tanh()
}

/// `double exp2(double x)` -- overflow from a finite `x` is `+HUGE_VAL` + ERANGE, as [`exp`].
pub fn exp2(ctx: &mut impl GuestContext, x: f64) -> f64 {
    let r = x.exp2();
    if r.is_infinite() && !x.is_infinite() {
        return range_error_inf(ctx, false);
    }
    r
}

/// `float exp2f(float x)` -- as [`exp2`].
pub fn exp2f(ctx: &mut impl GuestContext, x: f32) -> f32 {
    let r = x.exp2();
    if r.is_infinite() && !x.is_infinite() {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `double expm1(double x)` -- `e^x - 1`, accurate near 0; overflow as [`exp`].
pub fn expm1(ctx: &mut impl GuestContext, x: f64) -> f64 {
    let r = x.exp_m1();
    if r.is_infinite() && !x.is_infinite() {
        return range_error_inf(ctx, false);
    }
    r
}

/// `float hypotf(float x, float y)` -- `+inf` if either is infinite (even with a NaN, Annex F);
/// overflow from finite inputs is `+HUGE_VALF` + ERANGE.
pub fn hypotf(ctx: &mut impl GuestContext, x: f32, y: f32) -> f32 {
    if x.is_infinite() || y.is_infinite() {
        return f32::INFINITY;
    }
    let r = x.hypot(y);
    if r.is_infinite() {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `double fmod(double x, double y)` -- **exact**, as [`fmodf`]: `fmod(±inf, y)` and `fmod(x, 0)`
/// are NaN + EDOM; `fmod(finite, ±inf)` is `x`.
pub fn fmod(ctx: &mut impl GuestContext, x: f64, y: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x.is_infinite() || y == 0.0 {
        return domain_error(ctx);
    }
    x % y
}

/// `float frexpf(float x, int *exp)` -- **exact**, as [`frexp`]: `m * 2^e` with `m ∈ [0.5, 1)`;
/// `±0`, `±inf` and NaN return `x` with `*exp = 0`. Done in `f64`, where every `f32` (subnormals
/// included) is normal, so the split is a field extraction.
pub fn frexpf(ctx: &mut impl GuestContext, x: f32, exp_ptr: u64) -> Result<f32, Fault> {
    let (m, e) = if x == 0.0 || x.is_nan() || x.is_infinite() {
        (x, 0i32)
    } else {
        let bits = f64::from(x).to_bits();
        let e = ((bits >> 52) & 0x7ff) as i32 - 1022;
        let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
        (m as f32, e)
    };
    if exp_ptr != 0 {
        ctx.write(exp_ptr, &e.to_le_bytes())?;
    }
    Ok(m)
}

/// `double round(double x)` -- half away from zero; **exact**, no errno.
#[must_use]
pub fn round(x: f64) -> f64 {
    x.round()
}

/// `float nextafterf(float x, float y)` -- **exact** (FreeBSD's `s_nextafterf.c`, which bionic
/// builds): the next representable `f32` after `x` toward `y`; NaN in, NaN out; `x == y` is `y`;
/// from `±0` the smallest subnormal with `y`'s sign. Overflow to `±inf`, and a subnormal or zero
/// result from a non-zero `x`, set ERANGE (POSIX's range errors).
pub fn nextafterf(ctx: &mut impl GuestContext, x: f32, y: f32) -> f32 {
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x == y {
        return y;
    }
    if x == 0.0 {
        ctx.set_errno(ERANGE);
        return f32::from_bits(1).copysign(y);
    }
    let bits = x.to_bits();
    let away = (x > 0.0) == (y > x);
    let r = f32::from_bits(if away { bits + 1 } else { bits - 1 });
    if r.is_infinite() || !r.is_normal() {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `double nan(const char *tagp)` — a quiet NaN; bionic ignores the tag payload
/// (the returned NaN's bits are implementation-defined when tagp != 0 — C11 7.22.1.3).
/// A null tagp is a null *string pointer* in the guest... C says tagp is a pointer to a
/// string; "0" is the standard call. No memory access happens in bionic for the C locale.
pub fn nan(_tagp: u64) -> f64 {
    f64::NAN
}

/// `double pow(double x, double y)` — the full C Annex F edge-case table:
/// * `pow(±0, -y)` → ±inf + ERANGE (pole); `pow(±0, +y)` → ±0;
/// * `pow(-1, ±inf)` → 1; `pow(+1, y)` → 1 for any y (even NaN); `pow(x, ±0)` → 1;
/// * `pow(x, y)` with x < 0 and y non-integer → NaN + EDOM;
/// * overflow → ±HUGE_VAL + ERANGE; underflow → 0 + ERANGE.
pub fn pow(ctx: &mut impl GuestContext, x: f64, y: f64) -> f64 {
    // C11 Annex F.9.4.4 / POSIX cases that bypass the generic computation:
    if y.is_nan() {
        if x == 1.0 {
            return 1.0; // pow(1, NaN) == 1 (Annex F)
        }
        return x + y; // quiet NaN propagation
    }
    if x.is_nan() {
        if y == 0.0 {
            return 1.0; // pow(NaN, 0) == 1
        }
        return x + y;
    }
    if y == 0.0 {
        return 1.0; // pow(anything, ±0) == 1
    }
    if x == 0.0 {
        if y < 0.0 {
            // pow(±0, -y): pole error; sign: +inf for +0 or even-integral |y|, else -inf
            let negative_result = (x.is_sign_negative()) && (y.floor() != y || (y as i64) % 2 != 0);
            return range_error_inf(ctx, negative_result);
        }
        // pow(±0, +y) = ±0: the sign of x survives only when y is an odd integer
        // (Annex F: pow(-0, even y) = +0, pow(-0, odd y) = -0).
        if y.floor() == y && (y as i64) % 2 != 0 {
            return x;
        }
        return 0.0;
    }
    if x == 1.0 {
        return 1.0;
    }
    if y.is_infinite() {
        let m = x.abs();
        if m == 1.0 {
            return 1.0; // pow(±1, ±inf) == 1
        }
        if (y > 0.0) == (m > 1.0) {
            return range_error_inf(ctx, false);
        }
        return 0.0;
    }
    if x.is_infinite() {
        if y < 0.0 {
            return 0.0; // pow(±inf, -y) = +0 / -0 (sign: odd integral y and -inf)
        }
        // pow(+inf, +y) = +inf; pow(-inf, odd-integral y) = -inf; else +inf.
        let negative_result = x < 0.0 && (y as i64) % 2 != 0 && y.floor() == y;
        return range_error_inf(ctx, negative_result);
    }
    if x < 0.0 && y.floor() != y {
        // Even-integral check first: y must be integral for a real result.
        return domain_error(ctx);
    }
    let r = x.powf(y);
    if r.is_infinite() {
        return range_error_inf(ctx, x < 0.0 && (y as i64) % 2 != 0);
    }
    if r == 0.0 && x != 0.0 {
        ctx.set_errno(ERANGE);
        return r;
    }
    r
}

/// `float powf(float x, float y)` — as [`pow`], f32.
pub fn powf(ctx: &mut impl GuestContext, x: f32, y: f32) -> f32 {
    if y.is_nan() {
        if x == 1.0 {
            return 1.0;
        }
        return x + y;
    }
    if x.is_nan() {
        if y == 0.0 {
            return 1.0;
        }
        return x + y;
    }
    if y == 0.0 {
        return 1.0;
    }
    if x == 0.0 {
        if y < 0.0 {
            let negative_result = x.is_sign_negative() && (y.floor() != y || (y as i32) % 2 != 0);
            ctx.set_errno(ERANGE);
            return if negative_result { f32::NEG_INFINITY } else { f32::INFINITY };
        }
        return x;
    }
    if x == 1.0 {
        return 1.0;
    }
    if y.is_infinite() {
        let m = x.abs();
        if m == 1.0 {
            return 1.0;
        }
        if (y > 0.0) == (m > 1.0) {
            ctx.set_errno(ERANGE);
            return f32::INFINITY;
        }
        return 0.0;
    }
    if x.is_infinite() {
        if y < 0.0 {
            return 0.0;
        }
        let negative_result = x < 0.0 && (y.floor() != y || (y as i32) % 2 != 0);
        if negative_result {
            ctx.set_errno(ERANGE);
            return f32::NEG_INFINITY;
        }
        ctx.set_errno(ERANGE);
        return f32::INFINITY;
    }
    if x < 0.0 && y.floor() != y {
        ctx.set_errno(EDOM);
        return f32::NAN;
    }
    let r = x.powf(y);
    if r.is_infinite() {
        ctx.set_errno(ERANGE);
        return r;
    }
    if r == 0.0 && x != 0.0 {
        ctx.set_errno(ERANGE);
    }
    r
}

/// `void sincosf(float x, float *sin_ptr, float *cos_ptr)` — GNU/bionic extension:
/// computes both at once. Writes both results through guest memory (4 bytes each,
/// little-endian) when the respective pointer is non-null. NaN propagation and the
/// ±inf → NaN + EDOM rule as for sinf/cosf.
pub fn sincosf(
    ctx: &mut impl GuestContext,
    x: f32,
    sin_ptr: u64,
    cos_ptr: u64,
) -> Result<(), Fault> {
    let (s, c) = if x.is_infinite() {
        ctx.set_errno(EDOM);
        (f32::NAN, f32::NAN)
    } else {
        (x.sin(), x.cos())
    };
    if sin_ptr != 0 {
        ctx.write(sin_ptr, &s.to_le_bytes())?;
    }
    if cos_ptr != 0 {
        ctx.write(cos_ptr, &c.to_le_bytes())?;
    }
    Ok(())
}
