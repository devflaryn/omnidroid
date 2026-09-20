//! printf-style formatting core (Phase 6 stretch).
//!
//! `__vsnprintf_chk` and `vsprintf` are variadic and belong to the unreviewed thunk
//! boundary — but their *work* is pure formatting. This module implements that formatting
//! engine over an **explicit argument list** ([`FormatArg`]), so the future adapter can
//! marshal the guest's va_list into a `&[FormatArg]` and call [`format()`]; nothing here
//! knows about variadics.
//!
//! Scope and guarantees (honest, per the task's partial-is-acceptable rule):
//!
//! * conversions: `d i u o x X c s p e E f F g G a A %%`;
//! * flags `- + space # 0`; width including `*`; precision including `.*`;
//! * length modifiers honoured where they affect the value type: `hh h l ll z j t`
//!   (LP64: `l` == `ll` == 64-bit; `z` = u64; `j` = i64/u64; `t` = i64 — the caller
//!   passes the right-width value in the `FormatArg`, the modifier is validated and
//!   recorded);
//! * `%n` is **NOT supported** (Android/bionic: unavailable): `format` returns
//!   `Err(FormatError::NNotSupported)` and never writes through it — it is a classic
//!   exploit primitive;
//! * exponents always have **at least two digits** (C standard; Windows differs);
//! * `%p` prints `0x` + lowercase hex, `(nil)` for null — bionic/glibc behaviour;
//! * `a A` hex floats: implemented via exact bit decomposition, correctly rounded
//!   shortest-style is NOT attempted — the standard only requires a correctly rounded
//!   hex representation, which is what bit decomposition gives exactly;
//! * `g G`: trailing zeros removed unless `#`, exponent threshold per C (`< -4` or
//!   `>= precision`);
//!
//! NOT implemented (documented gaps): positional arguments (`%n$`), thousands grouping
//! (`'` flag), wide-string `%ls`/`%lc` (the adapter would need to convert UTF-32 → UTF-8
//! first; passing a `FormatArg::Str` covers the reachable initializers' needs), and
//! locale-dependent decimal points (always `.` in the C locale).

/// The widest field one conversion will produce.
///
/// **A policy number, stated as one.** 64 KiB matches the boundary's own `STRING_LIMIT`, which
/// is set where it is because nothing bionic's interfaces produce is longer — `PATH_MAX` is
/// 4096, a log line is 4096. It is not a correctness bound: it is the point past which honouring
/// a guest-chosen width stops being formatting and starts being an allocation the guest picked.
pub const MAX_FIELD_WIDTH: usize = 64 * 1024;

/// The most one [`format`] call will produce.
///
/// Capping a single field is not enough on its own: a format string may repeat a wide
/// conversion. Checked once per loop iteration, which also bounds the literal bytes.
pub const MAX_OUTPUT: usize = 1024 * 1024;

/// One printf argument. The adapter builds these from the guest's va_list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FormatArg<'a> {
    /// `int` (`d i c` after the modifier dance).
    Int(i64),
    /// `unsigned int`/`size_t`/... (`u o x X p`).
    UInt(u64),
    /// Pointer (`p`, or `%s` pointing at a NUL-terminated guest string).
    Ptr(u64),
    /// `double` (`e E f F g G a A`).
    Double(f64),
    /// NUL-terminated string content, already copied out of guest memory by the adapter
    /// (length modifiers do not affect strings).
    Str(&'a str),
}

/// A formatting failure. `Display` names the problem; nothing here writes through `%n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// `%n` was requested: unsupported on Android by design.
    NNotSupported,
    /// The format string referenced an argument that was not supplied.
    MissingArgument,
    /// The conversion specifier is unknown to this engine.
    UnknownSpecifier(char),
    /// The format string itself is malformed (e.g. a trailing `%`).
    MalformedFormat(&'static str),
    /// A `long double` conversion (`%Lf`, `%Le`, `%La`, `%Lg`) was requested.
    ///
    /// **Refused before any argument is read, on purpose.** On Android/LP64 a `long double`
    /// is a 128-bit quad passed in a 16-byte variadic slot, and every argument walker this
    /// crate is used with steps in 8-byte units. Reading one as a `double` would return a
    /// number that is wrong rather than imprecise, and the next conversion would then read
    /// the wrong half of it — a plausible wrong answer propagating through the rest of the
    /// format. The `char` is the conversion that asked.
    LongDoubleUnsupported(char),
    /// One conversion asked for a field wider than [`MAX_FIELD_WIDTH`].
    ///
    /// **Hostile input, not a limitation.** A width is guest-controlled — `%999999999d` in a
    /// format string, or a `*` width taken from an argument — and honouring one would make
    /// `emit_padded` push that many characters. At `usize::MAX` that is an allocation failure,
    /// which aborts, and Global Constraint 11 calls an abort reachable from untrusted input
    /// Critical because no caller can contain it.
    FieldTooWide {
        /// The conversion that asked.
        conversion: char,
        /// What it asked for.
        requested: usize,
        /// The cap it passed.
        limit: usize,
    },
    /// The formatted output passed [`MAX_OUTPUT`].
    ///
    /// The companion bound to [`FieldTooWide`](FormatError::FieldTooWide): capping one field
    /// still leaves a format string free to repeat a wide conversion thousands of times.
    OutputTooLarge {
        /// The cap that was passed.
        limit: usize,
    },
    /// A wide conversion (`%ls`, `%lc`) was requested.
    ///
    /// Refused rather than treated as its narrow form: on Android `wchar_t` is **32 bits**,
    /// so the argument is a `wchar_t*`/`wint_t` and reading it as a byte string or a byte
    /// would print the first character followed by garbage. The `char` is the conversion.
    WideUnsupported(char),
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FormatError::NNotSupported => write!(f, "%n is not supported on Android"),
            FormatError::MissingArgument => write!(f, "format references a missing argument"),
            FormatError::UnknownSpecifier(c) => write!(f, "unknown conversion '%{c}'"),
            FormatError::MalformedFormat(why) => write!(f, "malformed format: {why}"),
            FormatError::LongDoubleUnsupported(c) => write!(
                f,
                "%L{c}: long double is a 128-bit quad on Android and is not formatted here"
            ),
            FormatError::FieldTooWide { conversion, requested, limit } => write!(
                f,
                "%{conversion} asked for a field {requested} characters wide, past the {limit}                  this formatter will produce for one conversion"
            ),
            FormatError::OutputTooLarge { limit } => {
                write!(f, "the formatted output passed {limit} bytes and was stopped")
            }
            FormatError::WideUnsupported(c) => {
                write!(f, "%l{c}: wide characters (32-bit wchar_t) are not formatted here")
            }
        }
    }
}
impl std::error::Error for FormatError {}

/// Flags parsed from the format specification.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Flags {
    /// `-`: left-justify.
    left: bool,
    /// `+`: force a sign for signed conversions.
    plus: bool,
    /// ` `: space for positive signed values (ignored when `+` present).
    space: bool,
    /// `#`: alternate form (0x/0X for x/X, decimal point for e/f/g, etc.).
    alt: bool,
    /// `0`: zero-pad (ignored when `-` present).
    zero: bool,
}

/// Format `args` according to the format string into `out`, returning the number of
/// characters written (what snprintf would return, excluding the NUL).
///
/// The format string is a host `&str` — the adapter copies it out of guest memory first
/// (NUL-terminated by definition; a fault there is the adapter's problem, not ours).
pub fn format(
    fmt: &str,
    args: &[FormatArg],
    out: &mut String,
) -> Result<usize, FormatError> {
    let start_len = out.len();
    let mut arg_idx = 0usize;
    let bytes = fmt.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        // Checked here rather than at each push: every path through the loop body appends at
        // most one field, and a field is capped below, so one test per iteration bounds the
        // whole call at MAX_OUTPUT + MAX_FIELD_WIDTH.
        if out.len() - start_len > MAX_OUTPUT {
            return Err(FormatError::OutputTooLarge { limit: MAX_OUTPUT });
        }
        let b = bytes[i];
        if b != b'%' {
            out.push(b as char); // format strings with %s args stay ASCII-safe in practice;
            i += 1;             // non-ASCII bytes are copied verbatim as chars < 0x100 —
            continue;           // documented: fmt is expected ASCII (C locale)
        }
        i += 1;
        if i >= bytes.len() {
            return Err(FormatError::MalformedFormat("trailing %"));
        }
        // One parser, shared with [`plan`]. Two copies of this would be two chances to
        // disagree about where an argument sits, and a `printf` whose planner and whose
        // formatter disagree by one argument prints the *next* argument for every
        // conversion after the first — a plausible wrong answer, which is the one outcome
        // this crate forbids.
        let (parsed, next) = parse_spec(bytes, i)?;
        i = next;
        let mut flags = parsed.flags;
        let length = parsed.length;
        let spec = parsed.conv;
        // `*` width and precision consume their argument BEFORE the conversion character is
        // acted on — including for `%%` — which is what `plan` mirrors.
        let width: Option<usize> = match parsed.width {
            Count::Absent => None,
            Count::Fixed(n) => Some(n),
            Count::Star => match next_arg(args, &mut arg_idx)? {
                FormatArg::Int(n) if *n >= 0 => Some(*n as usize),
                FormatArg::Int(n) => {
                    // Negative width: left-justify with magnitude (C behaviour).
                    flags.left = true;
                    Some(n.unsigned_abs() as usize)
                }
                _ => return Err(FormatError::MalformedFormat("'*' width needs an int")),
            },
        };
        let precision: Option<usize> = match parsed.precision {
            Count::Absent => None,
            Count::Fixed(n) => Some(n),
            Count::Star => match next_arg(args, &mut arg_idx)? {
                FormatArg::Int(n) if *n >= 0 => Some(*n as usize),
                FormatArg::Int(_) => None, // negative precision = omitted
                _ => return Err(FormatError::MalformedFormat("'*' precision needs an int")),
            },
        };
        // Both the literal `%999999999d` and the `*` form arrive here, which is why the check is
        // after the fetch rather than in `plan`: a `*` width is an argument and the format string
        // alone cannot be scanned for it.
        for (requested, what) in [(width, "width"), (precision, "precision")] {
            let _ = what;
            if let Some(n) = requested {
                if n > MAX_FIELD_WIDTH {
                    return Err(FormatError::FieldTooWide {
                        conversion: spec,
                        requested: n,
                        limit: MAX_FIELD_WIDTH,
                    });
                }
            }
        }
        let arg = match spec {
            '%' => {
                out.push('%');
                continue;
            }
            _ => next_arg(args, &mut arg_idx)?,
        };
        match spec {
            'd' | 'i' => {
                let n = as_int(arg, spec, length)?;
                let mut digits = n.unsigned_abs().to_string();
                if let Some(p) = precision {
                    pad_zero(&mut digits, p); // precision on integers = minimum digit count
                    if n == 0 && p == 0 {
                        digits.clear(); // C: precision 0 on zero value prints nothing
                    }
                }
                let sign = if n < 0 {
                    "-"
                } else if flags.plus {
                    "+"
                } else if flags.space {
                    " "
                } else {
                    ""
                };
                emit_padded(out, &format!("{sign}{digits}"), width, &flags, &precision);
            }
            'u' => {
                let n = as_uint(arg, spec, length)?;
                let mut digits = n.to_string();
                if let Some(p) = precision {
                    pad_zero(&mut digits, p);
                    if n == 0 && p == 0 {
                        digits.clear(); // C: precision 0 on zero value prints nothing
                    }
                }
                emit_padded(out, &digits, width, &flags, &precision);
            }
            'o' | 'x' | 'X' => {
                // Alternate form: leading 0 for %o (only when it adds a zero), 0x/0X for x/X.
                let n = as_uint(arg, spec, length)?;
                let digits = match spec {
                    'o' => format!("{n:o}"),
                    'x' => format!("{n:x}"),
                    _ => format!("{n:X}"),
                };
                let prefix = match spec {
                    'o' if flags.alt && !digits.starts_with('0') => "0",
                    'x' if flags.alt && n != 0 => "0x",
                    'X' if flags.alt && n != 0 => "0X",
                    _ => "",
                };
                let mut body = digits;
                if let Some(p) = precision {
                    pad_zero(&mut body, p);
                    if n == 0 && p == 0 {
                        body.clear(); // C: "#." still prints empty for zero value
                    }
                }
                let prefix = if prefix == "0" && body.starts_with('0') { "" } else { prefix };
                emit_padded(out, &format!("{prefix}{body}"), width, &flags, &precision);
            }
            'c' => {
                let n = as_int(arg, spec, length)?;
                // %c takes an int converted to unsigned char.
                let ch = (n as u32 & 0xFF) as u8 as char;
                emit_padded(out, &ch.to_string(), width, &flags, &precision);
            }
            's' => {
                let s = match arg {
                    FormatArg::Str(s) => s,
                    FormatArg::Ptr(0) => "(null)", // bionic prints "(null)" for %p-style nulls in %s too
                    _ => return Err(FormatError::MalformedFormat("%s needs a string")),
                };
                // Precision truncates strings.
                let s = match precision {
                    Some(p) if s.len() > p => &s[..p],
                    _ => s,
                };
                emit_padded(out, s, width, &flags, &precision);
            }
            'p' => {
                let ptr = match arg {
                    FormatArg::Ptr(p) => p,
                    FormatArg::UInt(p) => p,
                    _ => return Err(FormatError::MalformedFormat("%p needs a pointer")),
                };
                let body = if *ptr == 0 {
                    "(nil)".to_string() // bionic/glibc: %p of NULL prints (nil)
                } else {
                    format!("0x{ptr:x}")
                };
                emit_padded(out, &body, width, &flags, &precision);
            }
            'e' | 'E' => {
                let v = as_double(arg, spec)?;
                let s = format_exp(v, precision.unwrap_or(6), flags.alt, spec == 'E');
                emit_padded(out, &s, width, &flags, &precision);
            }
            'f' | 'F' => {
                let v = as_double(arg, spec)?;
                let s = format_fixed(v, precision.unwrap_or(6), flags.alt, spec == 'F');
                emit_padded(out, &s, width, &flags, &precision);
            }
            'g' | 'G' => {
                let v = as_double(arg, spec)?;
                let s = format_g(v, precision.unwrap_or(6), flags.alt, spec == 'G');
                emit_padded(out, &s, width, &flags, &precision);
            }
            'a' | 'A' => {
                let v = as_double(arg, spec)?;
                let s = format_hex_float(v, spec == 'A');
                emit_padded(out, &s, width, &flags, &precision);
            }
            other => return Err(FormatError::UnknownSpecifier(other)),
        }
    }
    Ok(out.len() - start_len)
}

// ---------------------------------------------------------------------------
// The shared specification parser, and the pre-scan the adapter needs
// ---------------------------------------------------------------------------

/// A width or precision, before any `*` argument has been fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Count {
    /// Not written at all.
    Absent,
    /// Written as digits.
    Fixed(usize),
    /// Written as `*`: it comes from an argument.
    Star,
}

/// One parsed conversion specification, with no argument fetched yet.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Spec {
    flags: Flags,
    width: Count,
    precision: Count,
    length: &'static str,
    conv: char,
}

/// Parse the conversion specification whose `%` has already been consumed, starting at
/// `bytes[at]`. Returns the specification and the index one past the conversion character.
///
/// This is the **only** parser: [`format`] and [`plan`] both go through it, so the count and
/// the order of the arguments a format string needs cannot differ between planning the call
/// and performing it.
fn parse_spec(bytes: &[u8], at: usize) -> Result<(Spec, usize), FormatError> {
    let mut i = at;
    // Flags (in C's parse order, repetitions allowed).
    let mut flags = Flags::default();
    loop {
        if i >= bytes.len() {
            return Err(FormatError::MalformedFormat("flags run to end"));
        }
        match bytes[i] {
            b'-' => flags.left = true,
            b'+' => flags.plus = true,
            b' ' => flags.space = true,
            b'#' => flags.alt = true,
            b'0' => flags.zero = true,
            _ => break,
        }
        i += 1;
    }
    // Width (digits or '*').
    let mut width = Count::Absent;
    if bytes[i] == b'*' {
        i += 1;
        width = Count::Star;
    } else {
        let mut digits: Option<usize> = None;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            // Saturating, not wrapping: a hostile format string of thirty nines must not wrap
            // a width into a small number, nor panic in a debug build. A saturated width is
            // refused downstream by the padding it would need, and never silently becomes a
            // *different* width.
            digits = Some(
                digits
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add((bytes[i] - b'0') as usize),
            );
            i += 1;
        }
        if let Some(n) = digits {
            width = Count::Fixed(n);
        }
    }
    if i >= bytes.len() {
        return Err(FormatError::MalformedFormat("width runs to end"));
    }
    // Precision ('.' then digits or '*'; '.' alone means precision 0).
    let mut precision = Count::Absent;
    if bytes[i] == b'.' {
        i += 1;
        if i >= bytes.len() {
            return Err(FormatError::MalformedFormat("precision runs to end"));
        }
        if bytes[i] == b'*' {
            i += 1;
            precision = Count::Star;
        } else {
            let mut digits = 0usize;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                digits = digits.saturating_mul(10).saturating_add((bytes[i] - b'0') as usize);
                i += 1;
            }
            precision = Count::Fixed(digits);
        }
    }
    if i >= bytes.len() {
        return Err(FormatError::MalformedFormat("spec runs to end"));
    }
    // Length modifier (validated; the value's width comes from the FormatArg itself).
    let mut length = "";
    while let b'h' | b'l' | b'z' | b'j' | b't' | b'q' | b'L' = bytes[i] {
        if (bytes[i] == b'h' || bytes[i] == b'l') && i + 1 < bytes.len() && bytes[i + 1] == bytes[i]
        {
            length = if bytes[i] == b'h' { "hh" } else { "ll" };
            i += 1;
        } else {
            length = match bytes[i] {
                b'h' => "h",
                b'l' => "l",
                b'z' => "z",
                b'j' => "j",
                b't' => "t",
                b'q' => "q",
                _ => "L",
            };
        }
        i += 1;
        if i >= bytes.len() {
            return Err(FormatError::MalformedFormat("length runs to end"));
        }
    }
    // Conversion.
    let conv = bytes[i] as char;
    i += 1;
    if conv == 'n' {
        return Err(FormatError::NNotSupported);
    }
    // Unknown specifiers are rejected BEFORE any argument is consumed (the argument
    // count stays meaningful for callers that pre-scan) and before any output.
    if conv != '%'
        && !matches!(
            conv,
            'd' | 'i'
                | 'u'
                | 'o'
                | 'x'
                | 'X'
                | 'c'
                | 's'
                | 'p'
                | 'e'
                | 'E'
                | 'f'
                | 'F'
                | 'g'
                | 'G'
                | 'a'
                | 'A'
        )
    {
        return Err(FormatError::UnknownSpecifier(conv));
    }
    Ok((Spec { flags, width, precision, length, conv }, i))
}

/// The kind of value one variadic argument must be fetched as.
///
/// A C variadic call carries no type information, so the *format string* is the only
/// description of its own arguments. An adapter marshalling a guest `printf` has to know
/// which register bank each argument sits in **before** it reads one, because reading a
/// `double` out of the integer bank does not fail — it returns a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// A signed integer: `d i c`, and every `*` width or precision.
    Int,
    /// An unsigned integer: `u o x X`.
    UInt,
    /// A pointer: `p`.
    Ptr,
    /// A `double`, from the floating-point bank: `e E f F g G a A`.
    Double,
    /// A `const char *` whose bytes the adapter reads out of guest memory for `%s`.
    Str,
}

/// Walk `fmt` and report, in order, the kind of every argument it will consume.
///
/// The counterpart to [`format()`]: an adapter calls `plan` first, fetches exactly these
/// arguments from wherever its ABI keeps them, and then calls `format` with the resulting
/// slice. Both walk the format string through the same parser, so the list `plan` returns
/// is exactly the list `format` will ask for.
///
/// # Errors
///
/// Every [`FormatError`] the parser can raise, plus
/// [`FormatError::LongDoubleUnsupported`] and [`FormatError::WideUnsupported`] — both
/// raised **here**, before the caller reads anything, because neither has a correct
/// 8-byte read and a guess would be a plausible wrong number.
pub fn plan(fmt: &str) -> Result<Vec<ArgKind>, FormatError> {
    let bytes = fmt.as_bytes();
    let mut i = 0usize;
    let mut kinds = Vec::new();
    while i < bytes.len() {
        if bytes[i] != b'%' {
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            return Err(FormatError::MalformedFormat("trailing %"));
        }
        let (spec, next) = parse_spec(bytes, i)?;
        i = next;
        // `format` fetches the `*` arguments before it looks at the conversion character,
        // `%%` included, so the pre-scan has to do the same or the two walks diverge by one
        // argument on `%*%`.
        if spec.width == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.precision == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.conv == '%' {
            continue;
        }
        kinds.push(kind_of(spec.conv, spec.length)?);
    }
    Ok(kinds)
}

/// The argument kind one conversion needs, refusing the two shapes that have no correct
/// 8-byte read on Android/LP64.
fn kind_of(conv: char, length: &str) -> Result<ArgKind, FormatError> {
    match conv {
        'e' | 'E' | 'f' | 'F' | 'g' | 'G' | 'a' | 'A' => {
            if length == "L" {
                // AAPCS64 gives a 128-bit quad a 16-byte slot; there is no 16-byte variadic
                // read anywhere in this stack, so the only honest answer is a refusal.
                return Err(FormatError::LongDoubleUnsupported(conv));
            }
            Ok(ArgKind::Double)
        }
        's' => {
            if length == "l" {
                return Err(FormatError::WideUnsupported(conv));
            }
            Ok(ArgKind::Str)
        }
        'c' => {
            if length == "l" {
                return Err(FormatError::WideUnsupported(conv));
            }
            Ok(ArgKind::Int)
        }
        'd' | 'i' => Ok(ArgKind::Int),
        'u' | 'o' | 'x' | 'X' => Ok(ArgKind::UInt),
        'p' => Ok(ArgKind::Ptr),
        other => Err(FormatError::UnknownSpecifier(other)),
    }
}

/// Fetch the next variadic argument.
fn next_arg<'a>(args: &'a [FormatArg<'a>], idx: &mut usize) -> Result<&'a FormatArg<'a>, FormatError> {
    let a = args.get(*idx).ok_or(FormatError::MissingArgument)?;
    *idx += 1;
    Ok(a)
}

/// Coerce an argument to the signed integer the conversion needs, then apply the
/// length modifier's truncation exactly as C does: the *argument* is `int`-width (the
/// modifier only tells printf how many bits to print). `hh` prints the value converted
/// to `signed char`, `h` to `short`; wider modifiers (`l ll z j t`) print the full 64-bit
/// value on LP64.
fn as_int(arg: &FormatArg, spec: char, length: &str) -> Result<i64, FormatError> {
    let raw = match arg {
        FormatArg::Int(n) => *n,
        FormatArg::UInt(n) => *n as i64,
        FormatArg::Ptr(n) => *n as i64,
        _ => return Err(annotate(FormatError::MalformedFormat("argument is not an integer"), spec)),
    };
    Ok(match length {
        "hh" => raw as i8 as i64,
        "h" => raw as i16 as i64,
        _ => raw,
    })
}

/// Unsigned form of [`as_int`]: `hh` prints `raw as u8`, `h` prints `raw as u16`.
fn as_uint(arg: &FormatArg, _spec: char, length: &str) -> Result<u64, FormatError> {
    let raw = match arg {
        FormatArg::Int(n) => *n as u64,
        FormatArg::UInt(n) => *n,
        FormatArg::Ptr(n) => *n,
        _ => return Err(FormatError::MalformedFormat("argument is not an integer")),
    };
    Ok(match length {
        "hh" => raw as u8 as u64,
        "h" => raw as u16 as u64,
        _ => raw,
    })
}

fn as_double(arg: &FormatArg, _spec: char) -> Result<f64, FormatError> {
    match arg {
        FormatArg::Double(v) => Ok(*v),
        FormatArg::Int(n) => Ok(*n as f64),
        FormatArg::UInt(n) => Ok(*n as f64),
        _ => Err(FormatError::MalformedFormat("argument is not a float")),
    }
}

fn annotate(e: FormatError, _spec: char) -> FormatError {
    e
}

/// Zero-extend `digits` to at least `p` characters (integer precision).
fn pad_zero(digits: &mut String, p: usize) {
    while digits.len() < p {
        digits.insert(0, '0');
    }
}

/// Pad `body` to `width` per the flags. Zero-padding goes after any sign/prefix.
/// The `0` flag is ignored for integer conversions when a precision is given (C11
/// 7.21.6.1: "if a precision is specified, the 0 flag is ignored" for d,i,o,u,x,X).
fn emit_padded(
    out: &mut String,
    body: &str,
    width: Option<usize>,
    flags: &Flags,
    precision: &Option<usize>,
) {
    let pad = width.unwrap_or(0).saturating_sub(body.len());
    if pad == 0 {
        out.push_str(body);
        return;
    }
    let zero_ok = flags.zero && precision.is_none() && !numeric_needs_space_first(body);
    if flags.left {
        out.push_str(body);
        out.extend(core::iter::repeat_n(' ', pad));
    } else if zero_ok {
        // Zero padding must respect the sign/prefix position: "0x...", "-12" etc.
        let split = prefix_len(body);
        out.push_str(&body[..split]);
        out.extend(core::iter::repeat_n('0', pad));
        out.push_str(&body[split..]);
    } else {
        out.extend(core::iter::repeat_n(' ', pad));
        out.push_str(body);
    }
}

/// Whether padding must be spaces even with the `0` flag (inf/nan and strings).
fn numeric_needs_space_first(body: &str) -> bool {
    body.starts_with("inf")
        || body.starts_with("-inf")
        || body.starts_with("+inf")
        || body.starts_with("nan")
        || body.starts_with("-nan")
        || body.starts_with("+nan")
        || body.starts_with("(nil)")
        || body.is_empty()
}

/// Length of the sign/hex prefix that zero-padding must not overwrite.
fn prefix_len(body: &str) -> usize {
    let b = body.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+' || b[i] == b' ') {
        i += 1;
    }
    if i + 1 < b.len() && b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X') {
        i += 2;
    }
    i
}

/// `%e`/`%E`: fixed mantissa + exponent with **at least two exponent digits** (C
/// standard; Windows CRT does NOT do this — do not "fix" it to match the host).
/// The mantissa digits come from decimal scaling + a single rounding step; the carry
/// case (0.999… → 1.000…) renormalises the exponent.
fn format_exp(v: f64, precision: usize, _alt: bool, upper: bool) -> String {
    if v.is_nan() {
        return if upper { "NAN".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        let s = if v < 0.0 { "-inf" } else { "inf" };
        return if upper { s.to_uppercase() } else { s.into() };
    }
    let sign = if v.is_sign_negative() { "-" } else { "" };
    let (m, e) = decimal_decompose(v.abs());
    // Round the mantissa to `precision` fraction digits: work in integer digits.
    let scale = 10u64.pow(precision.min(15) as u32) as f64;
    let mut rounded = (m * scale).round() / scale;
    let mut exp = e;
    if rounded >= 10.0 {
        rounded /= 10.0;
        exp += 1;
    }
    // Build "d.ffff" from the rounded value: integer digit + fraction digits.
    let int_part = rounded.trunc() as u64; // 1..=9 (or 0 for v == 0)
    let frac_part = ((rounded - rounded.trunc()) * scale).round() as u64;
    let e_char = if upper { 'E' } else { 'e' };
    if precision == 0 {
        format!("{sign}{int_part}{e_char}{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        format!(
            "{sign}{int_part}.{:0width$}{e_char}{}{:02}",
            frac_part,
            if exp < 0 { '-' } else { '+' },
            exp.abs(),
            width = precision
        )
    }
}

/// Decompose a positive finite double into `(d.dddddd, exp)` with the mantissa in [1, 10).
/// Exact for the common magnitudes; the multiplier walk is exact in binary steps.
fn decimal_decompose(a: f64) -> (f64, i32) {
    if a == 0.0 {
        return (0.0, 0);
    }
    let mut m = a;
    let mut e = 0i32;
    while m >= 10.0 {
        m /= 10.0;
        e += 1;
    }
    while m < 1.0 {
        m *= 10.0;
        e -= 1;
    }
    (m, e)
}

/// `%f`/`%F`: fixed notation with the given precision (always a decimal point under `#`).
fn format_fixed(v: f64, precision: usize, alt: bool, upper: bool) -> String {
    if v.is_nan() {
        return if upper { "NAN".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        let s = if v < 0.0 { "-inf" } else { "inf" };
        return if upper { s.to_uppercase() } else { s.into() };
    }
    let s = format!("{:.*}", precision, v);
    if alt && precision == 0 && !s.contains('.') {
        return format!("{s}.");
    }
    s
}

/// `%g`/`%G`: shortest of e/f styles per C rules; trailing zeros removed unless `#`.
fn format_g(v: f64, precision: usize, alt: bool, upper: bool) -> String {
    if v.is_nan() {
        return if upper { "NAN".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        let s = if v < 0.0 { "-inf" } else { "inf" };
        return if upper { s.to_uppercase() } else { s.into() };
    }
    let p = if precision == 0 { 1 } else { precision };
    let e_char = if upper { 'E' } else { 'e' };
    if v == 0.0 {
        return if alt { format!("0.{:0p$}", 0, p = p - 1) } else { "0".into() };
    }
    let (_, exp) = decimal_decompose(v.abs());
    // C rule: style e if exp < -4 or exp >= p; else style f with precision p-1-exp.
    if exp < -4 || exp >= p as i32 {
        let s = format_exp(v, p.saturating_sub(1), alt, upper);
        // %g strips trailing zeros in the mantissa unless #.
        if !alt {
            return strip_exp_zeros(s, e_char);
        }
        s
    } else {
        let s = format_fixed(v, (p as i32 - 1 - exp).max(0) as usize, alt, upper);
        if !alt {
            strip_fixed_zeros(s)
        } else {
            s
        }
    }
}

fn strip_exp_zeros(s: String, e_char: char) -> String {
    let Some(epos) = s.find(e_char) else { return s };
    let (mant, exp) = s.split_at(epos);
    let mant = if mant.contains('.') {
        mant.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        mant.to_string()
    };
    format!("{mant}{exp}")
}

fn strip_fixed_zeros(s: String) -> String {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// `%a`/`%A`: hex float via exact bit decomposition, with **trailing zero nibbles
/// stripped** from the fraction and the exponent in **minimal digits with an explicit
/// sign** — the C standard's "only as many digits as necessary" rule, which is what
/// bionic and glibc print (`0x1p+0`, `0x1.8p+1`), NOT `0x1.0000000000000p+00`.
fn format_hex_float(v: f64, upper: bool) -> String {
    if v.is_nan() {
        return if upper { "NAN".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        let s = if v < 0.0 { "-inf" } else { "inf" };
        return if upper { s.to_uppercase() } else { s.into() };
    }
    let bits = v.to_bits();
    let sign = if bits >> 63 == 1 { "-" } else { "" };
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let mantissa = bits & ((1u64 << 52) - 1);
    let p_char = if upper { 'P' } else { 'p' };
    let prefix = if upper { "0X" } else { "0x" };
    let fmt_exp = |exp: i32| format!("{p_char}{}{}", if exp < 0 { '-' } else { '+' }, exp.abs());
    if biased == 0 {
        if mantissa == 0 {
            // ±0: "0x0p+0".
            return format!("{sign}{prefix}0{}", fmt_exp(0));
        }
        // Subnormal: exponent fixed at -1022; strip trailing zero nibbles.
        let nibbles = format!("{:013x}", mantissa);
        let nibbles = nibbles.trim_end_matches('0');
        return format!("{sign}{prefix}0.{nibbles}{}", fmt_exp(-1022));
    }
    let exp = biased - 1023;
    // Leading hex digit: the implicit 1 makes it always "1". Strip trailing zeros
    // from the 13 fraction nibbles; if none remain, omit the decimal point.
    let nibbles = format!("{:013x}", mantissa);
    let nibbles = nibbles.trim_end_matches('0');
    if nibbles.is_empty() {
        format!("{sign}{prefix}1{}", fmt_exp(exp))
    } else {
        format!("{sign}{prefix}1.{nibbles}{}", fmt_exp(exp))
    }
}
