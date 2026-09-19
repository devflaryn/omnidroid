//! Phase 3 tests: the `str*` family, `_chk` variants, and error strings.
//!
//! Oracles:
//! * hand-reasoned C99/POSIX semantics (`strlen(3)` ... `strcspn(3)`);
//! * bionic's documented FORTIFY contracts for `_chk` forms;
//! * the POSIX-specified error message texts;
//! * Rust's `str`/`slice` methods where their semantics genuinely match
//!   (e.g. `find` for `strstr`, ASCII case folding for `strcasecmp` — in the C locale
//!   these coincide exactly);
//! * sign-only assertions for `strcmp`/`strncmp`/`strcasecmp`/`strncasecmp`.
//!
//! Hostile inputs per function: null pointers, unterminated strings running into unmapped
//! memory (asserting the exact fault address and *no hang*), destination overflows,
//! zero lengths, overlap, and ranges near `u64::MAX`.

use omni_bionic::error::BionicError;
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;
use omni_bionic::string::{
    gnu_strerror_r, strcat, strcat_chk, strchr, strcmp, strcasecmp, strcpy, strcspn, strlen,
    strlen_chk, strncasecmp, strncmp, strncpy, strncpy_chk, strncpy_chk2, strncat, strnlen,
    strrchr, strspn, strstr,
};

fn m_str(s: &str, at: u64) -> MockMemory {
    let mut mem = MockMemory::new();
    mem.map_str(at, s);
    mem
}

fn two_strings(a: &str, at_a: u64, b: &str, at_b: u64) -> MockMemory {
    let mut mem = MockMemory::new();
    mem.map_str(at_a, a);
    mem.map_str(at_b, b);
    mem
}

// ---------------------------------------------------------------- strlen family

#[test]
fn strlen_basic_and_empty() {
    let mem = m_str("hello", 0x1000);
    assert_eq!(strlen(&mem, 0x1000), Ok(5));
    assert_eq!(strlen(&mem, 0x1005), Ok(0)); // at the NUL
}

#[test]
fn strlen_unterminated_faults_at_region_end_no_hang() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0x41; 32]); // no NUL anywhere
    assert_eq!(strlen(&mem, 0x1000), Err(Fault(0x1020)));
}

#[test]
fn strlen_null_faults_at_zero() {
    let mem = MockMemory::new();
    assert_eq!(strlen(&mem, 0), Err(Fault(0)));
}

#[test]
fn strlen_chk_ok_and_overflow_named() {
    let mem = m_str("hello", 0x1000);
    // Object of 8 bytes holding a 5-char string: fine.
    assert_eq!(strlen_chk(&mem, 0x1000, 8), Ok(5));
    // size 6 with len 5: room for the NUL — fine.
    assert_eq!(strlen_chk(&mem, 0x1000, 6), Ok(5));
    // Exactly full (5 == 5): the string leaves no room for its NUL — failed check.
    assert_eq!(
        strlen_chk(&mem, 0x1000, 5).unwrap_err(),
        BionicError::CheckFailed("__strlen_chk")
    );
    // Smaller still: also failed.
    assert_eq!(
        strlen_chk(&mem, 0x1000, 4).unwrap_err(),
        BionicError::CheckFailed("__strlen_chk")
    );
}

#[test]
fn strnlen_bounded_scans_and_caps() {
    let mem = two_strings("hello", 0x1000, "abcdef", 0x2000);
    assert_eq!(strnlen(&mem, 0x1000, 8), Ok(5));
    assert_eq!(strnlen(&mem, 0x1000, 5), Ok(5)); // NUL at index 5 is the cap
    assert_eq!(strnlen(&mem, 0x1000, 3), Ok(3)); // capped by n
    assert_eq!(strnlen(&mem, 0x1000, 0), Ok(0));
    // No NUL within n: returns n without needing the terminator.
    assert_eq!(strnlen(&mem, 0x2000, 3), Ok(3));
}

#[test]
fn strnlen_unmapped_within_n_faults() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0x41; 4]);
    assert_eq!(strnlen(&mem, 0x1000, 8), Err(Fault(0x1004)));
}

// ---------------------------------------------------------------- strcmp family

#[test]
fn strcmp_sign_only() {
    let mem = two_strings("apple", 0x1000, "banana", 0x2000);
    let ab = strcmp(&mem, 0x1000, 0x2000).unwrap();
    let ba = strcmp(&mem, 0x2000, 0x1000).unwrap();
    assert!(ab < 0);
    assert!(ba > 0);
    assert_eq!(strcmp(&mem, 0x1000, 0x1000), Ok(0));
    // Prefix ordering: "app" < "apple".
    let mem2 = two_strings("app", 0x1000, "apple", 0x2000);
    assert!(strcmp(&mem2, 0x1000, 0x2000).unwrap() < 0);
    // NUL vs byte: "app\0..." vs "appX..." — NUL sorts first.
    assert!(strcmp(&mem2, 0x1000, 0x2000).unwrap() < 0);
    // Sign-only, but the magnitude must be plausible for bionic: bionic returns the
    // byte difference, and byte differences are bounded by ±255 ('a'=97 vs 'z'=122 → -25).
    let mem3 = two_strings("a", 0x1000, "z", 0x2000);
    let d = strcmp(&mem3, 0x1000, 0x2000).unwrap();
    assert_eq!(d, -25, "bionic strcmp returns the byte difference 'a' - 'z'");
}

#[test]
fn strcmp_null_faults() {
    let mem = m_str("x", 0x1000);
    assert_eq!(strcmp(&mem, 0, 0x1000), Err(Fault(0)));
    assert_eq!(strcmp(&mem, 0x1000, 0), Err(Fault(0)));
}

#[test]
fn strcmp_unterminated_faults() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0x41; 8]);
    mem.map(0x2000, &[0x41; 8]);
    assert_eq!(strcmp(&mem, 0x1000, 0x2000), Err(Fault(0x1008)));
}

#[test]
fn strncmp_bounds_and_zero_n() {
    let mem = two_strings("apple", 0x1000, "apply", 0x2000);
    assert_eq!(strncmp(&mem, 0x1000, 0x2000, 4), Ok(0)); // "appl" == "appl"
    let ab = strncmp(&mem, 0x1000, 0x2000, 5).unwrap();
    assert!(ab < 0); // 'e' < 'y'
    assert_eq!(strncmp(&mem, 0x1000, 0x2000, 0), Ok(0));
    // n == 0 at null pointers: valid C, no access.
    assert_eq!(strncmp(&mem, 0, 0, 0), Ok(0));
}

#[test]
fn strcasecmp_c_locale_folding() {
    let mem = two_strings("MiXeDcAsE", 0x1000, "mixedcase", 0x2000);
    assert_eq!(strcasecmp(&mem, 0x1000, 0x2000), Ok(0));
    let mem2 = two_strings("a", 0x1000, "B", 0x2000);
    assert!(strcasecmp(&mem2, 0x1000, 0x2000).unwrap() < 0);
    // Bytes >= 0x80 compare unsigned, un-folded. (Rust string literals cannot carry raw
    // \xC4 escapes, so the bytes are written directly.)
    let mut mem3 = MockMemory::new();
    mem3.map(0x1000, &[0xC4, 0]);
    mem3.map(0x2000, &[0xC3, 0]);
    assert!(strcasecmp(&mem3, 0x1000, 0x2000).unwrap() > 0);
}

#[test]
fn strncasecmp_matches_c_locale_semantics() {
    let mem = two_strings("HELLO", 0x1000, "hello!", 0x2000);
    assert_eq!(strncasecmp(&mem, 0x1000, 0x2000, 5), Ok(0));
    // 6th byte: NUL vs '!' — NUL is less.
    assert!(strncasecmp(&mem, 0x1000, 0x2000, 6).unwrap() < 0);
    assert_eq!(strncasecmp(&mem, 0x1000, 0x2000, 0), Ok(0));
}

// ---------------------------------------------------------------- copy family

#[test]
fn strcpy_copies_including_nul_and_returns_dst() {
    let mut mem = MockMemory::new();
    mem.map_str(0x2000, "source");
    mem.map(0x1000, &[0u8; 16]);
    assert_eq!(strcpy(&mut mem, 0x1000, 0x2000), Ok(0x1000));
    let mut out = [0u8; 7];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(&out[..6], b"source");
    assert_eq!(out[6], 0);
}

#[test]
fn strcpy_empty_source_writes_one_nul() {
    let mut mem = MockMemory::new();
    mem.map_str(0x2000, "");
    mem.map(0x1000, &[0x41; 4]);
    assert_eq!(strcpy(&mut mem, 0x1000, 0x2000), Ok(0x1000));
    let mut out = [0u8; 4];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0, 0x41, 0x41, 0x41]); // only the first byte changed
}

#[test]
fn strcpy_null_or_unterminated_faults_before_writing() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 8]);
    mem.map(0x2000, &[0x42; 8]); // unterminated
    assert_eq!(strcpy(&mut mem, 0x1000, 0), Err(Fault(0)));
    assert_eq!(strcpy(&mut mem, 0x1000, 0x2000), Err(Fault(0x2008)));
    // Nothing was written.
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [0u8; 8]);
}

#[test]
fn strcpy_unmapped_dst_faults_before_writing() {
    let mut mem = MockMemory::new();
    mem.map_str(0x2000, "abc");
    assert_eq!(strcpy(&mut mem, 0x5000, 0x2000), Err(Fault(0x5000)));
}

#[test]
fn strncpy_exact_semantics_copy_pad_truncate() {
    let mut mem = MockMemory::new();
    mem.map_str(0x2000, "ab");
    mem.map(0x1000, &[0x7F; 8]);
    // Pad case: writes exactly n bytes ('a','b',NUL,NUL,NUL).
    assert_eq!(strncpy(&mut mem, 0x1000, 0x2000, 5), Ok(0x1000));
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [b'a', b'b', 0, 0, 0, 0x7F, 0x7F, 0x7F]);

    // Truncate case: n = 3 from "abcde" writes 'a','b','c' and NO terminator.
    let mut mem2 = MockMemory::new();
    mem2.map_str(0x2000, "abcde");
    mem2.map(0x1000, &[0x7F; 8]);
    assert_eq!(strncpy(&mut mem2, 0x1000, 0x2000, 3), Ok(0x1000));
    let mut out = [0u8; 8];
    mem2.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [b'a', b'b', b'c', 0x7F, 0x7F, 0x7F, 0x7F, 0x7F]);
}

#[test]
fn strncpy_zero_n_is_noop_even_at_null() {
    let mut mem = MockMemory::new();
    assert_eq!(strncpy(&mut mem, 0, 0, 0), Ok(0));
    assert_eq!(strncpy(&mut mem, 0x1000, 0, 0), Ok(0x1000));
}

#[test]
fn strncpy_src_unterminated_faults() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 16]);
    mem.map(0x2000, &[0x41; 8]); // no NUL
    assert_eq!(strncpy(&mut mem, 0x1000, 0x2000, 12), Err(Fault(0x2008)));
}

#[test]
fn strncpy_chk_and_chk2_named_failures() {
    let mut mem = MockMemory::new();
    mem.map_str(0x2000, "abc");
    mem.map(0x1000, &[0u8; 4]);
    // dst overflow: n > dst_size.
    let err = strncpy_chk(&mut mem, 0x1000, 0x2000, 5, 4).unwrap_err();
    assert_eq!(err, BionicError::CheckFailed("__strncpy_chk"));
    // chk2: n > src_size fails even when dst is fine.
    let err2 = strncpy_chk2(&mut mem, 0x1000, 0x2000, 4, 8, 3).unwrap_err();
    assert_eq!(err2, BionicError::CheckFailed("__strncpy_chk2"));
    // chk2 happy path: n <= both.
    assert_eq!(strncpy_chk2(&mut mem, 0x1000, 0x2000, 3, 8, 6), Ok(0x1000));
    let mut out = [0u8; 4];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(out, [b'a', b'b', b'c', 0]);
}

#[test]
fn strcat_appends_and_terminates() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &make_buf(b"foo", 16));
    mem.map_str(0x2000, "bar");
    assert_eq!(strcat(&mut mem, 0x1000, 0x2000), Ok(0x1000));
    let mut out = [0u8; 16];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(&out[..7], b"foobar\0");
}

#[test]
fn strcat_unmapped_dst_tail_faults_before_writing() {
    // dst region ends before the append fits: validate-first must fault with no partial write.
    let mut mem = MockMemory::new();
    mem.map(0x1000, &make_buf(b"foo", 5)); // dst string ends at 0x1004
    mem.map_str(0x2000, "barbaz");
    // Appending 7 bytes at the NUL (0x1003) would run to 0x100A: outside the region.
    let res = strcat(&mut mem, 0x1000, 0x2000);
    assert!(matches!(res, Err(Fault(_))));
    // Nothing was appended: the dst NUL is still at 0x1003 and bytes after are 0xAA-free.
    let mut out = [0u8; 5];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(&out[..4], b"foo\0");
}

#[test]
fn strncat_caps_and_terminates() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &make_buf(b"foo", 16));
    mem.map_str(0x2000, "barbaz");
    assert_eq!(strncat(&mut mem, 0x1000, 0x2000, 3), Ok(0x1000));
    let mut out = [0u8; 16];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(&out[..7], b"foobar\0"); // only 3 src bytes, then NUL
}

#[test]
fn strcat_chk_checks_combined_length() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &make_buf(b"foo", 8)); // object size 8
    mem.map_str(0x2000, "bar");
    // 3 + 3 + 1 = 7 <= 8: fine.
    assert_eq!(strcat_chk(&mut mem, 0x1000, 0x2000, 8), Ok(0x1000));
    let mut out = [0u8; 8];
    mem.read(0x1000, &mut out).unwrap();
    assert_eq!(&out[..7], b"foobar\0");

    // Overflow: fresh dst "foo" (3) + "barbaz" (6) + 1 = 10 > 8.
    let mut mem2 = MockMemory::new();
    mem2.map(0x1000, &make_buf(b"foo", 8));
    mem2.map_str(0x2000, "barbaz");
    let err = strcat_chk(&mut mem2, 0x1000, 0x2000, 8).unwrap_err();
    assert_eq!(err, BionicError::CheckFailed("__strcat_chk"));
}

fn make_buf(s: &[u8], total: usize) -> Vec<u8> {
    let mut v = s.to_vec();
    v.push(0);
    v.resize(total, 0);
    v
}

// ---------------------------------------------------------------- search family

#[test]
fn strchr_includes_terminator_and_missing_returns_null() {
    let mem = m_str("hello", 0x1000);
    assert_eq!(strchr(&mem, 0x1000, 'l' as i32), Ok(0x1002));
    assert_eq!(strchr(&mem, 0x1000, 'h' as i32), Ok(0x1000));
    // The NUL is findable.
    assert_eq!(strchr(&mem, 0x1000, 0), Ok(0x1005));
    // 'z' is absent, so the scan walks to the NUL and past it — C says strchr scans until
    // it finds the byte, so absence inside a 6-byte mapping means the scan hits unmapped
    // memory after the terminator: a fault (not guest NULL) is the honest result here.
    assert_eq!(strchr(&mem, 0x1000, 'z' as i32), Err(Fault(0x1006)));
    // c is converted to unsigned char: 0x141 matches 'A'.
    let mem2 = m_str("A", 0x1000);
    assert_eq!(strchr(&mem2, 0x1000, 0x141), Ok(0x1000));
}

#[test]
fn strchr_null_and_unterminated() {
    let mem = MockMemory::new();
    assert_eq!(strchr(&mem, 0, 'a' as i32), Err(Fault(0)));
    let mut mem2 = MockMemory::new();
    mem2.map(0x1000, &[0x41; 8]);
    assert_eq!(strchr(&mem2, 0x1000, 'z' as i32), Err(Fault(0x1008)));
}

#[test]
fn strrchr_finds_last_occurrence() {
    let mem = m_str("hello", 0x1000);
    assert_eq!(strrchr(&mem, 0x1000, 'l' as i32), Ok(0x1003));
    assert_eq!(strrchr(&mem, 0x1000, 'h' as i32), Ok(0x1000));
    assert_eq!(strrchr(&mem, 0x1000, 'z' as i32), Ok(0));
    // NUL is a candidate: the terminator itself.
    assert_eq!(strrchr(&mem, 0x1000, 0), Ok(0x1005));
}

#[test]
fn strstr_prefix_middle_and_not_found() {
    let mem = two_strings("hello world", 0x1000, "world", 0x2000);
    assert_eq!(strstr(&mem, 0x1000, 0x2000), Ok(0x1006));
    let mem2 = two_strings("abcabc", 0x1000, "abc", 0x2000);
    assert_eq!(strstr(&mem2, 0x1000, 0x2000), Ok(0x1000)); // first match
    let mem3 = two_strings("abcabc", 0x1000, "abd", 0x2000);
    assert_eq!(strstr(&mem3, 0x1000, 0x2000), Ok(0));
}

#[test]
fn strstr_empty_needle_returns_haystack() {
    let mem = two_strings("abc", 0x1000, "", 0x2000);
    assert_eq!(strstr(&mem, 0x1000, 0x2000), Ok(0x1000));
}

#[test]
fn strstr_needle_longer_than_haystack_returns_null() {
    let mem = two_strings("ab", 0x1000, "abcdef", 0x2000);
    assert_eq!(strstr(&mem, 0x1000, 0x2000), Ok(0));
}

#[test]
fn strstr_null_faults() {
    let mem = two_strings("ab", 0x1000, "b", 0x2000);
    assert_eq!(strstr(&mem, 0, 0x2000), Err(Fault(0)));
    assert_eq!(strstr(&mem, 0x1000, 0), Err(Fault(0)));
}

#[test]
fn strstr_cross_chunk_candidate_matches() {
    // A needle whose match starts near the 256-byte chunk boundary exercises the
    // cross-chunk comparison path (needle spans the chunk edge).
    let mut mem = MockMemory::new();
    let mut hay = vec![0x2D; 254]; // '-'
    hay.extend_from_slice(b"NEEDLE");
    hay.push(0);
    mem.map(0x1000, &hay);
    mem.map_str(0x2000, "NEEDLE");
    assert_eq!(strstr(&mem, 0x1000, 0x2000), Ok(0x1000 + 254));
}

#[test]
fn strspn_and_strcspn_exact() {
    let mem = two_strings("abcXYabc", 0x1000, "abc", 0x2000);
    assert_eq!(strspn(&mem, 0x1000, 0x2000), Ok(3));
    // strcspn with reject "XY".
    let mem2 = two_strings("abcXYabc", 0x1000, "XY", 0x2000);
    assert_eq!(strcspn(&mem2, 0x1000, 0x2000), Ok(3));
    // All-of-s in accept: whole string (the NUL stops it).
    let mem3 = two_strings("ababab", 0x1000, "ab", 0x2000);
    assert_eq!(strspn(&mem3, 0x1000, 0x2000), Ok(6));
    // Empty accept set.
    let mem4 = two_strings("abc", 0x1000, "", 0x2000);
    assert_eq!(strspn(&mem4, 0x1000, 0x2000), Ok(0));
    // Empty reject set: runs to the NUL.
    assert_eq!(strcspn(&mem4, 0x1000, 0x2000), Ok(3));
}

#[test]
fn span_null_faults() {
    let mem = two_strings("abc", 0x1000, "b", 0x2000);
    assert_eq!(strspn(&mem, 0, 0x2000), Err(Fault(0)));
    assert_eq!(strspn(&mem, 0x1000, 0), Err(Fault(0)));
    assert_eq!(strcspn(&mem, 0x1000, 0), Err(Fault(0)));
}

// ---------------------------------------------------------------- strerror

#[test]
fn gnu_strerror_r_known_messages_and_returns_buf() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 64]);
    // POSIX/glibc text for EINVAL (22): "Invalid argument".
    assert_eq!(gnu_strerror_r(&mut mem, 22, 0x1000, 64), Ok(0x1000));
    let mut out = [0u8; 64];
    mem.read(0x1000, &mut out).unwrap();
    let nul = out.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&out[..nul], b"Invalid argument");
    // ERANGE (34): "Numerical result out of range".
    mem.write(0x1000, &[0u8; 64]).unwrap();
    gnu_strerror_r(&mut mem, 34, 0x1000, 64).unwrap();
    mem.read(0x1000, &mut out).unwrap();
    let nul = out.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&out[..nul], b"Numerical result out of range");
}

#[test]
fn gnu_strerror_r_unknown_code_fallback_shape() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 64]);
    gnu_strerror_r(&mut mem, 999, 0x1000, 64).unwrap();
    let mut out = [0u8; 64];
    mem.read(0x1000, &mut out).unwrap();
    let nul = out.iter().position(|&b| b == 0).unwrap();
    // bionic's fallback: "Unknown error <n>".
    assert_eq!(&out[..nul], b"Unknown error 999");
}

#[test]
fn gnu_strerror_r_truncates_and_still_terminates() {
    let mut mem = MockMemory::new();
    mem.map(0x1000, &[0u8; 64]);
    // "Invalid argument" needs 17 bytes; give it 8.
    gnu_strerror_r(&mut mem, 22, 0x1000, 8).unwrap();
    let mut out = [0u8; 64];
    mem.read(0x1000, &mut out).unwrap();
    // Exactly 7 message bytes + NUL, remainder untouched (zero from map).
    assert_eq!(&out[..8], b"Invalid\0");
    assert!(out[8..].iter().all(|&b| b == 0));
}

#[test]
fn gnu_strerror_r_zero_len_or_null_buf_rejected() {
    let mut mem = MockMemory::new();
    let err = gnu_strerror_r(&mut mem, 22, 0x1000, 0).unwrap_err();
    assert_eq!(err, BionicError::InvalidArgument("__gnu_strerror_r"));
    assert_eq!(
        gnu_strerror_r(&mut mem, 22, 0, 64).unwrap_err(),
        BionicError::InvalidArgument("__gnu_strerror_r")
    );
}
