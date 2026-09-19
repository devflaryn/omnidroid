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

use omni_bionic::printf::{format, FormatArg, FormatError};

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
