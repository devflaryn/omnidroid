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

/// **Critical, and reachable from a guest format string.** `emit_padded` pads with
/// `repeat_n(' ', width - body.len())`, so a width of thirty nines saturates to `usize::MAX`
/// and the pad becomes an allocation that aborts. An abort cannot be contained by any caller.
///
/// Both forms are tested, because they arrive by different routes: the width written in the
/// format string, and the width taken from a `*` argument, which no pre-scan of the format
/// string can see.
#[test]
fn a_hostile_width_is_refused_rather_than_allocated() {
    let huge = format!("%{}d", "9".repeat(30));
    assert!(matches!(
        format(&huge, &[FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 'd', .. })
    ));
    // The `*` form: the width is an argument, so the format string looks harmless.
    assert!(matches!(
        format("%*d", &[FormatArg::Int(i64::MAX), FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 'd', .. })
    ));
    // A negative `*` width is left-justification with the magnitude, which is the same hazard.
    assert!(matches!(
        format("%*d", &[FormatArg::Int(i64::MIN + 1), FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 'd', .. })
    ));
    // Precision, too: `%.*f` builds that many digits.
    assert!(matches!(
        format("%.*f", &[FormatArg::Int(i64::MAX), FormatArg::Double(1.0)], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 'f', .. })
    ));
    // And nothing was written on the way to the refusal.
    let mut out = String::new();
    let _ = format(&huge, &[FormatArg::Int(1)], &mut out);
    assert_eq!(out, "");
}

/// The over-correction: a width a real program uses must still work, and the cap must be
/// exactly where it says it is. A bound that refused `%80s` would break a correct caller.
#[test]
fn the_width_cap_admits_every_width_below_it() {
    use omni_bionic::printf::MAX_FIELD_WIDTH;
    assert_eq!(fmt("%8d", &[FormatArg::Int(42)]).len(), 8);
    assert_eq!(fmt("%80s", &[FormatArg::Str("x")]).len(), 80);
    let at_limit = format!("%{MAX_FIELD_WIDTH}d");
    assert_eq!(fmt(&at_limit, &[FormatArg::Int(1)]).len(), MAX_FIELD_WIDTH);
    let past_limit = format!("%{}d", MAX_FIELD_WIDTH + 1);
    assert!(matches!(
        format(&past_limit, &[FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::FieldTooWide { .. })
    ));
}

/// Capping one field is not enough on its own: a format string may repeat a wide conversion
/// until the total is a gigabyte. The total cap stops that, and stops it *before* the
/// allocation rather than after it.
#[test]
fn a_repeated_wide_field_is_stopped_by_the_total_cap() {
    use omni_bionic::printf::{MAX_FIELD_WIDTH, MAX_OUTPUT};
    let wide = format!("%{}d", MAX_FIELD_WIDTH);
    let repeated = wide.repeat(64); // 64 x 64 KiB = 4 MiB, past the 1 MiB total
    let args: Vec<FormatArg> = (0..64).map(|_| FormatArg::Int(1)).collect();
    let mut out = String::new();
    assert!(matches!(
        format(&repeated, &args, &mut out),
        Err(FormatError::OutputTooLarge { .. })
    ));
    // It stopped near the cap rather than after building the whole 4 MiB.
    assert!(
        out.len() <= MAX_OUTPUT + MAX_FIELD_WIDTH,
        "output grew to {} before stopping, past the {} + {} bound",
        out.len(),
        MAX_OUTPUT,
        MAX_FIELD_WIDTH
    );
    // A long run of plain literal bytes is bounded by the same check.
    let literals = "x".repeat(MAX_OUTPUT + 16);
    assert!(matches!(
        format(&literals, &[], &mut String::new()),
        Err(FormatError::OutputTooLarge { .. })
    ));
}

// ---------------------------------------------------------------------------------------------
// The budget: what a `vsnprintf` into a fixed buffer keeps (adapter-review finding W1)
// ---------------------------------------------------------------------------------------------
//
// Oracle, as everywhere else in this file: the conversion rules, hand-derived. The device's
// arrangement is `char buf[LOG_BUF_SIZE]` with `LOG_BUF_SIZE` 1024, so 1,023 characters and a
// NUL -- `omni_platform::log` carries that constant and names the AOSP file it came out of, and
// it is written out here rather than imported because `omni-bionic` has no dependencies (D19).

/// `LOG_BUF_SIZE - 1`: the characters `liblog`'s `vsnprintf` keeps. See above.
const LIBLOG_BUDGET: usize = 1023;

/// Format under a budget, failing loudly rather than skipping if it refuses.
///
/// `VERIFICATION.md` entry 4: a test that takes an early exit on an unexpected outcome passes
/// without asserting anything. Every call here is a claim that this input **does not refuse**.
fn bounded(
    fmt_str: &str,
    args: &[FormatArg],
    budget: usize,
) -> (String, omni_bionic::printf::Produced) {
    let mut out = String::new();
    match omni_bionic::printf::format_bounded(fmt_str, args, &mut out, budget) {
        Ok(produced) => (out, produced),
        Err(error) => panic!(
            "`{}` at budget {budget} must truncate, not refuse: {error}",
            &fmt_str[..fmt_str.len().min(40)]
        ),
    }
}

/// **The trap, and the reason this arm needed thought rather than a `match`.**
///
/// A right-justified field puts its padding *first*, so a device's buffer fills with padding and
/// the number never reaches it: `vsnprintf(buf, 1024, "%70000d", 42)` leaves 1,023 **spaces**.
/// The believable wrong answer is to clamp the width to the budget, which produces 1,021 spaces
/// followed by `42` -- the right length, the right characters, in the wrong order, and a tail a
/// device does not have. Asserted on the bytes, not on the length, because the two differ only
/// in the last two of 1,023.
#[test]
fn a_wide_right_justified_field_is_cut_to_its_padding_and_never_to_its_number() {
    let (out, produced) = bounded("%70000d", &[FormatArg::Int(42)], LIBLOG_BUDGET);
    assert_eq!(out, " ".repeat(LIBLOG_BUDGET), "a device's buffer is all padding");
    assert!(!out.ends_with("42"), "the number belongs 68,977 characters past the buffer");
    // And the clamped-width answer is named, so a future change to it fails here rather than
    // passing a length check.
    let clamped = format!("{:>width$}", 42, width = LIBLOG_BUDGET);
    assert_ne!(out, clamped, "clamping the width to the budget is the wrong answer");
    // `full` is what `vsnprintf` returns: the whole field, not what was kept.
    assert_eq!(produced.full, 70_000);
    assert_eq!(produced.kept, LIBLOG_BUDGET);
    assert!(produced.truncated());
}

/// The other two padding modes, where the payload **is** in the prefix and must survive.
///
/// The companion to the test above: a fix that answered "a wide field is all padding" would be
/// right for one of the three modes and wrong for the other two.
#[test]
fn a_wide_left_justified_or_zero_padded_field_keeps_what_comes_first() {
    // `-`: the body leads, then spaces.
    let (left, _) = bounded("%-70000d", &[FormatArg::Int(42)], LIBLOG_BUDGET);
    assert_eq!(left, format!("42{}", " ".repeat(LIBLOG_BUDGET - 2)));

    // `0`: the sign leads, then zeros -- the sign is not overwritten by the padding.
    let (zero, _) = bounded("%070000d", &[FormatArg::Int(-42)], LIBLOG_BUDGET);
    assert_eq!(zero, format!("-{}", "0".repeat(LIBLOG_BUDGET - 1)));
    assert!(zero.starts_with('-'), "a lost sign is a different number");

    // `#0x`: the `0x` leads for the same reason.
    let (hex, _) = bounded("%#070000x", &[FormatArg::UInt(0xab)], LIBLOG_BUDGET);
    assert_eq!(hex, format!("0x{}", "0".repeat(LIBLOG_BUDGET - 2)));
}

/// **The budgeted output is a prefix of the unbounded output**, at every budget, for every
/// padding mode and both `*` forms.
///
/// The general statement of what a fixed buffer does, asserted against this engine's own
/// unbounded result rather than against a second implementation (`VERIFICATION.md` entry 7).
/// The widths stay under `MAX_FIELD_WIDTH` so that the unbounded half is producible at all --
/// which is exactly the case where the fix must change nothing.
#[test]
fn a_budgeted_result_is_a_character_prefix_of_the_unbounded_one() {
    let cases: &[(&str, &[FormatArg])] = &[
        ("[%4096d]", &[FormatArg::Int(-7)]),
        ("[%-4096d]", &[FormatArg::Int(-7)]),
        ("[%04096d]", &[FormatArg::Int(-7)]),
        ("[%+4096.100d]", &[FormatArg::Int(7)]),
        ("[%#4096.80x]", &[FormatArg::UInt(0xdead_beef)]),
        ("[%4096s]", &[FormatArg::Str("payload")]),
        ("[%-4096s]", &[FormatArg::Str("payload")]),
        ("[%4096.3s]", &[FormatArg::Str("payload")]),
        ("[%*d]", &[FormatArg::Int(4096), FormatArg::Int(-7)]),
        ("[%.*f]", &[FormatArg::Int(900), FormatArg::Double(0.5)]),
        ("[%4096c]", &[FormatArg::Int(0x41)]),
        ("[%4096p]", &[FormatArg::Ptr(0x1000)]),
        ("literal %s and %d over and over", &[FormatArg::Str("s"), FormatArg::Int(1)]),
    ];
    for (spec, args) in cases {
        let whole: Vec<char> = fmt(spec, args).chars().collect();
        for budget in [0usize, 1, 2, 7, 100, 1023, 4095, 4096, 4097, whole.len(), 1 << 20] {
            let (out, produced) = bounded(spec, args, budget);
            let expected: String = whole.iter().take(budget).collect();
            assert_eq!(out, expected, "`{spec}` at budget {budget}");
            assert_eq!(produced.full, whole.len(), "`{spec}` full length at budget {budget}");
            assert_eq!(produced.kept, expected.chars().count(), "`{spec}` kept at {budget}");
            assert_eq!(
                produced.truncated(),
                whole.len() > budget,
                "`{spec}` at budget {budget} must report whether it lost anything"
            );
        }
    }
}

/// **The case finding W1 names: a format string that *builds* a megabyte.**
///
/// Unbounded this is `OutputTooLarge` and the partial result is discarded, which aborts the
/// guest run. Under a budget it is what a device keeps. The input is genuinely constructed --
/// sixty-four 64 KiB fields, 4 MiB in total -- rather than asserted about in the abstract.
#[test]
fn a_repeated_wide_field_truncates_under_a_budget_where_it_refuses_without_one() {
    use omni_bionic::printf::{MAX_FIELD_WIDTH, MAX_OUTPUT};
    let wide = format!("%{MAX_FIELD_WIDTH}d");
    let repeated = wide.repeat(64);
    let args: Vec<FormatArg> = (0..64).map(|_| FormatArg::Int(1)).collect();

    // Unbounded: unchanged, and the refusal is still the right answer there.
    assert!(matches!(
        format(&repeated, &args, &mut String::new()),
        Err(FormatError::OutputTooLarge { .. })
    ));

    let (out, produced) = bounded(&repeated, &args, LIBLOG_BUDGET);
    assert_eq!(out, " ".repeat(LIBLOG_BUDGET), "the first field's padding, and nothing else");
    assert_eq!(produced.full, 64 * MAX_FIELD_WIDTH, "4 MiB is what it asked for");
    assert!(produced.full > MAX_OUTPUT, "the input really does pass the unbounded cap");
    assert!(produced.truncated());
}

/// A run of literal bytes is bounded by the same budget, and reports its true length.
#[test]
fn a_long_literal_run_truncates_under_a_budget() {
    use omni_bionic::printf::MAX_OUTPUT;
    let literals = "x".repeat(MAX_OUTPUT + 16);
    assert!(matches!(
        format(&literals, &[], &mut String::new()),
        Err(FormatError::OutputTooLarge { .. })
    ));
    let (out, produced) = bounded(&literals, &[], LIBLOG_BUDGET);
    assert_eq!(out, "x".repeat(LIBLOG_BUDGET));
    assert_eq!(produced.full, MAX_OUTPUT + 16);
}

/// **A guest-chosen precision is a counted run, not an allocation**, for the integer
/// conversions.
///
/// `%.70000d` is a minimum digit count: sign, then 69,999 zeros, then the digit. A device's
/// buffer therefore holds 1,023 zeros and no digit at all.
#[test]
fn a_huge_integer_precision_is_a_fill_and_the_digits_sit_past_the_budget() {
    let (out, produced) = bounded("%.70000d", &[FormatArg::Int(7)], LIBLOG_BUDGET);
    assert_eq!(out, "0".repeat(LIBLOG_BUDGET));
    assert_eq!(produced.full, 70_000, "69,999 zeros and one digit");

    // With a sign the sign leads, because the zeros are the *precision* and go after it.
    let (signed, _) = bounded("%.70000d", &[FormatArg::Int(-7)], LIBLOG_BUDGET);
    assert_eq!(signed, format!("-{}", "0".repeat(LIBLOG_BUDGET - 1)));

    // `%#.70000x`: the `0x` leads, and for `%#o` the alternate-form prefix is dropped where the
    // precision's own leading zero already supplies one.
    let (hex, hex_produced) = bounded("%#.70000x", &[FormatArg::UInt(0xab)], LIBLOG_BUDGET);
    assert_eq!(hex, format!("0x{}", "0".repeat(LIBLOG_BUDGET - 2)));
    assert_eq!(hex_produced.full, 70_002);
    let (oct, _) = bounded("%#.70000o", &[FormatArg::UInt(0o17)], LIBLOG_BUDGET);
    assert_eq!(oct, "0".repeat(LIBLOG_BUDGET), "no second `0` prefix: the precision supplied one");
}

/// **`%.*f` past `EXACT_FRACTION_DIGITS` is zeros, and splitting there changes no byte.**
///
/// The smallest positive `f64` is `2^-1074`, so a finite `double`'s exact decimal expansion has
/// at most 1,074 fraction digits and a precision past that appends literal zeros and rounds
/// nothing. That is what lets `%.70000f` be answered without building 70,000 digits.
///
/// Checked two ways: against the hand-derived bytes, and against this engine's **own** output at
/// a precision below the split, which is the half that does not take the fill path at all. If
/// the split rounded, those two would differ.
#[test]
fn a_huge_f_precision_is_a_fill_and_the_bytes_are_the_whole_conversions() {
    use omni_bionic::printf::EXACT_FRACTION_DIGITS;
    assert_eq!(EXACT_FRACTION_DIGITS, 1074, "2^-1074 is the smallest positive double");

    // 0.5 is exact: "0.5" then 69,998 zeros.
    let (out, produced) = bounded("%.70000f", &[FormatArg::Double(0.5)], LIBLOG_BUDGET);
    assert_eq!(out, format!("0.5{}", "0".repeat(LIBLOG_BUDGET - 3)));
    assert_eq!(produced.full, 70_002, "`0.` and 70,000 fraction digits");

    // 0.1 is not exact: its expansion is 55 digits long and the rest are zeros. The first 900
    // characters must be the same whether the conversion was asked for 1,000 of them (no fill)
    // or for 70,000 (fill).
    let below_the_split: String =
        fmt("%.1000f", &[FormatArg::Double(0.1)]).chars().take(900).collect();
    let (above_the_split, _) = bounded("%.70000f", &[FormatArg::Double(0.1)], 900);
    assert_eq!(above_the_split, below_the_split, "the split must not round");
    assert!(
        above_the_split.starts_with("0.1000000000000000055511151231257827"),
        "{}",
        &above_the_split[..40]
    );
    assert!(above_the_split.ends_with("000"), "and the tail is zeros");

    // A negative value with a width: width 70,000 over a body of 70,003 is no padding at all,
    // so the sign and the digits lead.
    let (wide, wide_produced) = bounded("%70000.70000f", &[FormatArg::Double(-0.5)], LIBLOG_BUDGET);
    assert_eq!(wide, format!("-0.5{}", "0".repeat(LIBLOG_BUDGET - 4)));
    assert_eq!(wide_produced.full, 70_003);

    // `inf` has no fraction digits at any precision, so there is no fill to run away with.
    let (infinite, inf_produced) =
        bounded("%.70000f", &[FormatArg::Double(f64::INFINITY)], LIBLOG_BUDGET);
    assert_eq!(infinite, "inf");
    assert_eq!(inf_produced.full, 3);
    assert!(!inf_produced.truncated());
}

/// **The arm that could not be made byte-correct, refused by name rather than guessed.**
///
/// `e E g G a A` build their digits through `10u64.pow(precision.min(15))` and through a
/// precision `format_g` derives, so past fifteen places this engine's digits are not
/// `vsnprintf`'s and a partial result would be a plausible wrong answer in the *visible* prefix.
/// A budget does not change that, so these stay refusals -- and the refusal names the conversion.
///
/// The negative half is the point: the conversions that **can** be placed must not be swept up
/// with them, or the fix would be a refusal with extra steps.
#[test]
fn the_floating_conversions_this_engine_cannot_place_still_refuse_under_a_budget() {
    for conversion in ['e', 'E', 'g', 'G', 'a', 'A'] {
        let spec = format!("%.70000{conversion}");
        let mut out = String::new();
        let outcome = omni_bionic::printf::format_bounded(
            &spec,
            &[FormatArg::Double(1.5)],
            &mut out,
            LIBLOG_BUDGET,
        );
        assert!(
            matches!(
                outcome,
                Err(FormatError::FieldTooWide { conversion: c, .. }) if c == conversion
            ),
            "`{spec}` must refuse and name its conversion, got {outcome:?}"
        );
        assert_eq!(out, "", "and nothing plausible-but-wrong was written on the way");
    }
    // Their *width* is a different question and is honoured: the padding is placeable.
    let (wide, wide_produced) = bounded("%70000e", &[FormatArg::Double(1.5)], LIBLOG_BUDGET);
    assert_eq!(wide, " ".repeat(LIBLOG_BUDGET));
    assert_eq!(wide_produced.full, 70_000);
    // And the conversions that can be placed are not refused with them.
    for spec in ["%.70000d", "%.70000u", "%.70000x", "%.70000f", "%.70000s"] {
        let args: Vec<FormatArg> = if spec.ends_with('s') {
            vec![FormatArg::Str("short")]
        } else if spec.ends_with('f') {
            vec![FormatArg::Double(0.5)]
        } else {
            vec![FormatArg::Int(1)]
        };
        let (_, produced) = bounded(spec, &args, LIBLOG_BUDGET);
        assert!(produced.full > 0, "`{spec}` produced nothing at all");
    }
}

/// A `%s` precision can only ever **shorten** a string, so a huge one costs nothing and a
/// device simply prints the string.
#[test]
fn a_huge_string_precision_prints_the_whole_string_rather_than_refusing() {
    let (out, produced) = bounded("%.70000s", &[FormatArg::Str("payload")], LIBLOG_BUDGET);
    assert_eq!(out, "payload");
    assert_eq!(produced.full, 7);
    assert!(!produced.truncated());
    // Unbounded, the cap is still what stops a hostile count reaching `emit_padded`.
    assert!(matches!(
        format("%.70000s", &[FormatArg::Str("payload")], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 's', .. })
    ));
}

/// **A `%s` precision that lands inside a character used to panic.**
///
/// A `%s` argument is one `char` per guest byte, so a guest byte above `0x7F` is two bytes of
/// the host `String`. The precision sliced at a byte index taken straight from the guest, which
/// both counted the wrong unit and panicked on a boundary the guest picked -- a panic unwinding
/// out of an import, which is the failure this layer exists to never produce. The input here is
/// the two guest bytes `0xC3 0xA9`, which is what `"e-acute"` arrives as.
#[test]
fn a_string_precision_counts_guest_bytes_and_cannot_split_a_character() {
    let guest = "\u{c3}\u{a9}"; // two guest bytes, four host bytes
    assert_eq!(guest.len(), 4, "the host String really is twice the guest's length");
    assert_eq!(guest.chars().count(), 2);
    assert_eq!(fmt("%.1s", &[FormatArg::Str(guest)]), "\u{c3}", "one guest byte, not half of one");
    assert_eq!(fmt("%.2s", &[FormatArg::Str(guest)]), guest);
    assert_eq!(fmt("%.9s", &[FormatArg::Str(guest)]), guest, "a precision past the end is a no-op");
    assert_eq!(fmt("%.0s", &[FormatArg::Str(guest)]), "");
    // And the width is measured in the same unit, or a two-byte guest character would be padded
    // as though it were two characters.
    assert_eq!(fmt("%4s", &[FormatArg::Str(guest)]), format!("  {guest}"));
}

/// **The budget is guest bytes, which is not host bytes**, and the difference is a factor of two
/// on exactly the input a hostile guest picks.
///
/// A budget applied to `out.len()` would keep 511 guest bytes where a device keeps 1,023.
#[test]
fn the_budget_counts_guest_bytes_and_not_host_string_bytes() {
    let high: String = core::iter::repeat_n('\u{ff}', 4000).collect();
    let (out, produced) = bounded("%s", &[FormatArg::Str(&high)], LIBLOG_BUDGET);
    assert_eq!(out.chars().count(), LIBLOG_BUDGET, "1,023 guest bytes");
    assert_eq!(out.len(), 2 * LIBLOG_BUDGET, "which is 2,046 host bytes");
    assert!(out.chars().all(|c| c == '\u{ff}'));
    assert_eq!(produced.full, 4000);
    assert_eq!(produced.kept, LIBLOG_BUDGET);
}

/// A message that fits is untouched, and says so -- the negative half of every assertion above.
///
/// `VERIFICATION.md` entry 11: a flag that is set under the fault and also set without it
/// reports nothing. So this asserts `truncated()` is **false** at the exact boundary and true
/// one character past it.
#[test]
fn a_message_that_fits_the_budget_is_byte_identical_and_reports_no_truncation() {
    let exact = "y".repeat(LIBLOG_BUDGET);
    let (out, produced) = bounded("%s", &[FormatArg::Str(&exact)], LIBLOG_BUDGET);
    assert_eq!(out, exact);
    assert_eq!(produced.kept, LIBLOG_BUDGET);
    assert_eq!(produced.full, LIBLOG_BUDGET);
    assert!(!produced.truncated(), "exactly at the budget is not a truncation");

    let past = "y".repeat(LIBLOG_BUDGET + 1);
    let (out, produced) = bounded("%s", &[FormatArg::Str(&past)], LIBLOG_BUDGET);
    assert_eq!(out, exact, "one character past it loses exactly one character");
    assert_eq!(produced.full, LIBLOG_BUDGET + 1);
    assert!(produced.truncated());

    // An ordinary line, which is every line the engine actually logs.
    let (out, produced) =
        bounded("hello %s #%d", &[FormatArg::Str("world"), FormatArg::Int(7)], LIBLOG_BUDGET);
    assert_eq!(out, "hello world #7");
    assert_eq!(produced.full, 14);
    assert!(!produced.truncated());
}

/// The unbounded entry point is unchanged, character for character, and `usize::MAX` is the
/// same call.
#[test]
fn an_unbounded_budget_is_the_unbounded_call() {
    let cases: &[(&str, &[FormatArg])] = &[
        (
            "%d %i %u %o %x %X",
            &[
                FormatArg::Int(-1),
                FormatArg::Int(2),
                FormatArg::UInt(3),
                FormatArg::UInt(8),
                FormatArg::UInt(255),
                FormatArg::UInt(255),
            ],
        ),
        ("%c%s%p", &[FormatArg::Int(0x41), FormatArg::Str("bc"), FormatArg::Ptr(0)]),
        (
            "%e %f %g %a",
            &[
                FormatArg::Double(1.5),
                FormatArg::Double(1.5),
                FormatArg::Double(1.5),
                FormatArg::Double(1.5),
            ],
        ),
        (
            "%08.3f|%-8d|%+d|% d|%#x",
            &[
                FormatArg::Double(-1.5),
                FormatArg::Int(7),
                FormatArg::Int(7),
                FormatArg::Int(7),
                FormatArg::UInt(0x2a),
            ],
        ),
    ];
    for (spec, args) in cases {
        let (out, produced) = bounded(spec, args, usize::MAX);
        assert_eq!(out, fmt(spec, args), "`{spec}`");
        assert!(!produced.truncated());
        assert_eq!(produced.kept, produced.full);
    }
}

/// A width past the cap **still refuses without a budget**, which is the half of the cap that is
/// still doing work: with nowhere for the output to stop, `emit_padded` would be asked for an
/// allocation the guest picked.
#[test]
fn a_hostile_width_without_a_budget_is_still_refused() {
    use omni_bionic::printf::MAX_FIELD_WIDTH;
    for spec in ["%70000d", "%-70000d", "%070000d"] {
        assert!(
            matches!(
                format(spec, &[FormatArg::Int(1)], &mut String::new()),
                Err(FormatError::FieldTooWide { requested: 70_000, limit: MAX_FIELD_WIDTH, .. })
            ),
            "`{spec}` unbounded"
        );
    }
    // The `*` form too: the format string alone looks harmless.
    assert!(matches!(
        format("%*d", &[FormatArg::Int(i64::MAX), FormatArg::Int(1)], &mut String::new()),
        Err(FormatError::FieldTooWide { conversion: 'd', .. })
    ));
    // And under a budget the same `*` width is honoured, because the padding is counted.
    let (out, produced) =
        bounded("%*d", &[FormatArg::Int(70_000), FormatArg::Int(1)], LIBLOG_BUDGET);
    assert_eq!(out, " ".repeat(LIBLOG_BUDGET));
    assert_eq!(produced.full, 70_000);
}

/// A budget of zero writes nothing and still reports the whole length -- `snprintf(NULL, 0, ...)`
/// is the documented way to ask how long a result would be.
#[test]
fn a_zero_budget_writes_nothing_and_still_counts() {
    let (out, produced) = bounded("%s and %d", &[FormatArg::Str("abc"), FormatArg::Int(10)], 0);
    assert_eq!(out, "");
    assert_eq!(produced.kept, 0);
    assert_eq!(produced.full, 10, "abc and 10");
    assert!(produced.truncated());
}
