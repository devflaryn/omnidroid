//! C/POSIX locale character classification and conversion.
//!
//! Only `isspace` and `tolower` are reachable from libroblox.so's initializers (the
//! `isw*_l`/`tow*_l` families are never-referenced — see the phase 0 scope table). They are
//! implemented for the **C/POSIX locale**, which POSIX pins exactly:
//!
//! * `isspace(c)`: true for space and `\t \n \v \f \r` — and nothing else (C11 7.4.1.10,
//!   POSIX: "the space character and the four space-adjacent control characters", i.e.
//!   exactly `\t\n\v\f\r` plus space).
//! * `tolower(c)`: folds `A`..`Z` to `a`..`z`; all other bytes unchanged (C11 7.4.2.2).
//!
//! In the C locale no byte ≥ 0x80 has class membership beyond the above, so the tables are
//! tiny and exact. `isalpha`, `isdigit`, `toupper` etc. are NOT imported by the engine and
//! are not implemented (no speculative surface).
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
}
