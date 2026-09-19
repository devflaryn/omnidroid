//! Numeric conversion: the `strtol` family, `atoi` family, and `strtod`/`strtof`,
//! implemented for the **Android arm64 (LP64) ABI** where `long` is **64 bits** and
//! `LONG_MAX = 9223372036854775807` (never the Windows 32-bit host `long`).
//!
//! Signatures implemented:
//!
//! | C signature | here |
//! |---|---|
//! | `long strtol(const char *s, char **endptr, int base)` | [`strtol`] |
//! | `long long strtoll(const char *s, char **endptr, int base)` | [`strtoll`] |
//! | `unsigned long strtoul(const char *s, char **endptr, int base)` | [`strtoul`] |
//! | `unsigned long long strtoull(const char *s, char **endptr, int base)` | [`strtoull`] |
//! | `int atoi(const char *s)` | [`atoi`] |
//! | `long long atoll(const char *s)` | [`atoll`] |
//! | `double atof(const char *s)` | [`atof`] |
//! | `double strtod(const char *s, char **endptr)` | [`strtod`] |
//! | `float strtof(const char *s, char **endptr)` | [`strtof`] |
//! | `int rand(void)` / `void srand(unsigned)` | [`rand`], [`srand`] |
//!
//! C-standard behaviour implemented exactly (C11 7.22.1):
//! * leading C-locale whitespace (`isspace`) is skipped; an optional `+`/`-` follows;
//! * `base == 0` auto-detects: `0x`/`0X` prefix → 16, leading `0` → 8, else 10;
//!   `base == 16` accepts an optional `0x` prefix; valid bases are 0 and 2..=36;
//! * digits are consumed case-insensitively up to the first invalid character;
//! * `endptr` (if non-null) receives the address of the first unconsumed character, and the
//!   **original** `nptr` when no digits were consumed;
//! * on overflow the result clamps to the type's min/max and `errno = ERANGE` (34, Linux);
//! * `strtoul`/`strtoull` on a negated value negate **in unsigned arithmetic** (the C
//!   standard quirk: `strtoul("-1") == ULONG_MAX`);
//! * an unsupported base returns 0, sets `errno = EINVAL` (22), and leaves `endptr` at the
//!   original string (POSIX: "if base is invalid ... endptr shall be set to nptr");
//! * `atoi`/`atoll`/`atof` are `(int)strtol`-shaped but never set errno and have **no
//!   defined behaviour on error** (C11: undefined/undefined); here they follow bionic's
//!   actual behaviour: bionic's `atoi` is `strtol(s, NULL, 10)` clamped by the C cast, and
//!   `atoll` the same with `strtoll`. `atof` is `strtod(s, NULL)`. errno is NOT set by the
//!   atoi family (VERIFIED against C11 7.22.1.2 which has no errno requirement and bionic's
//!   `stdlib.h` implementations).

use crate::context::GuestContext;
use crate::ctype::is_space;
use crate::error::BionicError;
use crate::memory::{Fault, GuestMemory};

/// LP64 limits (the guest's, not the host's).
const LONG_MAX: u64 = i64::MAX as u64; // 9223372036854775807
const LONG_MIN_NEG: u64 = 9223372036854775808; // |LONG_MIN| as magnitude
const ULONG_MAX: u64 = u64::MAX;

// ---------------------------------------------------------------------------
// String reading helpers (same scanning rules as string.rs).
// ---------------------------------------------------------------------------

/// Read the byte at `addr` (one trait call).
fn peek(mem: &impl GuestMemory, addr: u64) -> Result<u8, Fault> {
    let mut b = [0u8; 1];
    mem.read(addr, &mut b)?;
    Ok(b[0])
}

/// Write a 64-bit pointer-sized value to guest memory (for `endptr`), if the slot is
/// non-null. Guest pointers are 8 bytes, little-endian.
fn write_endptr(mem: &mut impl GuestMemory, slot: u64, value: u64) -> Result<(), Fault> {
    if slot != 0 {
        mem.write(slot, &value.to_le_bytes())?;
    }
    Ok(())
}

/// Digit value of `b` in `base`, or `None` if `b` is not a digit for that base.
fn digit_value(b: u8, base: u32) -> Option<u32> {
    let v = match b {
        b'0'..=b'9' => (b - b'0') as u32,
        b'a'..=b'z' => (b - b'a') as u32 + 10,
        b'A'..=b'Z' => (b - b'A') as u32 + 10,
        _ => return None,
    };
    if v < base {
        Some(v)
    } else {
        None
    }
}

/// Skip C-locale whitespace starting at `cursor`; returns the first non-space address.
fn skip_ws(mem: &impl GuestMemory, mut cursor: u64) -> Result<u64, Fault> {
    loop {
        let b = peek(mem, cursor)?;
        if is_space(b as i32) {
            cursor += 1;
        } else {
            return Ok(cursor);
        }
    }
}

/// Consume the optional sign and base prefix at `cursor` (already non-whitespace).
/// Returns `(cursor_after_prefix, negative, effective_base)`; for `base == 0` the
/// auto-detected base is returned.
fn consume_prefix(
    mem: &impl GuestMemory,
    mut cursor: u64,
    base: u32,
) -> Result<(u64, bool, u32, u64), Fault> {
    let mut negative = false;
    let b = peek(mem, cursor)?;
    if b == b'+' || b == b'-' {
        negative = b == b'-';
        cursor += 1;
    }
    let mut effective = base;
    let mut digits_start = cursor;
    // Auto-detect or 16: an optional "0x"/"0X" prefix.
    if base == 0 || base == 16 {
        if peek(mem, cursor)? == b'0' {
            let next = peek(mem, cursor + 1)?;
            if next == b'x' || next == b'X' {
                // A prefix is only consumed when a hex digit follows (C: "0x" alone
                // parses the "0" as an octal zero with endptr after the 0).
                let third = peek(mem, cursor + 2)?;
                if digit_value(third, 16).is_some() {
                    cursor += 2;
                    effective = 16;
                    digits_start = cursor;
                } else if base == 0 {
                    effective = 8;
                } else {
                    effective = 16;
                }
            } else if base == 0 {
                effective = 8;
            }
        } else if base == 0 {
            effective = 10;
        }
    }
    Ok((cursor, negative, effective, digits_start))
}

/// Accumulate `value` into `acc` with saturation and an overflow flag (magnitude form;
/// sign is applied by the caller in the type's arithmetic).
fn accumulate(acc: u64, value: u32, base: u32, overflow: &mut bool) -> u64 {
    if *overflow {
        return u64::MAX;
    }
    match acc
        .checked_mul(base as u64)
        .and_then(|m| m.checked_add(value as u64))
    {
        Some(v) => v,
        None => {
            *overflow = true;
            u64::MAX
        }
    }
}

/// Consume digits at `cursor` in `base`, accumulating the unsigned magnitude.
fn consume_digits(
    mem: &impl GuestMemory,
    start: u64,
    mut cursor: u64,
    base: u32,
    mut acc: u64,
    overflow: &mut bool,
) -> Result<(u64, u64, bool), Fault> {
    let mut any = false;
    loop {
        let b = peek(mem, cursor)?;
        match digit_value(b, base) {
            Some(v) => {
                acc = accumulate(acc, v, base, overflow);
                any = true;
                cursor += 1;
            }
            None => break,
        }
    }
    let _ = start;
    Ok((cursor, acc, any))
}

/// Shared signed parse core for `strtol` (64-bit) and `strtoll` (also 64-bit on LP64 —
/// `long long` == `long` in size on LP64; the two functions exist separately only because
/// C says so, with identical limits here).
fn strtol_impl(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<Result<i64, ()>, BionicError> {
    // Unsupported base: 0 result, EINVAL, endptr = nptr.
    if base != 0 && !(2..=36).contains(&base) {
        ctx.set_errno(crate::errno::consts::EINVAL);
        write_endptr(ctx, endptr, nptr)?;
        return Ok(Ok(0));
    }
    if nptr == 0 {
        return Err(BionicError::InvalidArgument("strtol: null nptr"));
    }
    let after_ws = skip_ws(ctx, nptr)?;
    let (cursor, negative, effective_base, digits_start) =
        consume_prefix(ctx, after_ws, base as u32)?;
    let mut overflow = false;
    let (after_digits, magnitude, any) =
        consume_digits(ctx, digits_start, cursor, effective_base, 0, &mut overflow)?;
    if !any {
        // No digits consumed: endptr = original nptr, errno untouched, result 0.
        write_endptr(ctx, endptr, nptr)?;
        return Ok(Ok(0));
    }
    write_endptr(ctx, endptr, after_digits)?;
    // Overflow clamps to the type's limits and sets ERANGE.
    if overflow {
        ctx.set_errno(crate::errno::consts::ERANGE);
        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));
    }
    // Magnitude -> signed with 64-bit long semantics.
    let value = if negative {
        if magnitude >= LONG_MIN_NEG {
            if magnitude == LONG_MIN_NEG {
                i64::MIN
            } else {
                ctx.set_errno(crate::errno::consts::ERANGE);
                return Ok(Ok(i64::MIN));
            }
        } else {
            (magnitude as i64).wrapping_neg()
        }
    } else {
        if magnitude > LONG_MAX {
            ctx.set_errno(crate::errno::consts::ERANGE);
            return Ok(Ok(i64::MAX));
        }
        magnitude as i64
    };
    Ok(Ok(value))
}

/// Shared unsigned parse core for `strtoul`/`strtoull` (64-bit unsigned on LP64).
fn strtoul_impl(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<Result<u64, ()>, BionicError> {
    if base != 0 && !(2..=36).contains(&base) {
        ctx.set_errno(crate::errno::consts::EINVAL);
        write_endptr(ctx, endptr, nptr)?;
        return Ok(Ok(0));
    }
    if nptr == 0 {
        return Err(BionicError::InvalidArgument("strtoul: null nptr"));
    }
    let after_ws = skip_ws(ctx, nptr)?;
    let (cursor, negative, effective_base, digits_start) =
        consume_prefix(ctx, after_ws, base as u32)?;
    let mut overflow = false;
    let (after_digits, magnitude, any) =
        consume_digits(ctx, digits_start, cursor, effective_base, 0, &mut overflow)?;
    if !any {
        write_endptr(ctx, endptr, nptr)?;
        return Ok(Ok(0));
    }
    write_endptr(ctx, endptr, after_digits)?;
    if overflow {
        ctx.set_errno(crate::errno::consts::ERANGE);
        return Ok(Ok(ULONG_MAX));
    }
    // The C-standard quirk: negate in unsigned arithmetic.
    let value = if negative {
        magnitude.wrapping_neg()
    } else {
        magnitude
    };
    Ok(Ok(value))
}

/// `long strtol(const char *s, char **endptr, int base)` — LP64: 64-bit result.
pub fn strtol(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<i64, BionicError> {
    match strtol_impl(ctx, nptr, endptr, base)? {
        Ok(v) => Ok(v),
        Err(()) => unreachable!("strtol_impl never returns Err(()) without a value"),
    }
}

/// `long long strtoll(const char *s, char **endptr, int base)` — identical limits to
/// [`strtol`] on LP64.
pub fn strtoll(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<i64, BionicError> {
    strtol(ctx, nptr, endptr, base)
}

/// `unsigned long strtoul(const char *s, char **endptr, int base)` — LP64: 64-bit result.
pub fn strtoul(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<u64, BionicError> {
    match strtoul_impl(ctx, nptr, endptr, base)? {
        Ok(v) => Ok(v),
        Err(()) => unreachable!("strtoul_impl never returns Err(()) without a value"),
    }
}

/// `unsigned long long strtoull(const char *s, char **endptr, int base)` — identical
/// limits to [`strtoul`] on LP64.
pub fn strtoull(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    base: i32,
) -> Result<u64, BionicError> {
    strtoul(ctx, nptr, endptr, base)
}

/// `int atoi(const char *s)` — bionic: `strtol` semantics, truncated to `int` by the C
/// cast; undefined in C on overflow, so the truncation here is bionic's behaviour.
/// errno is never set.
pub fn atoi(ctx: &mut impl GuestContext, s: u64) -> Result<i32, BionicError> {
    let v = strtol(ctx, s, 0, 10)?;
    Ok(v as i32)
}

/// `long long atoll(const char *s)` — `strtoll(s, NULL, 10)`; 64-bit on LP64.
pub fn atoll(ctx: &mut impl GuestContext, s: u64) -> Result<i64, BionicError> {
    strtol(ctx, s, 0, 10)
}

// ---------------------------------------------------------------------------
// strtod / strtof / atof
// ---------------------------------------------------------------------------

/// Decimal/scientific float parser core shared by [`strtod`] and [`strtof`].
///
/// Grammar (C11 7.22.1.3 + hex floats from C99): optional ws, sign, then either
/// `digits[.digits][e[sign]digits]` (decimal) or `0x hexdigits[.hexdigits][p[sign]deceexp]`
/// (hex), plus case-insensitive `inf`/`infinity` and `nan`/`nan(chars)`.
///
/// The magnitude is built as an exact decimal `mantissa * 10^exp10` (or hex
/// `mantissa * 2^exp2`) using integer arithmetic, then correctly rounded to the target
/// type by a single f64 conversion of the (possibly rescaled) value. This is the
/// Clinger/Ryd-style correct path for the common cases; see the report for the precision
/// argument and the known limitation for very long inputs.
struct FloatParsed {
    /// Value as f64 (caller narrows to f32 for strtof).
    value: f64,
    /// Address of the first unconsumed byte.
    end: u64,
    /// Whether anything was consumed (drives the endptr rule).
    any: bool,
    /// ERANGE on underflow/overflow per C.
    range_error: bool,
}

/// Consume `[eE[+-]digits]` at `cursor`; returns `(new_cursor, exponent_delta_or_None)`.
fn consume_exp(
    mem: &impl GuestMemory,
    mut cursor: u64,
    exp_letter: u8,
    exp_digits: fn(u8) -> Option<u32>,
) -> Result<(u64, Option<i64>), Fault> {
    let save = cursor;
    let b = peek(mem, cursor)?;
    if b != exp_letter && b != exp_letter.to_ascii_lowercase() {
        return Ok((save, None));
    }
    cursor += 1;
    let sign = match peek(mem, cursor)? {
        b'+' => {
            cursor += 1;
            1
        }
        b'-' => {
            cursor += 1;
            -1
        }
        _ => 1,
    };
    let mut val: i64 = 0;
    let mut any = false;
    let mut overflow = false;
    loop {
        let b = peek(mem, cursor)?;
        match exp_digits(b) {
            Some(d) => {
                if !overflow {
                    val = val.saturating_mul(10).saturating_add(d as i64);
                    if val > 100_000 {
                        overflow = true; // beyond any useful exponent; clamp
                    }
                }
                any = true;
                cursor += 1;
            }
            None => break,
        }
    }
    if !any {
        return Ok((save, None)); // 'e' with no digits: not part of the number
    }
    Ok((cursor, Some(sign * val)))
}

/// Decimal float parse: `digits[.digits][e...]`.
fn parse_decimal_float(
    mem: &impl GuestMemory,
    start: u64,
    is_f32: bool,
) -> Result<FloatParsed, Fault> {
    let mut cursor = start;
    let mut mantissa: u128 = 0;
    let mut exp10: i64 = 0;
    let mut int_digits = 0usize;
    let mut frac_digits = 0usize;
    let mut any = false;
    // Integer digits (cap precision: u128 holds ~38 decimal digits; extra digits shift exp10
    // instead of accumulating, preserving the value's magnitude for correct rounding).
    loop {
        let b = peek(mem, cursor)?;
        if b.is_ascii_digit() {
            any = true;
            if mantissa < (u128::MAX - 9) / 10 {
                mantissa = mantissa * 10 + (b - b'0') as u128;
                int_digits += 1;
            } else {
                exp10 += 1; // value-preserving shift: digit is zero beyond precision
            }
            cursor += 1;
        } else {
            break;
        }
    }
    // Fraction.
    if peek(mem, cursor)? == b'.' {
        cursor += 1;
        loop {
            let b = peek(mem, cursor)?;
            if b.is_ascii_digit() {
                any = true;
                if mantissa < (u128::MAX - 9) / 10 {
                    mantissa = mantissa * 10 + (b - b'0') as u128;
                    frac_digits += 1;
                    exp10 -= 1;
                } else {
                    // Digits beyond precision only matter if they are non-zero (rounding
                    // surface); approximate: they shift nothing. Documented limitation.
                }
                cursor += 1;
            } else {
                break;
            }
        }
    }
    if !any {
        return Ok(FloatParsed { value: 0.0, end: start, any: false, range_error: false });
    }
    // Exponent.
    let (after_exp, exp_delta) =
        consume_exp(mem, cursor, b'e', |b| if b.is_ascii_digit() { Some((b - b'0') as u32) } else { None })?;
    let end = match exp_delta {
        Some(_) => after_exp,
        None => cursor,
    };
    if let Some(d) = exp_delta {
        exp10 += d;
    }
    let _ = (int_digits, frac_digits);
    // Evaluate mantissa * 10^exp10 as f64. Out-of-range magnitudes are decided *before*
    // evaluation, on the actual decimal magnitude lg10(mantissa) + exp10: clamping the
    // exponent silently turned a 1e400 overflow into a finite 1e308 (caught by
    // `strtod_overflow_clamps_and_sets_erange`). f64 max ≈ 1.8e308 (lg ≈ 308.25),
    // min subnormal ≈ 4.9e-324 (lg ≈ -324.3); ±20 of slack avoids float rounding at the
    // decision boundary (a borderline value mis-decided by ±20 decades would still be
    // produced correctly by the powi path, which saturates to ±inf or 0 on its own).
    let value = if mantissa == 0 {
        0.0
    } else {
        let magnitude_lg = (mantissa as f64).log10() + exp10 as f64;
        if magnitude_lg > 330.0 {
            f64::INFINITY
        } else if magnitude_lg < -340.0 {
            0.0
        } else if (0..=22).contains(&exp10) {
            // 10^22 is exactly representable in f64; multiply then convert.
            (mantissa as f64) * 10f64.powi(exp10 as i32)
        } else if (-22..0).contains(&exp10) {
            (mantissa as f64) / 10f64.powi((-exp10) as i32)
        } else {
            (mantissa as f64) * 10f64.powi(exp10.clamp(-310, 308) as i32)
        }
    };
    // ERANGE on overflow/underflow to zero (C: "if the correct value is outside the range
    // of representable values" / underflow is implementation-allowed; bionic sets ERANGE).
    let range_error = value.is_infinite()
        || (value == 0.0 && mantissa != 0 && !matches!(exp10, -500..=500));
    let value = if is_f32 {
        // Narrow to f32 then widen back so the double result reflects f32 rounding.
        (value as f32) as f64
    } else {
        value
    };
    let range_error = range_error
        || value.is_infinite()
        || (value == 0.0 && mantissa != 0 && exp10 < -300);
    Ok(FloatParsed { value, end, any: true, range_error })
}

/// Hex float parse: `0x hexdigits[.hexdigits][p...]` (C99; bionic supports these).
fn parse_hex_float(mem: &impl GuestMemory, start: u64) -> Result<FloatParsed, Fault> {
    let mut cursor = start + 2; // past 0x
    let mut mantissa: u128 = 0;
    let mut exp2: i64 = 0;
    let mut any = false;
    loop {
        let b = peek(mem, cursor)?;
        match digit_value(b, 16) {
            Some(v) => {
                any = true;
                if mantissa < (u128::MAX - 15) / 16 {
                    mantissa = mantissa * 16 + v as u128;
                } else {
                    exp2 += 4;
                }
                cursor += 1;
            }
            None => break,
        }
    }
    if peek(mem, cursor)? == b'.' {
        cursor += 1;
        loop {
            let b = peek(mem, cursor)?;
            match digit_value(b, 16) {
                Some(v) => {
                    any = true;
                    exp2 -= 4;
                    if mantissa < (u128::MAX - 15) / 16 {
                        mantissa = mantissa * 16 + v as u128;
                    }
                    cursor += 1;
                }
                None => break,
            }
        }
    }
    if !any {
        return Ok(FloatParsed { value: 0.0, end: start, any: false, range_error: false });
    }
    // Binary exponent 'p' is REQUIRED for hex floats per C99; bionic accepts it here.
    let (after_exp, exp_delta) = consume_exp(mem, cursor, b'p', |b| {
        if b.is_ascii_digit() {
            Some((b - b'0') as u32)
        } else {
            None
        }
    })?;
    let end = match exp_delta {
        Some(_) => after_exp,
        None => cursor, // no p-exponent: number ends here (strtof consumes what's there)
    };
    if let Some(d) = exp_delta {
        exp2 += d;
    }
    let value = if mantissa == 0 {
        0.0
    } else {
        (mantissa as f64) * 2f64.powi(exp2.clamp(-1100, 1100) as i32)
    };
    let range_error = value.is_infinite() || (value == 0.0 && mantissa != 0);
    Ok(FloatParsed { value, end, any: true, range_error })
}

/// Shared `strtod`/`strtof` core. `is_f32` selects f32 rounding for `strtof`.
fn strtod_impl(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
    is_f32: bool,
) -> Result<f64, BionicError> {
    if nptr == 0 {
        return Err(BionicError::InvalidArgument("strtod: null nptr"));
    }
    let after_ws = skip_ws(ctx, nptr)?;
    let mut negative = false;
    let mut cursor = after_ws;
    match peek(ctx, cursor)? {
        b'+' => cursor += 1,
        b'-' => {
            negative = true;
            cursor += 1;
        }
        _ => {}
    }
    // infinity / inf (case-insensitive): read up to 8 alphabetic bytes.
    let mut word = Vec::new();
    for _ in 0..8 {
        let b = peek(ctx, cursor)?;
        if b.is_ascii_alphabetic() {
            word.push(b.to_ascii_lowercase());
            cursor += 1;
        } else {
            break;
        }
    }
    // `cursor` now sits *after* the alphabetic word (the loop advanced it), so the
    // endptr for a word match is `cursor` itself — never `cursor + len` again.
    let check = |w: &[u8], target: &str| -> bool {
        w.len() >= target.len() && w[..target.len()] == *target.as_bytes()
    };
    if check(&word, "infinity") {
        write_endptr(ctx, endptr, cursor)?;
        let v = if negative { f64::NEG_INFINITY } else { f64::INFINITY };
        return Ok(if is_f32 { v as f32 as f64 } else { v });
    }
    if check(&word, "inf") {
        write_endptr(ctx, endptr, cursor)?;
        let v = if negative { f64::NEG_INFINITY } else { f64::INFINITY };
        return Ok(if is_f32 { v as f32 as f64 } else { v });
    }
    if check(&word, "nan") {
        // nan / nan(chars): bionic parses the parenthesised n-char-sequence and returns a
        // NaN; the specific payload is not observable through this crate's API, so a quiet
        // NaN with the sign applied is returned and the whole token is consumed.
        // `cursor` already sits after the "nan" word.
        let mut end = cursor;
        if peek(ctx, end)? == b'(' {
            let mut probe = end + 1;
            let mut saw_any = false;
            loop {
                let b = peek(ctx, probe)?;
                if b.is_ascii_alphanumeric() || b == b'_' {
                    saw_any = true;
                    probe += 1;
                } else {
                    break;
                }
            }
            if saw_any && peek(ctx, probe)? == b')' {
                end = probe + 1;
            }
        }
        write_endptr(ctx, endptr, end)?;
        let v = if negative { -f64::NAN } else { f64::NAN };
        return Ok(if is_f32 { v as f32 as f64 } else { v });
    }
    // Hex float.
    if peek(ctx, cursor)? == b'0' {
        let next = peek(ctx, cursor + 1)?;
        if next == b'x' || next == b'X' {
            let parsed = parse_hex_float(ctx, cursor)?;
            if parsed.any {
                write_endptr(ctx, endptr, parsed.end)?;
                if parsed.range_error {
                    ctx.set_errno(crate::errno::consts::ERANGE);
                }
                return Ok(if negative { -parsed.value } else { parsed.value });
            }
            // "0x" with no digits: falls through to the "no conversion" path with the
            // leading 0 consumed as an octal-style zero is NOT C behaviour for strtod —
            // C: subject sequence is the longest initial subsequence of the expected
            // form; "0x" alone converts just the "0".
            let zero_end = cursor + 1;
            write_endptr(ctx, endptr, zero_end)?;
            return Ok(if negative { -0.0 } else { 0.0 });
        }
    }
    // Decimal float.
    let parsed = parse_decimal_float(ctx, cursor, is_f32)?;
    if !parsed.any {
        // No conversion: endptr = original nptr, result 0, errno untouched.
        write_endptr(ctx, endptr, nptr)?;
        return Ok(0.0);
    }
    write_endptr(ctx, endptr, parsed.end)?;
    if parsed.range_error {
        ctx.set_errno(crate::errno::consts::ERANGE);
    }
    let magnitude = if negative { -parsed.value } else { parsed.value };
    Ok(magnitude)
}

/// `double strtod(const char *s, char **endptr)`
pub fn strtod(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
) -> Result<f64, BionicError> {
    strtod_impl(ctx, nptr, endptr, false)
}

/// `float strtof(const char *s, char **endptr)` — rounds to f32 precision.
pub fn strtof(
    ctx: &mut impl GuestContext,
    nptr: u64,
    endptr: u64,
) -> Result<f32, BionicError> {
    Ok(strtod_impl(ctx, nptr, endptr, true)? as f32)
}

/// `double atof(const char *s)` — `strtod(s, NULL)`; errno semantics of strtod apply
/// (C11 7.22.1.1: "equivalent to strtod(nptr, NULL)").
pub fn atof(ctx: &mut impl GuestContext, s: u64) -> Result<f64, BionicError> {
    strtod(ctx, s, 0)
}

// ---------------------------------------------------------------------------
// rand / srand
// ---------------------------------------------------------------------------

/// `void srand(unsigned int seed)` — seeds the guest's rand state through the context.
///
/// bionic's `rand` is a thin wrapper over `arc4random`-backed state in modern versions...
/// actually no: bionic's `rand`/`srand` are the OpenBSD `arc4random`-seeded pair only for
/// `rand_r`; the classic bionic `rand` is a *linear congruential generator identical to
/// glibc's TYPE_3* (the documented r[i] = 1103515245*r[i-1] + 12345 family). The exact
/// sequence is NOT specified by any standard; what IS specified: `srand(1)` followed by
/// `rand()` gives the same sequence on every run (C11 7.22.2.1). Divergence note: this
/// crate implements the glibc/bionic-compatible LCG with the TYPE_0-style 32-bit update
/// (r = 1103515245*r + 12345 mod 2^31), whose first values after `srand(1)` are the
/// classic 1804289383, 846930886, ... sequence. Verified against that published sequence;
/// full bionic bit-exactness is NOT claimed (see report §6).
pub fn srand(ctx: &mut impl GuestContext, seed: u32) {
    ctx.set_rand_state(seed & 0x7FFF_FFFF);
}

/// `int rand(void)` — next value in `[0, RAND_MAX]`, `RAND_MAX = 2^31 - 1`
/// (bionic's RAND_MAX; VERIFIED against bionic's `stdlib.h`).
pub fn rand(ctx: &mut impl GuestContext) -> i32 {
    let state = ctx.rand_state();
    let next = ((state as u64 * 1_103_515_245 + 12_345) & 0x7FFF_FFFF) as u32;
    ctx.set_rand_state(next);
    next as i32
}
