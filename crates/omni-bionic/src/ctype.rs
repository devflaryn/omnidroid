//! C/POSIX locale character classification and conversion.
//!
//! The **C/POSIX locale** is the only locale bionic has, and POSIX pins its classes exactly, so
//! every function here is a closed-form test on the low byte:
//!
//! * `isspace(c)`: true for space and `\t \n \v \f \r` — and nothing else (C11 7.4.1.10,
//!   POSIX: "the space character and the four space-adjacent control characters", i.e.
//!   exactly `\t\n\v\f\r` plus space).
//! * `isalpha`, `isupper`, `islower`, `isdigit`, `isxdigit`: C11 7.4.1.1–7.4.1.6, each
//!   exactly its ASCII set.
//! * `tolower(c)`: folds `A`..`Z` to `a`..`z`; `toupper(c)` folds `a`..`z` to `A`..`Z`. All
//!   other bytes unchanged (C11 7.4.2.2, 7.4.2.1).
//!
//! In the C locale no byte ≥ 0x80 has class membership beyond the above.
//!
//! # Which of these are reachable, and why that is not a speculative surface
//!
//! `isspace` and `tolower` are imported by `libroblox.so`'s own initializers, and the thread-failure
//! assertion named each further one the first time a guest asked for it: the APK's compression
//! library imports the `isalpha`/`is{upper,lower,digit,xdigit}_l` families and `toupper`, which is
//! what these were added for (MEASURED 2026-09-26, on a substituted build of
//! `libzstd-jni-1.5.7-6.so`). An implementation that no guest has asked for is not added — this
//! module had two functions and grew to eight because eight were measured, not because a header
//! lists them.
//!
//! The `_l` forms take a `locale_t` and this crate **ignores it**, which is what bionic does for
//! every locale it has: `LC_GLOBAL` (0) is the only value a C-locale caller can pass, and a
//! non-null locale pointer a guest built is refused at the adapter rather than silently treated as
//! the C locale.
//!
//! `to_lower_ascii`/`is_space_ascii` are also used by [`crate::string::strcasecmp`], which
//! POSIX defines in the current locale — the C locale for this crate.

/// C-locale `isspace` on a byte (as `int` in C, where the argument must be representable
/// as `unsigned char` or `EOF`; a negative value other than `EOF` is UB in C — here the
/// guest callers pass `int`s, so any `i32` is accepted and only the low byte classifies).
pub fn is_space(c: i32) -> bool {
    matches!(c as u8, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// C-locale `tolower` on a byte: folds `A`..`Z` only.
pub fn to_lower(c: i32) -> i32 {
    let b = c as u8;
    if b.is_ascii_uppercase() {
        (b + 32) as i32
    } else {
        c
    }
}

/// Byte-level helper for [`crate::string::strcasecmp`]: ASCII-lowercase one byte.
pub fn to_lower_ascii(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

/// C-locale `isalpha`: `A`..`Z` and `a`..`z`, and nothing else (C11 7.4.1.1).
pub fn is_alpha(c: i32) -> bool {
    (c as u8).is_ascii_alphabetic()
}

/// C-locale `isupper`: `A`..`Z` (C11 7.4.1.2).
pub fn is_upper(c: i32) -> bool {
    (c as u8).is_ascii_uppercase()
}

/// C-locale `islower`: `a`..`z` (C11 7.4.1.3).
pub fn is_lower(c: i32) -> bool {
    (c as u8).is_ascii_lowercase()
}

/// C-locale `isdigit`: `0`..`9` (C11 7.4.1.4).
pub fn is_digit(c: i32) -> bool {
    (c as u8).is_ascii_digit()
}

/// C-locale `isxdigit`: `0`..`9`, `A`..`F`, `a`..`f` (C11 7.4.1.6).
pub fn is_xdigit(c: i32) -> bool {
    (c as u8).is_ascii_hexdigit()
}

/// C-locale `toupper`: folds `a`..`z` to `A`..`Z`; every other byte unchanged (C11 7.4.2.1).
pub fn to_upper(c: i32) -> i32 {
    let b = c as u8;
    if b.is_ascii_lowercase() {
        (b - 32) as i32
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isspace_exactly_six_bytes() {
        assert!(is_space(b' ' as i32));
        assert!(is_space(b'\t' as i32));
        assert!(is_space(b'\n' as i32));
        assert!(is_space(0x0B));
        assert!(is_space(0x0C));
        assert!(is_space(b'\r' as i32));
        // Everything else must be false.
        assert!(!is_space(b'a' as i32));
        assert!(!is_space(b'0' as i32));
        assert!(!is_space(0x00));
        assert!(!is_space(0xA0)); // NBSP is not space in the C locale
        assert!(!is_space(0xFF));
        assert!(!is_space(-1)); // low byte 0xFF
    }

    #[test]
    fn tolower_folds_only_ascii_uppercase() {
        assert_eq!(to_lower(b'A' as i32), b'a' as i32);
        assert_eq!(to_lower(b'Z' as i32), b'z' as i32);
        assert_eq!(to_lower(b'M' as i32), b'm' as i32);
        assert_eq!(to_lower(b'a' as i32), b'a' as i32);
        assert_eq!(to_lower(b'0' as i32), b'0' as i32);
        assert_eq!(to_lower(b'@' as i32), b'@' as i32); // 0x40, just below 'A'
        assert_eq!(to_lower(b'[' as i32), b'[' as i32); // 0x5B, just above 'Z'
        assert_eq!(to_lower(0xC0), 0xC0); // À folds only outside the C locale
    }

    #[test]
    fn byte_helper_matches_int_helper() {
        for b in 0u8..=255 {
            assert_eq!(to_lower_ascii(b), b.to_ascii_lowercase());
        }
    }

    /// **Every predicate is exactly its ASCII set, over all 256 byte values** — which is the whole
    /// claim, and a spot check would not make it. `EOF` (-1) is included as a negative `int`,
    /// since C's `int` argument is where a caller can pass it.
    #[test]
    fn every_predicate_is_exactly_its_ascii_set() {
        for c in -2i32..=255 {
            let byte = c as u8;
            assert_eq!(is_alpha(c), byte.is_ascii_alphabetic(), "isalpha({c})");
            assert_eq!(is_upper(c), byte.is_ascii_uppercase(), "isupper({c})");
            assert_eq!(is_lower(c), byte.is_ascii_lowercase(), "islower({c})");
            assert_eq!(is_digit(c), byte.is_ascii_digit(), "isdigit({c})");
            assert_eq!(is_xdigit(c), byte.is_ascii_hexdigit(), "isxdigit({c})");
        }
        // The boundary bytes, named: 0x40 is '@' (not alpha), 0x5B '[' (not upper), 0x60 '`'
        // (not lower), 0x3A ':' (not digit), 0x47 'G' (not xdigit), 0x60 '`' again.
        assert!(!is_alpha(b'@' as i32) && !is_alpha(b'[' as i32));
        assert!(!is_upper(b'@' as i32) && is_upper(b'A' as i32));
        assert!(!is_lower(b'`' as i32) && is_lower(b'a' as i32));
        assert!(!is_digit(b':' as i32) && is_digit(b'0' as i32));
        assert!(!is_xdigit(b'G' as i32) && is_xdigit(b'F' as i32) && is_xdigit(b'f' as i32));
    }

    /// `toupper` folds only `a`..`z` and is the mirror of `tolower`.
    ///
    /// **The round trip is per case, and asserting the wrong one is a trap**: `tolower(toupper(x))`
    /// is `x` only for a lowercase letter, and `toupper(tolower(x))` only for an uppercase one --
    /// `tolower(toupper('A'))` is `'a'`, and a test claiming otherwise would be asserting a
    /// property C does not have. What is true over the whole byte range: both functions leave a
    /// non-letter alone, a lowercase letter survives an up-then-down trip, and an uppercase letter
    /// survives a down-then-up trip.
    #[test]
    fn toupper_folds_only_ascii_lowercase_and_mirrors_tolower() {
        assert_eq!(to_upper(b'a' as i32), b'A' as i32);
        assert_eq!(to_upper(b'z' as i32), b'Z' as i32);
        assert_eq!(to_upper(b'A' as i32), b'A' as i32);
        assert_eq!(to_upper(b'0' as i32), b'0' as i32);
        assert_eq!(to_upper(b'@' as i32), b'@' as i32);
        assert_eq!(to_upper(b'[' as i32), b'[' as i32);
        assert_eq!(to_upper(b'`' as i32), b'`' as i32);
        for b in 0u8..=255 {
            let c = b as i32;
            if b.is_ascii_alphabetic() {
                if b.is_ascii_lowercase() {
                    assert_eq!(to_lower(to_upper(c)), c, "lower(up({b:#x})) is not itself");
                    assert!(is_upper(to_upper(c)), "up({b:#x}) is not uppercase");
                } else {
                    assert_eq!(to_upper(to_lower(c)), c, "up(lower({b:#x})) is not itself");
                    assert!(is_lower(to_lower(c)), "lower({b:#x}) is not lowercase");
                }
            } else {
                assert_eq!(to_upper(c), c, "toupper moved a non-letter, {b:#x}");
                assert_eq!(to_lower(c), c, "tolower moved a non-letter, {b:#x}");
            }
        }
    }
}
