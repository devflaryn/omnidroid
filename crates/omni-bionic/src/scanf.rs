//! The `scanf` family's conversion engine: bionic's `vfscanf`, which is OpenBSD's, run on bytes.
//!
//! # Why an engine and not `strtol`
//!
//! MEASURED: the engine's renderer parses its thumbnail requests with
//! `sscanf(query, "type=%[^&]&id=%lld&w=%d&h=%d", ...)`, and the query it meets first has an empty
//! `id=` -- so the answer a device gives is **1**: the scanset matches, `%lld` meets `&`, and the
//! scan stops there with every later pointer untouched. That return value, and which pointers are
//! written, is what the caller branches on, and it comes from `vfscanf`'s own rules rather than
//! from any number parser: literals and whitespace, a conversion's field collection, and the
//! difference between an *input failure* and a *matching failure*.
//!
//! # The rules, as `libc/stdio/vfscanf.cpp` (android-13.0.0_r1) has them
//!
//! * A whitespace byte in the format skips any amount of input whitespace, including none.
//! * Any other literal byte must match the next input byte. Input exhausted there is an **input
//!   failure**; a different byte is a **matching failure**.
//! * `%%` matches `%` exactly, with **no** whitespace skipped first (bionic follows OpenBSD here).
//! * Every conversion except `%n` fails as an input failure if the input is already exhausted.
//!   All but `%c`, `%[` and `%n` first skip input whitespace.
//! * The integer conversions collect characters with `vfscanf`'s own state machine (optional
//!   sign, `0`/`0x` prefixes under `%i`/`%x`/`%p`, digits of the base) up to the field width,
//!   then convert as `strtoimax`/`strtoumax` would: a signed overflow clamps, an unsigned one is
//!   `UINTMAX_MAX`, and a `-` before an unsigned conversion wraps. A field with no digits is a
//!   matching failure, and a trailing `x` of a bare `0x` is given back.
//! * The value is stored in the width its length modifier names (`hh` 1, `h` 2, none 4, `l`
//!   `ll` `j` `z` `t` `q` 8 on LP64, `%p` 8).
//! * The return value is the number of assignments -- `%n` and `*`-suppressed conversions are not
//!   counted -- or `EOF` (-1) for an input failure before the first conversion. A format that
//!   ends in the middle of a conversion specification returns `EOF`.
//!
//! # What is refused, by name
//!
//! Floating-point conversions (`%e %f %g %a` and their capitals) and the wide forms (`%lc`, `%ls`,
//! `%l[`) have not been reached by any run, and a partial answer would write a wrong value through
//! a guest pointer. So would an unknown conversion character, which bionic reads as a decimal
//! integer for compatibility. Each is an [`Unsupported`] before anything is stored.

/// One value a conversion produced, for the adapter to write through the next pointer argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Store {
    /// An integer conversion (or `%n`, or `%p`): the low `size` bytes of `value`, little-endian.
    Int {
        /// The converted value, as the 64 bits `strtoimax`/`strtoumax` produced.
        value: u64,
        /// How many bytes the length modifier says the destination holds.
        size: usize,
    },
    /// `%s` or `%[`: these bytes, **then a NUL** the adapter writes.
    Str(Vec<u8>),
    /// `%c`: exactly these bytes, no NUL.
    Chars(Vec<u8>),
}

/// What one scan did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    /// The values to write, in argument order: one per conversion that was reached and not
    /// suppressed, `%n` included. A conversion past the failure consumed no argument.
    pub stores: Vec<Store>,
    /// The call's return value.
    pub result: i32,
}

/// A conversion this engine does not implement, found before anything was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// The conversion specification as written, from its `%`.
    pub conversion: String,
    /// Why it is not implemented.
    pub why: &'static str,
}

/// `EOF`.
const EOF: i32 = -1;

/// `isspace` in the C locale, which is the only locale this layer has.
fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The integer storage width a length modifier names, for LP64.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Length {
    None,
    Char,
    Short,
    Long,
    Wide,
}

impl Length {
    fn int_size(self) -> usize {
        match self {
            Length::Char => 1,
            Length::Short => 2,
            Length::None => 4,
            Length::Long | Length::Wide => 8,
        }
    }
}

/// Scan `input` (the bytes before its NUL) against `format` (the bytes before its NUL).
///
/// # Errors
///
/// [`Unsupported`] for a conversion this engine does not implement, before any store is made.
pub fn scan(input: &[u8], format: &[u8]) -> Result<Scanned, Unsupported> {
    // Refuse up front, so that an unsupported conversion anywhere in the format never leaves the
    // guest with some pointers written and a result that pretends the scan was whole.
    check_supported(format)?;

    let mut at = 0usize;
    let mut f = 0usize;
    let mut stores = Vec::new();
    let mut assigned = 0i32;
    let mut conversions = 0u32;
    let input_failure = |stores: Vec<Store>, assigned: i32, conversions: u32| Scanned {
        stores,
        result: if conversions == 0 { EOF } else { assigned },
    };

    while let Some(&c) = format.get(f) {
        f += 1;
        if is_space(c) {
            while input.get(at).is_some_and(|&b| is_space(b)) {
                at += 1;
            }
            continue;
        }
        if c != b'%' {
            match input.get(at) {
                None => return Ok(input_failure(stores, assigned, conversions)),
                Some(&b) if b == c => {
                    at += 1;
                    continue;
                }
                Some(_) => return Ok(Scanned { stores, result: assigned }),
            }
        }

        // A conversion specification.
        let mut suppress = false;
        if format.get(f) == Some(&b'*') {
            suppress = true;
            f += 1;
        }
        let mut width = 0usize;
        while let Some(&d) = format.get(f).filter(|d| d.is_ascii_digit()) {
            width = width.saturating_mul(10).saturating_add(usize::from(d - b'0'));
            f += 1;
        }
        let mut length = Length::None;
        loop {
            match format.get(f) {
                Some(b'h') if format.get(f + 1) == Some(&b'h') => {
                    length = Length::Char;
                    f += 2;
                }
                Some(b'h') => {
                    length = Length::Short;
                    f += 1;
                }
                Some(b'l') if format.get(f + 1) == Some(&b'l') => {
                    length = Length::Wide;
                    f += 2;
                }
                Some(b'l') => {
                    length = Length::Long;
                    f += 1;
                }
                Some(b'j' | b'z' | b't' | b'q') => {
                    length = Length::Wide;
                    f += 1;
                }
                Some(b'L') => f += 1,
                _ => break,
            }
        }
        let Some(&conversion) = format.get(f) else {
            // `case '\0': return (EOF);` -- the format ended inside a specification.
            return Ok(Scanned { stores, result: EOF });
        };
        f += 1;

        if conversion == b'%' {
            match input.get(at) {
                None => return Ok(input_failure(stores, assigned, conversions)),
                Some(&b'%') => {
                    at += 1;
                    continue;
                }
                Some(_) => return Ok(Scanned { stores, result: assigned }),
            }
        }
        if conversion == b'n' {
            if !suppress {
                stores.push(Store::Int { value: at as u64, size: length.int_size() });
            }
            continue;
        }

        // Every other conversion needs input, and all but `%c` and `%[` skip whitespace first.
        let skips = !matches!(conversion, b'c' | b'[');
        if skips {
            while input.get(at).is_some_and(|&b| is_space(b)) {
                at += 1;
            }
        }
        if at >= input.len() {
            return Ok(input_failure(stores, assigned, conversions));
        }

        match conversion {
            b'c' => {
                let want = if width == 0 { 1 } else { width };
                let take = want.min(input.len() - at);
                if !suppress {
                    stores.push(Store::Chars(input[at..at + take].to_vec()));
                    assigned += 1;
                }
                at += take;
                conversions += 1;
            }
            b's' => {
                let limit = if width == 0 { usize::MAX } else { width };
                let start = at;
                while at < input.len() && at - start < limit && !is_space(input[at]) {
                    at += 1;
                }
                if !suppress {
                    stores.push(Store::Str(input[start..at].to_vec()));
                    assigned += 1;
                }
                conversions += 1;
            }
            b'[' => {
                let (set, after) = scanset(format, f);
                f = after;
                let limit = if width == 0 { usize::MAX } else { width };
                let start = at;
                while at < input.len() && at - start < limit && set[usize::from(input[at])] {
                    at += 1;
                }
                if at == start {
                    return Ok(Scanned { stores, result: assigned });
                }
                if !suppress {
                    stores.push(Store::Str(input[start..at].to_vec()));
                    assigned += 1;
                }
                conversions += 1;
            }
            b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'p' => {
                let (base, unsigned, prefix_ok) = match conversion {
                    b'd' => (10, false, false),
                    b'i' => (0, false, false),
                    b'o' => (8, true, false),
                    b'u' => (10, true, false),
                    _ => (16, true, true),
                };
                let Some((field, value)) = integer(&input[at..], width, base, unsigned, prefix_ok)
                else {
                    return Ok(Scanned { stores, result: assigned });
                };
                at += field;
                if !suppress {
                    let size = if conversion == b'p' { 8 } else { length.int_size() };
                    stores.push(Store::Int { value, size });
                    assigned += 1;
                }
                conversions += 1;
            }
            _ => unreachable!("check_supported refused every other conversion"),
        }
    }
    Ok(Scanned { stores, result: assigned })
}

/// Refuse the first conversion this engine does not implement, naming it.
fn check_supported(format: &[u8]) -> Result<(), Unsupported> {
    let mut f = 0usize;
    while let Some(&c) = format.get(f) {
        f += 1;
        if c != b'%' {
            continue;
        }
        let start = f - 1;
        if format.get(f) == Some(&b'*') {
            f += 1;
        }
        while format.get(f).is_some_and(u8::is_ascii_digit) {
            f += 1;
        }
        let mut long = false;
        while let Some(&m) = format.get(f).filter(|m| b"hljztqL".contains(m)) {
            long |= m == b'l';
            f += 1;
        }
        let Some(&conversion) = format.get(f) else {
            return Ok(());
        };
        f += 1;
        let spec = || String::from_utf8_lossy(&format[start..f]).into_owned();
        match conversion {
            b'%' | b'n' | b'd' | b'i' | b'o' | b'u' | b'x' | b'X' | b'p' => {}
            b'c' | b's' if !long => {}
            b'[' if !long => {
                f = scanset(format, f).1;
            }
            b'c' | b's' | b'[' => {
                return Err(Unsupported {
                    conversion: spec(),
                    why: "a wide-character conversion: the guest's `wchar_t` is 32 bits and no run \
                          has reached one",
                })
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                return Err(Unsupported {
                    conversion: spec(),
                    why: "a floating-point conversion, which no run has reached; a value parsed \
                          by any other rule than bionic's `strtod` would be written through the \
                          guest's pointer",
                })
            }
            _ => {
                return Err(Unsupported {
                    conversion: spec(),
                    why: "not a C conversion: bionic reads an unknown one as a decimal integer \
                          for compatibility, which no run has reached",
                })
            }
        }
    }
    Ok(())
}

/// `__sccl`: the set a `%[` names, starting just after the `[`, and where the format resumes.
///
/// The first byte after `[` (or after `[^`) is always a member, so `]` there is literal; `a-z` is
/// a range unless the `-` is last or the range runs backwards, when `-` is a member itself; a
/// format that ends before `]` ends the set there.
fn scanset(format: &[u8], mut f: usize) -> ([bool; 256], usize) {
    let negate = format.get(f) == Some(&b'^');
    if negate {
        f += 1;
    }
    let mut set = [negate; 256];
    let member = !negate;
    let Some(&first) = format.get(f) else {
        return (set, f);
    };
    f += 1;
    let mut c = first;
    loop {
        set[usize::from(c)] = member;
        let Some(&n) = format.get(f) else {
            return (set, f);
        };
        f += 1;
        match n {
            b']' => return (set, f),
            b'-' => match format.get(f) {
                Some(&end) if end != b']' && end >= c => {
                    f += 1;
                    for byte in c..=end {
                        set[usize::from(byte)] = member;
                    }
                    c = end;
                }
                _ => c = b'-',
            },
            other => c = other,
        }
    }
}

/// `vfscanf`'s integer field: how many bytes it took, and the value `strtoimax`/`strtoumax` makes
/// of them -- or `None` for a matching failure (no digits).
fn integer(
    input: &[u8],
    width: usize,
    mut base: u32,
    unsigned: bool,
    prefix_ok: bool,
) -> Option<(usize, u64)> {
    let limit = if width == 0 { usize::MAX } else { width };
    let mut field: Vec<u8> = Vec::new();
    let mut sign_ok = true;
    let mut no_digits = true;
    let mut no_zero_digits = true;
    let mut have_sign = false;
    let mut pfx_ok = prefix_ok;
    let mut at = 0usize;
    while at < input.len() && field.len() < limit {
        let c = input[at];
        let ok = match c {
            b'0' => {
                if base == 0 {
                    base = 8;
                    pfx_ok = true;
                }
                if no_zero_digits {
                    sign_ok = false;
                    no_zero_digits = false;
                    no_digits = false;
                } else {
                    sign_ok = false;
                    pfx_ok = false;
                    no_digits = false;
                }
                true
            }
            b'1'..=b'7' => {
                if base == 0 {
                    base = 10;
                }
                sign_ok = false;
                pfx_ok = false;
                no_digits = false;
                true
            }
            b'8' | b'9' => {
                if base == 0 {
                    base = 10;
                }
                if base <= 8 {
                    false
                } else {
                    sign_ok = false;
                    pfx_ok = false;
                    no_digits = false;
                    true
                }
            }
            b'a'..=b'f' | b'A'..=b'F' => {
                if base <= 10 {
                    false
                } else {
                    sign_ok = false;
                    pfx_ok = false;
                    no_digits = false;
                    true
                }
            }
            b'+' | b'-' => {
                if sign_ok {
                    sign_ok = false;
                    have_sign = true;
                    true
                } else {
                    false
                }
            }
            b'x' | b'X' => {
                if pfx_ok && field.len() == 1 + usize::from(have_sign) {
                    base = 16;
                    pfx_ok = false;
                    true
                } else {
                    false
                }
            }
            _ => false,
        };
        if !ok {
            break;
        }
        field.push(c);
        at += 1;
    }
    if no_digits {
        return None;
    }
    // A bare `0x`: the `x` is given back and the field is the `0`.
    if matches!(field.last(), Some(b'x' | b'X')) {
        field.pop();
    }
    let base = if base == 0 { 10 } else { base };
    Some((field.len(), convert(&field, base, unsigned)))
}

/// `strtoimax` or `strtoumax` on a field `integer` has already shaped.
fn convert(field: &[u8], base: u32, unsigned: bool) -> u64 {
    let (negative, mut digits) = match field.first() {
        Some(b'-') => (true, &field[1..]),
        Some(b'+') => (false, &field[1..]),
        _ => (false, field),
    };
    if base == 16 && digits.len() >= 2 && digits[0] == b'0' && matches!(digits[1], b'x' | b'X') {
        digits = &digits[2..];
    }
    let mut magnitude: u64 = 0;
    let mut overflow = false;
    for &d in digits {
        let value = (d as char).to_digit(base).expect("the field holds digits of its base") as u64;
        match magnitude.checked_mul(u64::from(base)).and_then(|m| m.checked_add(value)) {
            Some(m) => magnitude = m,
            None => overflow = true,
        }
    }
    if unsigned {
        if overflow {
            u64::MAX
        } else if negative {
            magnitude.wrapping_neg()
        } else {
            magnitude
        }
    } else if negative {
        if overflow || magnitude > 1 << 63 {
            i64::MIN as u64
        } else {
            (magnitude as i64).wrapping_neg() as u64
        }
    } else if overflow || magnitude > i64::MAX as u64 {
        i64::MAX as u64
    } else {
        magnitude
    }
}
