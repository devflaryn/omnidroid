//! `omni_bionic::scanf`: bionic's `vfscanf` rules, case by case.

use omni_bionic::scanf::{scan, Store, Unsupported};

fn int(value: u64, size: usize) -> Store {
    Store::Int { value, size }
}

fn text(bytes: &[u8]) -> Store {
    Store::Str(bytes.to_vec())
}

/// **The engine's own call, as the renderer makes it** (MEASURED: `libroblox.so` `0x2f7c5b`), on
/// the query it meets first: `id=` is empty, so the scanset matches and `%lld` meets `&` -- a
/// matching failure. One assignment, and no store for `id`, `w` or `h`.
#[test]
fn the_renderers_thumbnail_query_with_no_id_assigns_only_its_type() {
    let scanned = scan(
        b"type=AvatarHeadShot&id=&w=48&h=48&filters=circular&includebackground=true",
        b"type=%[^&]&id=%lld&w=%d&h=%d",
    )
    .expect("every conversion is implemented");
    assert_eq!(scanned.result, 1);
    assert_eq!(scanned.stores, vec![text(b"AvatarHeadShot")]);
}

/// The same format with an `id`: four assignments, `%lld` in eight bytes and `%d` in four.
#[test]
fn the_thumbnail_query_with_an_id_assigns_all_four_in_their_widths() {
    let scanned = scan(b"type=Asset&id=5000000000&w=420&h=36", b"type=%[^&]&id=%lld&w=%d&h=%d")
        .expect("implemented");
    assert_eq!(scanned.result, 4);
    assert_eq!(
        scanned.stores,
        vec![text(b"Asset"), int(5_000_000_000, 8), int(420, 4), int(36, 4)]
    );
}

/// `EOF` is an input failure **before the first conversion**; after one, it is the count.
#[test]
fn input_failure_is_eof_only_before_the_first_conversion() {
    assert_eq!(scan(b"", b"%d").expect("ok").result, -1, "nothing to read");
    assert_eq!(scan(b"   ", b"%d").expect("ok").result, -1, "input ends while skipping space");
    assert_eq!(scan(b"", b"x%d").expect("ok").result, -1, "input ends at a literal");
    assert_eq!(scan(b"12", b"%d%d").expect("ok").result, 1, "ends after one conversion");
    // A matching failure is never EOF.
    assert_eq!(scan(b"abc", b"%d").expect("ok").result, 0);
    assert_eq!(scan(b"w=48", b"h=%d").expect("ok").result, 0, "a literal that does not match");
    // A format that ends inside a specification is EOF, as bionic's `case '\0'` returns.
    assert_eq!(scan(b"12", b"%l").expect("ok").result, -1);
}

/// `%i` takes its base from the prefix; `%x` accepts `0x` and gives back the `x` of a bare one.
#[test]
fn integer_prefixes_follow_vfscanfs_state_machine() {
    let scanned = scan(b"0x1f 017 -9 08", b"%i %i %i %i").expect("ok");
    assert_eq!(scanned.result, 4);
    assert_eq!(
        scanned.stores,
        vec![int(31, 4), int(15, 4), int((-9i64) as u64, 4), int(0, 4)],
        "`08` under %i is octal 0, stopping at the 8"
    );
    // `0x` with no hex digit after it: the field is `0`, and the `x` is the next input.
    let scanned = scan(b"0xg", b"%x%c").expect("ok");
    assert_eq!(scanned.stores, vec![int(0, 4), Store::Chars(b"x".to_vec())]);
    // `%d` does not take a prefix at all.
    let scanned = scan(b"0x10", b"%d%s").expect("ok");
    assert_eq!(scanned.stores, vec![int(0, 4), text(b"x10")]);
}

/// A field width counts every byte of the field, sign and prefix included.
#[test]
fn a_width_bounds_the_field() {
    let scanned = scan(b"12345", b"%2d%3d").expect("ok");
    assert_eq!(scanned.stores, vec![int(12, 4), int(345, 4)]);
    let scanned = scan(b"-123", b"%2d%d").expect("ok");
    assert_eq!(scanned.stores, vec![int((-1i64) as u64, 4), int(23, 4)]);
    let scanned = scan(b"abcdef", b"%3s%s").expect("ok");
    assert_eq!(scanned.stores, vec![text(b"abc"), text(b"def")]);
}

/// `*` converts without assigning; `%n` stores the bytes consumed and is never counted.
#[test]
fn suppression_and_n_are_stored_or_counted_as_bionic_does() {
    let scanned = scan(b"1 2 3", b"%*d %d %n").expect("ok");
    assert_eq!(scanned.result, 1, "neither the suppressed %d nor %n is an assignment");
    assert_eq!(scanned.stores, vec![int(2, 4), int(4, 4)], "%n: four bytes consumed");
    let scanned = scan(b"ab", b"%hhn%c").expect("ok");
    assert_eq!(scanned.stores, vec![int(0, 1), Store::Chars(b"a".to_vec())], "%hhn is one byte");
}

/// `%[`: a leading `]` is a member, `a-c` is a range, a trailing `-` is itself, `^` negates, and
/// a set that matches nothing is a matching failure. It skips no whitespace.
#[test]
fn a_scanset_is_parsed_as_sccl_parses_it() {
    assert_eq!(scan(b"abc]def", b"%[]a-c]").expect("ok").stores, vec![text(b"abc]")]);
    assert_eq!(scan(b"a-b-c+", b"%[abc-]").expect("ok").stores, vec![text(b"a-b-c")]);
    assert_eq!(scan(b"x&y", b"%[^&]").expect("ok").stores, vec![text(b"x")]);
    assert_eq!(scan(b" x", b"%[x]").expect("ok").result, 0, "no skip, so the space fails");
    assert_eq!(scan(b"&y", b"%[^&]").expect("ok").result, 0, "empty is a matching failure");
}

/// `strtoimax` clamps, `strtoumax` saturates and wraps a `-`, and the store keeps the low bytes.
#[test]
fn overflow_and_sign_are_strtoimaxs_and_strtoumaxs() {
    let huge = b"99999999999999999999";
    assert_eq!(scan(huge, b"%lld").expect("ok").stores, vec![int(i64::MAX as u64, 8)]);
    assert_eq!(scan(b"-99999999999999999999", b"%lld").expect("ok").stores, vec![int(i64::MIN as u64, 8)]);
    assert_eq!(scan(huge, b"%llu").expect("ok").stores, vec![int(u64::MAX, 8)]);
    assert_eq!(scan(b"-1", b"%u").expect("ok").stores, vec![int(u64::MAX, 4)], "wrapped");
    assert_eq!(scan(b"300", b"%hhd").expect("ok").stores, vec![int(300, 1)], "the low byte is kept");
    assert_eq!(scan(b"0x7f", b"%p").expect("ok").stores, vec![int(0x7f, 8)], "a pointer is 8");
}

/// `%c` does not skip space, defaults to one byte, stores no NUL, and keeps a short read.
#[test]
fn c_reads_exactly_what_is_there() {
    assert_eq!(scan(b" x", b"%c").expect("ok").stores, vec![Store::Chars(b" ".to_vec())]);
    let scanned = scan(b"ab", b"%5c").expect("ok");
    assert_eq!((scanned.result, scanned.stores), (1, vec![Store::Chars(b"ab".to_vec())]));
}

/// `%%` matches `%` with no whitespace skipped first, as bionic (OpenBSD) does.
#[test]
fn percent_percent_is_a_literal_that_skips_nothing() {
    assert_eq!(scan(b"%5", b"%%%d").expect("ok").stores, vec![int(5, 4)]);
    assert_eq!(scan(b" %5", b"%%%d").expect("ok").result, 0);
}

/// What no run has reached is refused by name, before anything is stored.
#[test]
fn floats_wide_forms_and_unknown_conversions_are_refused_by_name() {
    for (format, conversion) in [
        (&b"%d %f"[..], "%f"),
        (b"%lf", "%lf"),
        (b"%ls", "%ls"),
        (b"%lc", "%lc"),
        (b"%l[a]", "%l["),
        (b"%y", "%y"),
    ] {
        let Err(Unsupported { conversion: named, .. }) = scan(b"1 2.5", format) else {
            panic!("`{}` must be refused", String::from_utf8_lossy(format));
        };
        assert_eq!(named, conversion);
    }
}
