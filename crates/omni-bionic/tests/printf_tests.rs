//! Phase 6 tests: the printf formatting engine.
//!
//! Oracles: the **C standard's** own conversion rules and bionic's documented behaviour —
//! deliberately NOT the Windows CRT. Every expected string is hand-derived:
//! * `%p` → `0x` + lowercase hex; `(nil)` for null;
//! * `%n` must error, never write;
//! * exponents ≥ 2 digits (`1e+05` style);
//! * `%g` trailing-zero stripping, `#` keeps them; style switch at exp < -4 or >= precision;
//! * `#` forms: `0x` prefix, forced decimal point;
//! * zero-padding places zeros after sign/prefix.

use omni_bionic::printf::{format, plan, ArgKind, FormatArg, FormatError};

fn fmt(fmt_str: &str, args: &[FormatArg]) -> String {
    let mut out = String::new();
    format(fmt_str, args, &mut out).unwrap();
    out
}

#[test]
fn plain_text_and_percent() {
    assert_eq!(fmt("hello", &[]), "hello");
    assert_eq!(fmt("100%%", &[]), "100%");
}

#[test]
fn d_i_signed_forms() {
    assert_eq!(fmt("%d", &[FormatArg::Int(42)]), "42");
    assert_eq!(fmt("%d", &[FormatArg::Int(-42)]), "-42");
    assert_eq!(fmt("%i", &[FormatArg::Int(7)]), "7");
    // Flags: + and space.
    assert_eq!(fmt("%+d", &[FormatArg::Int(42)]), "+42");
    assert_eq!(fmt("%+d", &[FormatArg::Int(-42)]), "-42");
    assert_eq!(fmt("% d", &[FormatArg::Int(42)]), " 42");
    assert_eq!(fmt("% d", &[FormatArg::Int(-42)]), "-42");
    // C11 7.21.6.1p5: the space flag is IGNORED when + is present — never "+ 42".
    assert_eq!(fmt("%+ d", &[FormatArg::Int(42)]), "+42");
    // Width and zero padding.
    assert_eq!(fmt("%5d", &[FormatArg::Int(42)]), "   42");
    assert_eq!(fmt("%-5d|", &[FormatArg::Int(42)]), "42   |");
    assert_eq!(fmt("%05d", &[FormatArg::Int(42)]), "00042");
    assert_eq!(fmt("%05d", &[FormatArg::Int(-42)]), "-0042"); // zero pad after sign
    // Precision: minimum digits.
    assert_eq!(fmt("%.5d", &[FormatArg::Int(42)]), "00042");
    assert_eq!(fmt("%.2d", &[FormatArg::Int(123)]), "123"); // precision never truncates
    assert_eq!(fmt("%08.3d", &[FormatArg::Int(42)]), "     042");
    // ll (64-bit) values.
    assert_eq!(fmt("%lld", &[FormatArg::Int(9223372036854775807)]), "9223372036854775807");
    assert_eq!(fmt("%lld", &[FormatArg::Int(-9223372036854775808)]), "-9223372036854775808");
}

#[test]
fn u_o_x_x_unsigned() {
    assert_eq!(fmt("%u", &[FormatArg::UInt(42)]), "42");
    // u prints the unsigned value: -1 as unsigned is 2^64-1 (LP64).
    assert_eq!(fmt("%lu", &[FormatArg::UInt(u64::MAX)]), "18446744073709551615");
    assert_eq!(fmt("%x", &[FormatArg::UInt(0xCAFE)]), "cafe");
    assert_eq!(fmt("%X", &[FormatArg::UInt(0xCAFE)]), "CAFE");
    assert_eq!(fmt("%#x", &[FormatArg::UInt(0xCAFE)]), "0xcafe");
    assert_eq!(fmt("%#X", &[FormatArg::UInt(0xCAFE)]), "0XCAFE");
    assert_eq!(fmt("%#x", &[FormatArg::UInt(0)]), "0"); // no 0x for zero
    assert_eq!(fmt("%#o", &[FormatArg::UInt(8)]), "010");
    assert_eq!(fmt("%o", &[FormatArg::UInt(8)]), "10");
    // Precision 0 on zero value prints nothing (C rule).
    assert_eq!(fmt("%.0d", &[FormatArg::Int(0)]), "");
    assert_eq!(fmt("%.0o", &[FormatArg::UInt(0)]), "");
    // Width + zero pad respects 0x prefix.
    assert_eq!(fmt("%#010x", &[FormatArg::UInt(0xCAFE)]), "0x0000cafe");
}

#[test]
fn c_s_p_conversions() {
    assert_eq!(fmt("%c", &[FormatArg::Int(65)]), "A");
    assert_eq!(fmt("%3c|", &[FormatArg::Int(65)]), "  A|");
    assert_eq!(fmt("%s", &[FormatArg::Str("hi")]), "hi");
    assert_eq!(fmt("%5s|", &[FormatArg::Str("hi")]), "   hi|");
    assert_eq!(fmt("%-5s|", &[FormatArg::Str("hi")]), "hi   |");
    // Precision truncates strings.
    assert_eq!(fmt("%.1s", &[FormatArg::Str("hello")]), "h");
    // %p: lowercase hex with 0x; (nil) for null (bionic/glibc).
    assert_eq!(fmt("%p", &[FormatArg::Ptr(0)]), "(nil)");
    assert_eq!(fmt("%p", &[FormatArg::Ptr(0x1234)]), "0x1234");
}

#[test]
fn star_width_and_precision() {
    assert_eq!(fmt("%*d", &[FormatArg::Int(5), FormatArg::Int(42)]), "   42");
    assert_eq!(fmt("%.*s", &[FormatArg::Int(2), FormatArg::Str("hello")]), "he");
    // Negative width: left-justify.
    assert_eq!(fmt("%*d|", &[FormatArg::Int(-5), FormatArg::Int(42)]), "42   |");
    // Negative precision: omitted.
    assert_eq!(fmt("%.*d", &[FormatArg::Int(-3), FormatArg::Int(42)]), "42");
}

#[test]
fn f_e_g_floats() {
    // %f default precision 6.
    assert_eq!(fmt("%f", &[FormatArg::Double(1.5)]), "1.500000");
    assert_eq!(fmt("%.2f", &[FormatArg::Double(1.005)]), "1.00"); // round-half-even of 1.005 at 2 places: binary 1.00499... → 1.00
    assert_eq!(fmt("%.0f", &[FormatArg::Double(2.5)]), "2"); // round-half-even
    assert_eq!(fmt("%.0f#", &[FormatArg::Double(2.5)]), "2#"); // (sanity)
    // # forces a decimal point.
    assert_eq!(fmt("%#.0f", &[FormatArg::Double(2.0)]), "2.");
    // %e: exponent at least two digits.
    assert_eq!(fmt("%e", &[FormatArg::Double(100000.0)]), "1.000000e+05");
    assert_eq!(fmt("%E", &[FormatArg::Double(100000.0)]), "1.000000E+05");
    assert_eq!(fmt("%.1e", &[FormatArg::Double(0.01)]), "1.0e-02");
    // inf/nan.
    assert_eq!(fmt("%f", &[FormatArg::Double(f64::INFINITY)]), "inf");
    assert_eq!(fmt("%F", &[FormatArg::Double(f64::INFINITY)]), "INF");
    assert_eq!(fmt("%e", &[FormatArg::Double(f64::NAN)]), "nan");
    // %g: strips trailing zeros; switches to e-style appropriately.
    assert_eq!(fmt("%g", &[FormatArg::Double(0.0001)]), "0.0001");
    assert_eq!(fmt("%g", &[FormatArg::Double(0.00001)]), "1e-05"); // exp < -4 → e style
    assert_eq!(fmt("%g", &[FormatArg::Double(100000.0)]), "100000"); // exp 5 < precision 6
    assert_eq!(fmt("%g", &[FormatArg::Double(1000000.0)]), "1e+06"); // exp 6 >= 6
    assert_eq!(fmt("%g", &[FormatArg::Double(1.5)]), "1.5");
    assert_eq!(fmt("%#g", &[FormatArg::Double(1.5)]), "1.50000");
}

#[test]
fn a_a_hex_floats() {
    // Hand-derived from the bit decomposition: 1.0 = 0x1p+0.
    assert_eq!(fmt("%a", &[FormatArg::Double(1.0)]), "0x1p+0");
    assert_eq!(fmt("%A", &[FormatArg::Double(1.0)]), "0X1P+0");
    // 2.0 = 0x1p+1; 0.5 = 0x1p-1.
    assert_eq!(fmt("%a", &[FormatArg::Double(2.0)]), "0x1p+1");
    assert_eq!(fmt("%a", &[FormatArg::Double(0.5)]), "0x1p-1");
    // 1.5 = 0x1.8p+0 (mantissa 0x8000000000000).
    assert_eq!(fmt("%a", &[FormatArg::Double(1.5)]), "0x1.8p+0");
    assert_eq!(fmt("%a", &[FormatArg::Double(0.0)]), "0x0p+0");
    assert_eq!(fmt("%a", &[FormatArg::Double(-0.0)]), "-0x0p+0");
    // Subnormal: fixed exponent -1022.
    let smallest = f64::from_bits(1);
    assert_eq!(fmt("%a", &[FormatArg::Double(smallest)]), "0x0.0000000000001p-1022");
}

#[test]
fn n_is_rejected_and_writes_nothing() {
    let mut out = String::from("keep");
    let err = format("%d%n%s", &[FormatArg::Int(1), FormatArg::Ptr(0x5000), FormatArg::Str("x")], &mut out)
        .unwrap_err();
    assert_eq!(err, FormatError::NNotSupported);
    assert_eq!(out, "keep1"); // %d written, %n errored before writing anything more
}

#[test]
fn missing_argument_and_unknown_specifier() {
    assert_eq!(
        format("%d%d", &[FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::MissingArgument)
    );
    assert_eq!(
        format("%y", &[], &mut String::new()),
        Err(FormatError::UnknownSpecifier('y'))
    );
    // Trailing % is malformed.
    assert!(format("abc%", &[], &mut String::new()).is_err());
}

#[test]
fn snprintf_return_value_semantics() {
    // format returns the written character count (snprintf's return excluding NUL).
    let mut out = String::new();
    let n = format("a%db", &[FormatArg::Int(42)], &mut out).unwrap();
    assert_eq!(n, 4);
    assert_eq!(out, "a42b");
}

#[test]
fn length_modifiers_parse_and_pass_through() {
    // The modifier is validated and consumed; the FormatArg carries the value's width.
    // LP64: %ld == %lld == %d-width.
    assert_eq!(fmt("%ld", &[FormatArg::Int(7)]), "7");
    assert_eq!(fmt("%lld", &[FormatArg::Int(7)]), "7");
    assert_eq!(fmt("%hhd", &[FormatArg::Int(300)]), "44"); // (unsigned char)300 = 44
    assert_eq!(fmt("%hd", &[FormatArg::Int(70000)]), "4464"); // (short)70000 = 4464
    assert_eq!(fmt("%zd", &[FormatArg::Int(7)]), "7");
    assert_eq!(fmt("%jd", &[FormatArg::Int(7)]), "7");
    assert_eq!(fmt("%td", &[FormatArg::Int(7)]), "7");
}

// ---------------------------------------------------------------------------
// `plan`: the pre-scan an ABI adapter needs, and the two refusals it owes F6
// ---------------------------------------------------------------------------

/// Build one argument of each planned kind, so a planned call can actually be formatted.
fn sample(kind: ArgKind) -> FormatArg<'static> {
    match kind {
        ArgKind::Int => FormatArg::Int(3),
        ArgKind::UInt => FormatArg::UInt(3),
        ArgKind::Ptr => FormatArg::Ptr(0x1000),
        ArgKind::Double => FormatArg::Double(1.5),
        ArgKind::Str => FormatArg::Str("xy"),
    }
}

/// **The property the adapter depends on.** `plan` and `format` walk the same parser, so
/// for every format string the number and order of arguments `plan` reports is exactly what
/// `format` asks for: supplying that list succeeds, and supplying one fewer fails.
///
/// A silent disagreement of one argument here does not error — it prints the *next*
/// argument for every conversion after the divergence, which is the plausible wrong answer
/// Global Constraint 1 exists for.
#[test]
fn plan_and_format_agree_on_every_argument() {
    let corpus = [
        "",
        "no conversions at all",
        "100%%",
        "%d",
        "%d %d %d",
        "%s=%d",
        "%-8.3f|%+d|% i|%#x|%#o",
        "%u %o %x %X",
        "%p and %s",
        "%e %E %f %F %g %G %a %A",
        "%*d",
        "%.*f",
        "%*.*s",
        "%*%",
        "%lld %zu %jd %td %hhd %hd %qd",
        "%c%c%c",
        "%10s%-10s",
        "mixed %d %f %s %p %x done",
    ];
    for fmt_str in corpus {
        let kinds = plan(fmt_str).unwrap_or_else(|e| panic!("plan({fmt_str:?}) failed: {e}"));
        let args: Vec<FormatArg> = kinds.iter().copied().map(sample).collect();

        let mut out = String::new();
        format(fmt_str, &args, &mut out)
            .unwrap_or_else(|e| panic!("format({fmt_str:?}) with the planned args failed: {e}"));

        // One fewer argument must be detected, which is what proves the count is exact
        // rather than merely sufficient. (A format string needing none is exempt.)
        if !args.is_empty() {
            let short = &args[..args.len() - 1];
            assert_eq!(
                format(fmt_str, short, &mut String::new()),
                Err(FormatError::MissingArgument),
                "{fmt_str:?}: plan said {} arguments, but format was satisfied by {}",
                args.len(),
                short.len()
            );
        }

        // One extra argument must be ignored, not consumed early: the output is unchanged.
        let mut long = args.clone();
        long.push(FormatArg::Int(99));
        let mut out_long = String::new();
        format(fmt_str, &long, &mut out_long).expect("a spare argument is not an error");
        assert_eq!(out, out_long, "{fmt_str:?}: a spare argument changed the output");
    }
}

/// `plan` reports the *kinds*, not just a count: a `%f` must be fetched from the
/// floating-point bank and a `%d` from the integer bank, and an adapter that mixed them up
/// would read a number rather than fail.
#[test]
fn plan_reports_the_bank_each_argument_comes_from() {
    assert_eq!(plan("%d").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%i").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%c").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%u").unwrap(), vec![ArgKind::UInt]);
    assert_eq!(plan("%o").unwrap(), vec![ArgKind::UInt]);
    assert_eq!(plan("%x").unwrap(), vec![ArgKind::UInt]);
    assert_eq!(plan("%X").unwrap(), vec![ArgKind::UInt]);
    assert_eq!(plan("%p").unwrap(), vec![ArgKind::Ptr]);
    assert_eq!(plan("%s").unwrap(), vec![ArgKind::Str]);
    for spec in ['e', 'E', 'f', 'F', 'g', 'G', 'a', 'A'] {
        assert_eq!(plan(&format!("%{spec}")).unwrap(), vec![ArgKind::Double], "%{spec}");
    }
    // `*` width and precision are integers, and they come *before* the conversion's own
    // argument.
    assert_eq!(plan("%*.*f").unwrap(), vec![ArgKind::Int, ArgKind::Int, ArgKind::Double]);
    // `%%` takes no argument of its own but still consumes a `*` width, because `format`
    // fetches the width before it looks at the conversion character.
    assert_eq!(plan("%*%").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%%").unwrap(), Vec::new());
}

/// **F6's refusal.** `long double` is a 128-bit quad on Android/LP64 with a 16-byte variadic
/// slot; nothing in this stack can read one. `plan` refuses **before** the caller reads an
/// argument, so no wrong value is ever fetched, let alone printed.
#[test]
fn plan_refuses_long_double_before_any_argument_is_read() {
    for spec in ['e', 'E', 'f', 'F', 'g', 'G', 'a', 'A'] {
        assert_eq!(
            plan(&format!("%L{spec}")),
            Err(FormatError::LongDoubleUnsupported(spec)),
            "%L{spec} must be refused, not guessed at"
        );
    }
    // The refusal survives flags, width and precision, and it names the conversion.
    assert_eq!(plan("%-+#012.7Lf"), Err(FormatError::LongDoubleUnsupported('f')));
    // It is refused even when it is not the first conversion, so a format string cannot
    // smuggle one in behind a conversion that plans cleanly.
    assert_eq!(plan("%d then %Lg"), Err(FormatError::LongDoubleUnsupported('g')));
    // And the message names it, because a refusal nobody can act on is not much better
    // than a wrong number.
    assert!(FormatError::LongDoubleUnsupported('f').to_string().contains("%Lf"));
}

/// `L` on an integer conversion is not a `long double` and is not refused: C says the
/// modifier only applies to floating point, and bionic's own `printf` ignores it there.
/// The over-correction — refusing every `L` — would break a legal format string.
#[test]
fn plan_refuses_long_double_only_where_the_argument_really_is_one() {
    assert_eq!(plan("%Ld").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%Lu").unwrap(), vec![ArgKind::UInt]);
    // `q` is BSD's spelling of `ll` — a 64-bit integer, not a quad — so it is not refused.
    assert_eq!(plan("%qd").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%qf").unwrap(), vec![ArgKind::Double]);
}

/// `wchar_t` is **32 bits** on Android, so `%ls`/`%lc` are not their narrow forms with an
/// extra letter: reading one as a byte string prints the first character and then garbage.
#[test]
fn plan_refuses_wide_conversions() {
    assert_eq!(plan("%ls"), Err(FormatError::WideUnsupported('s')));
    assert_eq!(plan("%lc"), Err(FormatError::WideUnsupported('c')));
    // `ll` is not `l`: `%lld` is an ordinary 64-bit integer.
    assert_eq!(plan("%lld").unwrap(), vec![ArgKind::Int]);
    // And `l` on the conversions where it means "64-bit" is fine.
    assert_eq!(plan("%ld").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%lx").unwrap(), vec![ArgKind::UInt]);
    assert_eq!(plan("%lf").unwrap(), vec![ArgKind::Double]);
}

/// Hostile format strings: the guest chooses these bytes, so every one of them has to be a
/// returned error rather than a panic, a hang, or an allocation the length of the width.
#[test]
fn plan_survives_hostile_format_strings() {
    // Truncated specifications.
    assert!(plan("%").is_err());
    assert!(plan("abc%").is_err());
    assert!(plan("%-").is_err());
    assert!(plan("%12").is_err());
    assert!(plan("%.").is_err());
    assert!(plan("%.*").is_err());
    assert!(plan("%l").is_err());
    assert!(plan("%hh").is_err());
    // `%n` is an exploit primitive and is refused by name.
    assert_eq!(plan("%n"), Err(FormatError::NNotSupported));
    assert_eq!(plan("write here: %n"), Err(FormatError::NNotSupported));
    // Unknown conversions.
    assert_eq!(plan("%y"), Err(FormatError::UnknownSpecifier('y')));
    assert_eq!(plan("%\0"), Err(FormatError::UnknownSpecifier('\0')));
    // A width of thirty nines must saturate rather than wrap or panic. The plan itself is
    // fine — it is one integer — and nothing is allocated here.
    assert_eq!(plan(&format!("%{}d", "9".repeat(30))).unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan(&format!("%.{}f", "9".repeat(30))).unwrap(), vec![ArgKind::Double]);
    // A long run of conversions terminates and reports all of them.
    let many = "%d".repeat(4096);
    assert_eq!(plan(&many).unwrap().len(), 4096);
    // Flags with nothing after them, repeated flags, and every flag at once.
    assert!(plan("%-+ #0").is_err());
    assert_eq!(plan("%-+ #0d").unwrap(), vec![ArgKind::Int]);
    assert_eq!(plan("%-----d").unwrap(), vec![ArgKind::Int]);
}

/// A saturated width does not become a 2 GB allocation. The guest picks the width, so the
/// cost of honouring it is the guest's choice unless something bounds it — this records
/// where the bound is **not**: `format` will try to pad, so the adapter caps the output
/// buffer, and this test pins that `plan` itself neither allocates nor refuses.
#[test]
fn a_saturated_width_is_planned_but_not_honoured_here() {
    let f = format!("%{}d", "9".repeat(30));
    assert_eq!(plan(&f).unwrap(), vec![ArgKind::Int]);
    // Deliberately not calling `format` with it: that is the adapter's bound, tested there.
}
