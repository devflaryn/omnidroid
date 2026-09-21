//! The `fopen`/`fdopen` mode string, case by case.
//!
//! **This file exists because of review finding M5**: a mode string over sixteen bytes was
//! *silently truncated* where the code's own documentation said `EINVAL`, so a long mode lost its
//! trailing `+` and a caller that asked for a read-write stream got a read-only one with nothing
//! anywhere reporting it. The fix removed the bound rather than raising it — see
//! [`omni_bionic::stdio::parse_mode`] for why — and these are the assertions that would have
//! caught it, plus one per mode the review asked to be walked through explicitly.
//!
//! Every expectation below is derived from **C17 7.21.5.3p3** (the list of mode strings, the `x`
//! spellings, and the footnote permitting an implementation to ignore trailing characters) and
//! from **POSIX.1-2017** `fopen` (`b` has no effect) and `open` (`O_EXCL` without `O_CREAT` is
//! undefined). Nothing here is checked against another implementation's output:
//! `docs/VERIFICATION.md` entry 7 is the record of what that costs.

use omni_bionic::errno::consts;
use omni_bionic::stdio::{parse_mode, ModeRefusal, OpenMode};

/// `(read, write, create, truncate, append, exclusive, close_on_exec)` for a mode that parses.
fn shape(mode: &[u8]) -> (bool, bool, bool, bool, bool, bool, bool) {
    let m = parse_mode(mode).unwrap_or_else(|why| {
        panic!("`{}` was refused: {why}", String::from_utf8_lossy(mode));
    });
    (m.read, m.write, m.create, m.truncate, m.append, m.exclusive, m.close_on_exec)
}

/// The six modes C17 7.21.5.3p3 defines, each asserted field by field.
///
/// **Field by field and not against a reference value**: the whole point of the finding is that a
/// stream's *access* can silently differ from the mode that asked for it, and `read`/`write` are
/// the two fields that carry it.
#[test]
fn the_six_modes_c_defines_grant_exactly_the_access_they_name() {
    //                        read  write create trunc append excl  cloexec
    assert_eq!(shape(b"r"), (true, false, false, false, false, false, false), "r");
    assert_eq!(shape(b"r+"), (true, true, false, false, false, false, false), "r+");
    assert_eq!(shape(b"w"), (false, true, true, true, false, false, false), "w");
    assert_eq!(shape(b"w+"), (true, true, true, true, false, false, false), "w+");
    assert_eq!(shape(b"a"), (false, true, true, false, true, false, false), "a");
    assert_eq!(shape(b"a+"), (true, true, true, false, true, false, false), "a+");
}

/// **The finding, by its own name.**
///
/// `"rbbbbbbbbbbbbbb+"` is the string the adapter review named. Two things are asserted about it,
/// and the second is the one that fails against the defect:
///
/// 1. It is **sixteen** bytes — exactly the old `MAX_MODE_BYTES`. `take(16)` keeps all sixteen,
///    so the review's named witness is one byte short of the truncation it describes; the shape
///    is real and begins at **seventeen**. Recorded here rather than in prose because
///    `docs/VERIFICATION.md` entry 10 is about evidence that has been through one restatement.
/// 2. It asks for a **read-write** stream and must get one, at sixteen bytes and at seventeen and
///    at sixty-four kibibytes.
#[test]
fn rbbbbbbbbbbbbbb_plus_is_a_read_write_stream_at_every_length() {
    let witness = b"rbbbbbbbbbbbbbb+";
    assert_eq!(witness.len(), 16, "the review's named witness is not sixteen bytes");
    let parsed = parse_mode(witness).expect("the review's own mode string");
    assert!(parsed.read && parsed.write, "`rbbbbbbbbbbbbbb+` is not read-write");

    // Seventeen bytes: one `b` more, which is where the old sixteen-byte `take` began to bite.
    let seventeen = b"rbbbbbbbbbbbbbbb+";
    assert_eq!(seventeen.len(), 17);
    let parsed = parse_mode(seventeen).expect("a seventeen-byte mode");
    assert!(parsed.read && parsed.write, "a seventeen-byte mode lost its `+`");

    // And the hostile length. 64 KiB is `GuestMem::STRING_LIMIT`, the longest mode string the
    // adapter can hand this function at all.
    let mut huge = vec![b'r'];
    huge.extend(std::iter::repeat_n(b'b', 64 * 1024 - 2));
    huge.push(b'+');
    assert_eq!(huge.len(), 64 * 1024);
    let parsed = parse_mode(&huge).expect("a 64 KiB mode");
    assert!(parsed.read && parsed.write, "a 64 KiB mode lost its `+`");
}

/// **The defect made observable**: truncating first is what turns a read-write mode into a
/// read-only one, and the parse itself is what must not do it.
///
/// This asserts the *difference* rather than only the fixed answer. A test that checked the long
/// mode is read-write could pass against a parse that happened to stop somewhere harmless; this
/// one names the wrong answer the old code produced, so it cannot be satisfied by accident.
#[test]
fn shortening_a_mode_before_parsing_it_is_what_loses_the_plus() {
    let asked = b"rbbbbbbbbbbbbbbb+"; // seventeen bytes
    let whole = parse_mode(asked).expect("the whole mode");
    let shortened = parse_mode(&asked[..16]).expect("the mode the old sixteen-byte cap kept");

    assert!(whole.write, "the whole mode asks for write access");
    assert!(
        !shortened.write,
        "a mode cut at sixteen bytes must be read-only -- if it is not, this test has stopped \
         demonstrating the defect and the numbers above need re-deriving"
    );
    assert_ne!(whole, shortened, "the cut changed the access, which is the finding");
}

/// `b` and `e` change nothing a guest can observe, and `x` is `O_EXCL`.
#[test]
fn the_modifiers_do_what_their_standards_say_and_nothing_else() {
    // POSIX.1-2017 `fopen`: "the character `b` shall have no effect".
    assert_eq!(parse_mode(b"rb"), parse_mode(b"r"), "`b` changed something");
    assert_eq!(parse_mode(b"wb+"), parse_mode(b"w+"), "`b` changed something");
    assert_eq!(parse_mode(b"ab"), parse_mode(b"a"), "`b` changed something");

    // C17 7.21.5.3p3 lists `r+b` and `rb+` as separate spellings of one mode, so the modifiers
    // are a set and not a sequence.
    assert_eq!(parse_mode(b"r+b"), parse_mode(b"rb+"), "`r+b` and `rb+` are the same mode");
    assert_eq!(parse_mode(b"w+b"), parse_mode(b"wb+"), "`w+b` and `wb+` are the same mode");
    assert_eq!(parse_mode(b"a+b"), parse_mode(b"ab+"), "`a+b` and `ab+` are the same mode");

    // `e` is bionic's `O_CLOEXEC` modifier. It is *recorded*, not discarded, and it changes no
    // access -- `rbe` grants exactly what `r` grants.
    let cloexec = parse_mode(b"rbe").expect("rbe");
    assert!(cloexec.close_on_exec, "`e` was dropped instead of recorded");
    assert_eq!(
        (cloexec.read, cloexec.write, cloexec.create),
        (true, false, false),
        "`e` changed the access"
    );
    assert_eq!(OpenMode { close_on_exec: false, ..cloexec }, parse_mode(b"rb").expect("rb"));

    // `x` is `O_EXCL` (C11 added `wx` to 7.21.5.3p3), honoured where something is created.
    let exclusive = parse_mode(b"wx").expect("wx");
    assert_eq!(
        (exclusive.write, exclusive.create, exclusive.truncate, exclusive.exclusive),
        (true, true, true, true),
        "`wx` did not ask for an exclusive create"
    );
    assert!(parse_mode(b"ax").expect("ax").exclusive, "`ax` did not ask for an exclusive create");
    assert!(!parse_mode(b"w").expect("w").exclusive, "`w` alone asked to be exclusive");
    assert!(!parse_mode(b"r").expect("r").exclusive, "`r` alone asked to be exclusive");

    // ...and dropped where nothing is, because POSIX.1-2017 `open` says `O_EXCL` without
    // `O_CREAT` is undefined, so there is no defined thing to pass on. The access is untouched,
    // which is what stops this being the finding's own shape.
    let read_exclusive = parse_mode(b"rx").expect("rx");
    assert!(!read_exclusive.create, "`rx` must not create");
    assert!(
        !read_exclusive.exclusive,
        "`rx` set O_EXCL with no O_CREAT, which POSIX leaves undefined"
    );
    assert_eq!(
        (read_exclusive.read, read_exclusive.write),
        (true, false),
        "`rx` changed the access rather than only the flag"
    );
}

/// The refusals: empty, a first byte that is not `r`/`w`/`a`, and an unknown modifier.
///
/// **Membership, not a count** (`docs/VERIFICATION.md` entry 1): each case names the byte it
/// expects to be blamed for, so a parse that refused for a different reason fails here.
#[test]
fn a_mode_that_is_not_a_mode_is_a_named_einval_and_not_a_guess() {
    assert_eq!(parse_mode(b""), Err(ModeRefusal::Empty), "an empty mode");
    assert_eq!(parse_mode(b"\0"), Err(ModeRefusal::Empty), "a mode that is only a terminator");

    // A leading space. C17 7.21.5.3p3's list has no leading whitespace and nothing skips it.
    assert_eq!(parse_mode(b" r"), Err(ModeRefusal::Access(b' ')), "a leading space");
    assert_eq!(parse_mode(b" "), Err(ModeRefusal::Access(b' ')), "a mode that is one space");
    assert_eq!(parse_mode(b"\tr+"), Err(ModeRefusal::Access(b'\t')), "a leading tab");

    // A first byte that is not an access letter, including the ones that look close.
    for bad in [&b"R"[..], b"W", b"A", b"+", b"b", b"x", b"e", b"rw", b"z"] {
        let refusal = parse_mode(bad).expect_err(&format!(
            "`{}` was accepted as a mode",
            String::from_utf8_lossy(bad)
        ));
        // `rw` is the interesting one: its FIRST byte is a valid access letter, so it is the
        // `w` that is blamed, not the `r`.
        let blamed = if bad == b"rw" {
            ModeRefusal::Modifier(b'w')
        } else {
            ModeRefusal::Access(bad[0])
        };
        assert_eq!(refusal, blamed, "`{}`", String::from_utf8_lossy(bad));
    }

    // A modifier this layer does not understand. It is refused rather than ignored: an unknown
    // character MIGHT be one that changes the access on a device, and a refusal is the only
    // answer that cannot hand back a stream whose access differs from the one asked for.
    assert_eq!(parse_mode(b"rt"), Err(ModeRefusal::Modifier(b't')), "MSVC's text-mode `t`");
    assert_eq!(parse_mode(b"rm"), Err(ModeRefusal::Modifier(b'm')), "glibc's mmap `m`");
    assert_eq!(parse_mode(b"r,ccs=UTF-8"), Err(ModeRefusal::Modifier(b',')), "glibc's `,ccs=`");
    let junk = parse_mode(b"rbbbbbbbbbbbbbby");
    assert_eq!(junk, Err(ModeRefusal::Modifier(b'y')), "a sixteen-byte junk mode");

    // Non-printable bytes are guest-chosen too, and the message must still be readable.
    assert_eq!(parse_mode(b"r\x01"), Err(ModeRefusal::Modifier(1)));
    assert_eq!(parse_mode(b"\xff"), Err(ModeRefusal::Access(0xff)));
    assert!(format!("{}", ModeRefusal::Access(0xff)).contains("0xff"), "an unprintable byte");
    assert!(format!("{}", ModeRefusal::Modifier(b'y')).contains("`y`"), "a printable byte");

    // Every refusal is EINVAL, which is what `fopen` reports for a mode it cannot parse.
    for refusal in [ModeRefusal::Empty, ModeRefusal::Access(b'z'), ModeRefusal::Modifier(b'y')] {
        assert_eq!(refusal.errno(), consts::EINVAL, "{refusal:?}");
    }
}

/// A NUL ends the mode, because that is where a C string ends.
///
/// `fopen(path, "r\0b+")` asks for `"r"`. Honouring that is not the truncation the finding is
/// about — it is C's own definition of a string, and it is the one case where dropping bytes is
/// the correct answer rather than the defect.
#[test]
fn an_interior_nul_ends_the_mode_string() {
    assert_eq!(parse_mode(b"r\0b+"), parse_mode(b"r"), "the bytes after a NUL are not the mode");
    assert_eq!(parse_mode(b"w+\0xxxx"), parse_mode(b"w+"));
    assert!(!parse_mode(b"r\0+").expect("r").write, "a `+` after the NUL granted write access");
    // And a mode whose tail after the NUL would have been *invalid* still parses, which is the
    // half a "reject anything with a NUL in it" reading would get wrong.
    assert_eq!(parse_mode(b"a\0!!!"), parse_mode(b"a"));
}

/// The parse is total over arbitrary bytes: it answers or refuses, and never panics.
///
/// Hostile input is the expected case here. This walks every single byte in both positions and
/// asserts only that *something* was decided — the specific answers are asserted above, and a
/// test that restated them here would be asserting its own definition.
#[test]
fn every_byte_in_either_position_is_decided_rather_than_assumed() {
    for byte in 0u8..=255 {
        let first = parse_mode(&[byte]);
        match byte {
            b'r' | b'w' | b'a' => assert!(first.is_ok(), "{byte:#04x} as an access letter"),
            0 => assert_eq!(first, Err(ModeRefusal::Empty)),
            _ => assert_eq!(first, Err(ModeRefusal::Access(byte))),
        }
        let second = parse_mode(&[b'r', byte]);
        match byte {
            0 | b'+' | b'b' | b'x' | b'e' => assert!(second.is_ok(), "{byte:#04x} as a modifier"),
            _ => assert_eq!(second, Err(ModeRefusal::Modifier(byte))),
        }
    }
}

/// Repetition and order change nothing, at any count.
///
/// A parse that flipped a flag per occurrence instead of setting it would pass every test above,
/// because every mode there has at most one of each modifier.
#[test]
fn a_repeated_modifier_is_not_a_toggle() {
    assert_eq!(parse_mode(b"r++"), parse_mode(b"r+"), "a doubled `+` cancelled itself");
    assert_eq!(parse_mode(b"r+++"), parse_mode(b"r+"));
    assert_eq!(parse_mode(b"wxx"), parse_mode(b"wx"), "a doubled `x` cancelled itself");
    assert_eq!(parse_mode(b"reebb++xx"), parse_mode(b"r+bex"), "order or repetition mattered");
    let mut five_thousand = vec![b'r'];
    five_thousand.extend(std::iter::repeat_n(b'+', 5000));
    let many = parse_mode(&five_thousand).expect("five thousand plus signs");
    assert!(many.read && many.write, "five thousand `+` did not mean read-write");
}
