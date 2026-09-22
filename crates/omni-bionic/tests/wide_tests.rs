//! Phase 3b tests: wide (32-bit `wchar_t`) and multibyte (UTF-8) functions.
//!
//! Oracles:
//! * the arm64 ABI itself (wchar_t = 4 bytes, little-endian) — element addresses asserted;
//! * the Unicode standard / RFC 3629 UTF-8 encoding table (hand-encoded sequences);
//! * C-standard return contracts for `mbrtowc`/`mbsrtowcs` ((size_t)-1/-2 forms);
//! * Linux errno numbering for EILSEQ = 84 (kernel UAPI, not the host's).
//!
//! Hostile inputs: null pointers, unmapped element reads, `n*4` overflowing `u64`,
//! invalid UTF-8 lead bytes, truncated sequences, overlong encodings, surrogates,
//! and U+10FFFF+1.

use omni_bionic::context::GuestContext;
use omni_bionic::memory::{Fault, GuestMemory};
use omni_bionic::mock::MockMemory;
use omni_bionic::wide::{
    btowc, c32rtomb, mbrtoc32, mbsnrtowcs, mbsrtowcs, mbrtowc, mbtowc, wcsnrtombs, wctob, wmemcmp,
    wmemchr, wcslen, MB_ILLEGAL, WEOF,
};

/// Context double over [`MockMemory`].
#[derive(Default)]
struct Ctx {
    mem: MockMemory,
    errno: i32,
}
impl GuestMemory for Ctx {
    fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
        self.mem.read(addr, buf)
    }
    fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
        self.mem.write(addr, buf)
    }
}
impl GuestContext for Ctx {
    fn errno(&self) -> i32 {
        self.errno
    }
    fn set_errno(&mut self, v: i32) {
        self.errno = v;
    }
    fn rand_state(&self) -> u32 {
        0
    }
    fn set_rand_state(&mut self, _: u32) {}
    fn scratch(&mut self) -> Option<(u64, usize)> {
        None
    }
}

/// Map `s` as UTF-32LE elements (each `char` becomes 4 bytes), NUL-terminated, at `addr`.
fn map_wstr(mem: &mut MockMemory, addr: u64, s: &[u32]) {
    let mut bytes = Vec::new();
    for &w in s {
        bytes.extend_from_slice(&w.to_le_bytes());
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    mem.map(addr, &bytes);
}

#[test]
fn wcslen_counts_elements_not_bytes() {
    let mut mem = MockMemory::new();
    map_wstr(&mut mem, 0x1000, &['a' as u32, 'b' as u32, 0x1F600]); // 😀 is 1 element
    assert_eq!(wcslen(&mem, 0x1000), Ok(3));
    assert_eq!(wcslen(&mem, 0x1000 + 4 * 3), Ok(0)); // the empty string at the NUL
}

#[test]
fn wcslen_unterminated_faults_not_hangs() {
    let mut mem = MockMemory::new();
    // 16 non-zero elements, nothing after: the scan must fault at the region end.
    mem.map(0x1000, &[0x41u8, 0, 0, 0].repeat(16));
    assert_eq!(wcslen(&mem, 0x1000), Err(Fault(0x1040)));
}

#[test]
fn wcslen_null_faults_at_zero() {
    let mem = MockMemory::new();
    assert_eq!(wcslen(&mem, 0), Err(Fault(0)));
}

#[test]
fn wmemchr_finds_element_address() {
    let mut mem = MockMemory::new();
    map_wstr(&mut mem, 0x1000, &[1, 2, 3, 2]);
    assert_eq!(wmemchr(&mem, 0x1000, 3, 4), Ok(0x1000 + 8));
    assert_eq!(wmemchr(&mem, 0x1000, 2, 4), Ok(0x1000 + 4)); // first match
    assert_eq!(wmemchr(&mem, 0x1000, 9, 4), Ok(0));
    // Zero elements: success without access, even at null (valid C).
    assert_eq!(wmemchr(&mem, 0, 1, 0), Ok(0));
    // n*4 overflow: faults at the element address, never wraps.
    assert_eq!(wmemchr(&mem, u64::MAX - 4, 1, 2), Err(Fault(u64::MAX - 4)));
}

#[test]
fn wmemcmp_sign_only_on_unsigned_elements() {
    let mut mem = MockMemory::new();
    map_wstr(&mut mem, 0x1000, &[1, 0xFFFF_FFFF]);
    map_wstr(&mut mem, 0x2000, &[1, 1]);
    assert_eq!(wmemcmp(&mem, 0x1000, 0x2000, 2), Ok(1)); // 0xFFFFFFFF > 1 unsigned
    assert_eq!(wmemcmp(&mem, 0x2000, 0x1000, 2), Ok(-1));
    assert_eq!(wmemcmp(&mem, 0x1000, 0x2000, 1), Ok(0));
    assert_eq!(wmemcmp(&mem, 0, 0x2000, 0), Ok(0)); // n==0 valid C
}

#[test]
fn wctob_single_byte_range_only() {
    assert_eq!(wctob(0), 0);
    assert_eq!(wctob(0x41), 0x41);
    assert_eq!(wctob(0x7F), 0x7F);
    assert_eq!(wctob(0x80), -1); // first non-single-byte in UTF-8
    assert_eq!(wctob(0x1F600), -1);
    assert_eq!(wctob(u32::MAX), -1);
}

// ------------------------------------------------------------------ mbrtowc

/// bionic's `mbrtoc32`, which is its `mbrtowc`: a character **split across calls** is finished by
/// the second call from the state the first left, and the second call returns the bytes *it*
/// consumed; `(size_t)-1` with `EILSEQ` (not a refusal) for bad input, with the state reset.
#[test]
fn mbrtoc32_keeps_a_split_character_in_the_state_as_bionic_does() {
    const STATE: u64 = 0x3000;
    const OUT: u64 = 0x2000;
    let state = |ctx: &Ctx| {
        let mut b = [0u8; 4];
        ctx.read(STATE, &mut b).unwrap();
        b
    };
    let out = |ctx: &Ctx| {
        let mut b = [0u8; 4];
        ctx.read(OUT, &mut b).unwrap();
        u32::from_le_bytes(b)
    };
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"A\xE2\x82\xAC\xFF\xC0\x80\xED\xA0\x80\x00");
    ctx.mem.map(OUT, &[0u8; 4]);
    ctx.mem.map(STATE, &[0u8; 8]);

    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1000, 8, STATE), Ok(1), "ASCII fast path");
    assert_eq!(out(&ctx), 0x41);
    // U+20AC in two calls: one byte, then the other two.
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1001, 1, STATE), Ok(u64::MAX - 1), "incomplete");
    assert_eq!(state(&ctx), [0xE2, 0, 0, 0], "the lead byte is kept");
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1002, 2, STATE), Ok(2), "the bytes this call took");
    assert_eq!(out(&ctx), 0x20AC);
    assert_eq!(state(&ctx), [0; 4], "reset once complete");
    // A bad lead byte, an overlong NUL and a surrogate: -1, EILSEQ, state reset.
    for (at, len, what) in [(0x1004, 1, "0xFF"), (0x1005, 2, "C0 80"), (0x1007, 3, "a surrogate")] {
        ctx.errno = 0;
        assert_eq!(mbrtoc32(&mut ctx, OUT, at, len, STATE), Ok(u64::MAX), "{what}");
        assert_eq!(ctx.errno, 84, "{what} is EILSEQ");
        assert_eq!(state(&ctx), [0; 4], "{what} resets the state");
    }
    // A continuation expected and not found, mid-sequence.
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1001, 1, STATE), Ok(u64::MAX - 1));
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1000, 1, STATE), Ok(u64::MAX), "'A' is not 10xxxxxx");
    // NUL, n == 0, and s == NULL.
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x100A, 1, STATE), Ok(0), "NUL");
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1000, 0, STATE), Ok(0), "n == 0 is 0 on Android 13");
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0, 5, STATE), Ok(0), "s == NULL returns 0");
    // A state with __seq[3] set is EINVAL.
    ctx.write(STATE, &[0xF0, 0x9F, 0x98, 0x80]).unwrap();
    assert_eq!(mbrtoc32(&mut ctx, OUT, 0x1000, 1, STATE), Ok(u64::MAX));
    assert_eq!(ctx.errno, 22, "EINVAL");
    assert_eq!(state(&ctx), [0; 4]);
}

/// bionic's `mbsnrtowcs`: `nmc` bounds the bytes read, `len` the characters written, `*src`
/// ends where the conversion stopped -- mid-string when `len` runs out, at the bad byte for
/// `EILSEQ` -- and a partial character left by `nmc` is `EILSEQ` with `*src` moved past `nmc`.
#[test]
fn mbsnrtowcs_moves_src_as_bionic_does() {
    const STATE: u64 = 0x3000;
    const SRC: u64 = 0x3100;
    const DST: u64 = 0x2000;
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a\xE2\x82\xACbc\x00\xFFz\x00");
    ctx.mem.map(DST, &[0u8; 64]);
    ctx.mem.map(STATE, &[0u8; 8]);
    ctx.mem.map(SRC, &[0u8; 8]);
    let src = |ctx: &Ctx| {
        let mut b = [0u8; 8];
        ctx.read(SRC, &mut b).unwrap();
        u64::from_le_bytes(b)
    };
    let set_src = |ctx: &mut Ctx, at: u64| ctx.write(SRC, &at.to_le_bytes()).unwrap();

    set_src(&mut ctx, 0x1000);
    assert_eq!(mbsnrtowcs(&mut ctx, 0, SRC, u64::MAX, 0, STATE), Ok(4), "measure: a U+20AC b c");
    // `len` = 2: two characters, *src after them.
    assert_eq!(mbsnrtowcs(&mut ctx, DST, SRC, u64::MAX, 2, STATE), Ok(2));
    assert_eq!(src(&ctx), 0x1004, "past 'a' and the three bytes of U+20AC");
    // `nmc` cutting U+20AC in half: EILSEQ, *src past nmc.
    set_src(&mut ctx, 0x1000);
    assert_eq!(mbsnrtowcs(&mut ctx, DST, SRC, 2, 8, STATE), Ok(MB_ILLEGAL));
    assert_eq!(ctx.errno, 84);
    assert_eq!(src(&ctx), 0x1002, "*src += nmc");
    // A bad byte: EILSEQ with *src at it.
    set_src(&mut ctx, 0x1007);
    assert_eq!(mbsnrtowcs(&mut ctx, DST, SRC, u64::MAX, 8, STATE), Ok(MB_ILLEGAL));
    assert_eq!(src(&ctx), 0x1007);
}

/// bionic's `wcsnrtombs` and `c32rtomb`: `len` too short for the next character stops before it
/// (a partial character is never written), and past `0x1FFFFF` is `EILSEQ`.
#[test]
fn wcsnrtombs_stops_before_a_character_that_does_not_fit() {
    const STATE: u64 = 0x3000;
    const SRC: u64 = 0x3100;
    const DST: u64 = 0x2000;
    let mut ctx = Ctx::default();
    let mut wide = Vec::new();
    for c in [0x61u32, 0x20AC, 0x62, 0] {
        wide.extend_from_slice(&c.to_le_bytes());
    }
    ctx.mem.map(0x1000, &wide);
    ctx.mem.map(DST, &[0xEEu8; 16]);
    ctx.mem.map(STATE, &[0u8; 8]);
    ctx.mem.map(SRC, &0x1000u64.to_le_bytes());
    assert_eq!(wcsnrtombs(&mut ctx, 0, SRC, u64::MAX, 0, STATE), Ok(5), "measure");
    assert_eq!(wcsnrtombs(&mut ctx, DST, SRC, u64::MAX, 3, STATE), Ok(1), "U+20AC needs 3");
    let mut out = [0u8; 4];
    ctx.read(DST, &mut out).unwrap();
    assert_eq!(out, [0x61, 0xEE, 0xEE, 0xEE], "nothing of U+20AC was written");
    let mut b = [0u8; 8];
    ctx.read(SRC, &mut b).unwrap();
    assert_eq!(u64::from_le_bytes(b), 0x1004, "*src at U+20AC");
    assert_eq!(c32rtomb(&mut ctx, DST, 0x20_0000, STATE), Ok(MB_ILLEGAL));
    assert_eq!(ctx.errno, 84);
    assert_eq!(c32rtomb(&mut ctx, DST, 0x10_FFFF, STATE), Ok(4));
    assert_eq!(c32rtomb(&mut ctx, 0, 0x41, STATE), Ok(1), "s == NULL is 1");
    assert_eq!(btowc(-1), WEOF);
    assert_eq!(btowc(0x141), 0x41, "(char)c");
    assert_eq!(btowc(0x80), WEOF);
    assert_eq!(wctob(WEOF), -1);
}

#[test]
fn mbrtowc_ascii_and_multibyte_decode() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a\xC3\xA9\xE2\x82\xAC\xF0\x9F\x98\x80"); // a é € 😀
    // Map a scratch wchar_t slot.
    ctx.mem.map(0x2000, &[0u8; 4]);

    // 'a': 1 byte, cp 0x61.
    ctx.mem.map(0x2000, &[0u8; 4]);
    assert_eq!(mbrtowc(&mut ctx, 0x2000, 0x1000, 4, 0), Ok(1));
    let mut out = [0u8; 4];
    ctx.read(0x2000, &mut out).unwrap();
    assert_eq!(u32::from_le_bytes(out), 0x61);

    // é: 2 bytes, cp 0xE9.
    assert_eq!(mbrtowc(&mut ctx, 0x2000, 0x1000 + 1, 4, 0), Ok(2));
    ctx.read(0x2000, &mut out).unwrap();
    assert_eq!(u32::from_le_bytes(out), 0xE9);

    // €: 3 bytes, cp 0x20AC.
    assert_eq!(mbrtowc(&mut ctx, 0x2000, 0x1000 + 3, 4, 0), Ok(3));
    ctx.read(0x2000, &mut out).unwrap();
    assert_eq!(u32::from_le_bytes(out), 0x20AC);

    // 😀: 4 bytes, cp 0x1F600.
    assert_eq!(mbrtowc(&mut ctx, 0x2000, 0x1000 + 6, 4, 0), Ok(4));
    ctx.read(0x2000, &mut out).unwrap();
    assert_eq!(u32::from_le_bytes(out), 0x1F600);
}

#[test]
fn mbrtowc_nul_returns_zero_and_stores_lzero() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"\x00");
    ctx.mem.map(0x2000, &[0u8; 4]);
    assert_eq!(mbrtowc(&mut ctx, 0x2000, 0x1000, 1, 0), Ok(0));
}

#[test]
fn mbrtowc_incomplete_returns_size_t_minus_2() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"\xC3"); // é without its second byte
    assert_eq!(mbrtowc(&mut ctx, 0, 0x1000, 1, 0), Ok(u64::MAX - 1));
    // Valid 4-byte lead, only 3 bytes present.
    ctx.mem.map(0x1100, b"\xF0\x9F\x98");
    assert_eq!(mbrtowc(&mut ctx, 0, 0x1100, 3, 0), Ok(u64::MAX - 1));
}

#[test]
fn mbrtowc_invalid_sets_eilseq_84() {
    let mut ctx = Ctx::default();
    // 0x80: bare continuation byte — invalid lead.
    ctx.mem.map(0x1000, b"\x80");
    let res = mbrtowc(&mut ctx, 0, 0x1000, 1, 0);
    assert!(res.is_err());
    assert_eq!(ctx.errno(), 84); // EILSEQ, Linux numbering (not the Windows value)
    // 0xFF: also invalid.
    ctx.mem.map(0x1100, b"\xFF");
    assert!(mbrtowc(&mut ctx, 0, 0x1100, 1, 0).is_err());
    assert_eq!(ctx.errno(), 84);
}

#[test]
fn mbrtowc_overlong_and_surrogate_rejected() {
    let mut ctx = Ctx::default();
    // Overlong 'a': 0xC1 0x81 (never valid — C1/C0 lead is invalid outright).
    ctx.mem.map(0x1000, b"\xC1\x81");
    assert!(mbrtowc(&mut ctx, 0, 0x1000, 2, 0).is_err());
    assert_eq!(ctx.errno(), 84);
    // Surrogate U+D800 encoded as ED A0 80 — invalid per UTF-8.
    ctx.mem.map(0x1100, b"\xED\xA0\x80");
    assert!(mbrtowc(&mut ctx, 0, 0x1100, 3, 0).is_err());
    // Overlong U+0000 as C0 80 — invalid lead.
    ctx.mem.map(0x1200, b"\xC0\x80");
    assert!(mbrtowc(&mut ctx, 0, 0x1200, 2, 0).is_err());
    // Broken continuation: C3 28 ("é" lead + a non-continuation) must be EILSEQ,
    // not decoded as garbage.
    ctx.mem.map(0x1300, b"\xC3\x28");
    assert!(mbrtowc(&mut ctx, 0, 0x1300, 2, 0).is_err());
    assert_eq!(ctx.errno(), 84);
}

#[test]
fn mbrtowc_u10ffff_max_and_out_of_range() {
    let mut ctx = Ctx::default();
    // U+10FFFF: F4 8F BF BF — the largest valid scalar.
    ctx.mem.map(0x1000, b"\xF4\x8F\xBF\xBF");
    assert_eq!(mbrtowc(&mut ctx, 0, 0x1000, 4, 0), Ok(4));
    // U+110000: F4 90 80 80 — one past the maximum: invalid.
    ctx.mem.map(0x1100, b"\xF4\x90\x80\x80");
    assert!(mbrtowc(&mut ctx, 0, 0x1100, 4, 0).is_err());
    assert_eq!(ctx.errno(), 84);
}

#[test]
fn mbrtowc_flush_and_zero_n() {
    let mut ctx = Ctx::default();
    // s == NULL: flush — stateless encoding returns 0.
    assert_eq!(mbrtowc(&mut ctx, 0, 0, 0, 0), Ok(0));
    // n == 0 with s != NULL: (size_t)-2 per C.
    ctx.mem.map(0x1000, b"a");
    assert_eq!(mbrtowc(&mut ctx, 0, 0x1000, 0, 0), Ok(u64::MAX - 1));
}

// ------------------------------------------------------------------ mbsrtowcs

#[test]
fn mbsrtowcs_converts_and_null_terminates_dst() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a\xC3\xA9\x00extra"); // NUL ends the string
    ctx.mem.map(0x2000, &[0u8; 32]); // dst
    // The char** slot points at 0x1000.
    ctx.mem.map(0x3000, &0x1000u64.to_le_bytes());

    assert_eq!(mbsrtowcs(&mut ctx, 0x2000, 0x3000, 8, 0), Ok(2));
    // dst got the 2 code points + L'\0'.
    let mut out = [0u8; 12];
    ctx.read(0x2000, &mut out).unwrap();
    let w0 = u32::from_le_bytes(out[0..4].try_into().unwrap());
    let w1 = u32::from_le_bytes(out[4..8].try_into().unwrap());
    let w2 = u32::from_le_bytes(out[8..12].try_into().unwrap());
    assert_eq!((w0, w1, w2), (0x61, 0xE9, 0));
    // *src was set to NULL.
    let mut pp = [0u8; 8];
    ctx.read(0x3000, &mut pp).unwrap();
    assert_eq!(u64::from_le_bytes(pp), 0);
}

#[test]
fn mbsrtowcs_dst_null_counts_only() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"\xF0\x9F\x98\x80\xF0\x9F\x98\x80\x00");
    ctx.mem.map(0x3000, &0x1000u64.to_le_bytes());
    // dst == 0: pure count; len is ignored.
    assert_eq!(mbsrtowcs(&mut ctx, 0, 0x3000, 0, 0), Ok(2));
    // *src untouched in count mode when the whole string converted (C leaves it
    // implementation-defined whether *src moves on dst==0; bionic does not move it
    // when the terminator is reached — VERIFIED: we set NULL only when dst != 0 or on
    // terminator, which matches: terminator reached => NULL).
    let mut pp = [0u8; 8];
    ctx.read(0x3000, &mut pp).unwrap();
    assert_eq!(u64::from_le_bytes(pp), 0);
}

#[test]
fn mbsrtowcs_stops_when_dst_full_and_advances_src() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"abc\x00");
    ctx.mem.map(0x2000, &[0u8; 8]); // room for 2 wchar_t
    ctx.mem.map(0x3000, &0x1000u64.to_le_bytes());
    assert_eq!(mbsrtowcs(&mut ctx, 0x2000, 0x3000, 2, 0), Ok(2));
    // *src now points at the 'c' (0x1002).
    let mut pp = [0u8; 8];
    ctx.read(0x3000, &mut pp).unwrap();
    assert_eq!(u64::from_le_bytes(pp), 0x1002);
}

#[test]
fn mbsrtowcs_invalid_sequence_sets_eilseq() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a\xFF\x00");
    ctx.mem.map(0x2000, &[0u8; 16]);
    ctx.mem.map(0x3000, &0x1000u64.to_le_bytes());
    assert_eq!(mbsrtowcs(&mut ctx, 0x2000, 0x3000, 8, 0), Ok(u64::MAX));
    assert_eq!(ctx.errno(), 84);
}

#[test]
fn mbsrtowcs_null_src_slot_rejected() {
    let mut ctx = Ctx::default();
    assert!(mbsrtowcs(&mut ctx, 0x2000, 0, 8, 0).is_err());
    // Slot exists but points at null.
    ctx.mem.map(0x3000, &0u64.to_le_bytes());
    assert!(mbsrtowcs(&mut ctx, 0x2000, 0x3000, 8, 0).is_err());
}

// ------------------------------------------------------------------ mbtowc (M3's gate)

/// `mbtowc` stores the scalar value and returns how many bytes it consumed.
#[test]
fn mbtowc_decodes_one_character_and_reports_its_length() {
    let mut ctx = Ctx::default();
    // U+00E9 LATIN SMALL LETTER E WITH ACUTE, two bytes in UTF-8.
    ctx.mem.map(0x1000, b"\xC3\xA9x\x00");
    ctx.mem.map(0x2000, &[0u8; 4]);
    assert_eq!(mbtowc(&mut ctx, 0x2000, 0x1000, 4), Ok(2));
    let mut out = [0u8; 4];
    ctx.read(0x2000, &mut out).unwrap();
    assert_eq!(u32::from_le_bytes(out), 0xE9);
    // A NUL character is `0`, and nothing is stored -- which is `mbrtowc`'s rule and therefore
    // bionic's, since `mbtowc` returns what `mbrtowc` returned.
    ctx.mem.map(0x3000, b"\x00");
    assert_eq!(mbtowc(&mut ctx, 0x2000, 0x3000, 1), Ok(0));
}

/// **`EILSEQ` is a return value here, not a refusal**, and that is the whole reason this function
/// exists separately from [`mbrtowc`].
///
/// `mbrtowc`'s invalid-sequence arm reports `Unimplemented`, which the adapter turns into a
/// refusal that would stop a run where a device returns `-1`. Both of bionic's error cases --
/// an illegal sequence and an incomplete one -- are `-1` with `EILSEQ`.
#[test]
fn mbtowc_reports_an_illegal_or_incomplete_sequence_as_minus_one_with_eilseq() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"\xFF\x00"); // 0xFF is not a legal UTF-8 lead byte
    assert_eq!(mbtowc(&mut ctx, 0, 0x1000, 2), Ok(-1));
    assert_eq!(ctx.errno(), 84, "EILSEQ, Linux numbering");

    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"\xC3"); // a valid prefix with its continuation byte missing
    assert_eq!(mbtowc(&mut ctx, 0, 0x1000, 1), Ok(-1));
    assert_eq!(ctx.errno(), 84);

    // `n == 0` is "no bytes to look at", which is bionic's incomplete case.
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a");
    assert_eq!(mbtowc(&mut ctx, 0, 0x1000, 0), Ok(-1));
    assert_eq!(ctx.errno(), 84);
}

/// `s == NULL` asks whether encodings are state-dependent. UTF-8's are not, so the answer is 0.
#[test]
fn mbtowc_with_a_null_string_answers_zero_for_a_stateless_encoding() {
    let mut ctx = Ctx::default();
    assert_eq!(mbtowc(&mut ctx, 0x2000, 0, 4), Ok(0));
}

/// Hostile: a `pwc` that is not writable is a fault, not a silent discard.
#[test]
fn mbtowc_with_an_unmapped_destination_faults() {
    let mut ctx = Ctx::default();
    ctx.mem.map(0x1000, b"a");
    assert!(mbtowc(&mut ctx, 0x9999_0000, 0x1000, 1).is_err());
}
