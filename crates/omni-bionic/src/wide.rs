//! Wide-character and multibyte functions, for the **Android arm64 ABI**: `wchar_t` is
//! 32-bit (UTF-32 on the wire for the C locale), `wint_t` is `u32`, and the multibyte
//! encoding in the C/POSIX locale is UTF-8 with `MB_CUR_MAX == 4` (see `locale.rs` for the
//! `__ctype_get_mb_cur_max` value and its bionic justification).
//!
//! Signatures implemented:
//!
//! | C signature | here |
//! |---|---|
//! | `size_t wcslen(const wchar_t *s)` | [`wcslen`] |
//! | `wchar_t *wmemchr(const wchar_t *s, wchar_t c, size_t n)` | [`wmemchr`] |
//! | `int wmemcmp(const wchar_t *a, const wchar_t *b, size_t n)` | [`wmemcmp`] |
//! | `int wctob(wint_t c)` | [`wctob`] |
//! | `size_t mbrtowc(wchar_t *pwc, const char *s, size_t n, mbstate_t *ps)` | [`mbrtowc`] |
//! | `int mbtowc(wchar_t *pwc, const char *s, size_t n)` | [`mbtowc`] |
//! | `size_t mbsrtowcs(wchar_t *dst, const char **src, size_t len, mbstate_t *ps)` | [`mbsrtowcs`] |
//!
//! All are the C/POSIX-locale forms (no `_l` variants are reachable). Element size for
//! `wchar_t*` functions is **4 bytes everywhere**, including `wmemcmp` sign semantics
//! (unsigned comparison — `wchar_t` is unsigned on arm64).
//!
//! `mbstate_t`: bionic's `mbstate_t` is 4 bytes of opaque state; the C/POSIX locale has no
//! encoding states to track (UTF-8 is stateless), so any state word of zero counts as the
//! initial state and no function here ever produces a non-initial state. A non-initial
//! state fed to [`mbrtowc`] returns the C-standard `(size_t)-3` "assume a previous byte"
//! result is NOT synthesised — this crate reports `Unimplemented` instead, because
//! reconstructing bionic's internal state layout would be guessing (rule: no plausible
//! stubs).

use crate::error::BionicError;
use crate::memory::{checked_range, Fault, GuestMemory};

/// `size_t` value bionic's `mbrtowc`/`mbsrtowcs` use for errors (C standard `(size_t)-1`).
const MB_ERR: u64 = u64::MAX;
/// C standard `(size_t)-2`: incomplete multibyte sequence.
const MB_INCOMPLETE: u64 = u64::MAX - 1;
/// `EILSEQ`, 84 in Linux numbering (kernel UAPI; VERIFIED, and the same source the rest of
/// [`crate::errno`] comes from).
const EILSEQ: i32 = 84;

/// Read one 32-bit little-endian `wchar_t` at `addr`.
fn read_wc(mem: &impl GuestMemory, addr: u64) -> Result<u32, Fault> {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

/// `size_t wcslen(const wchar_t *s)` — number of `wchar_t` elements before the L'\0'.
///
/// Each iteration reads 4 bytes; termination at the NUL element or a fault.
pub fn wcslen(mem: &impl GuestMemory, s: u64) -> Result<u64, Fault> {
    if s == 0 {
        return Err(Fault(0));
    }
    let mut cursor = s;
    let mut count = 0u64;
    loop {
        let wc = read_wc(mem, cursor)?;
        if wc == 0 {
            return Ok(count);
        }
        count += 1;
        cursor = cursor.checked_add(4).ok_or(Fault(cursor))?;
    }
}

/// `wchar_t *wmemchr(const wchar_t *s, wchar_t c, size_t n)` — address of the first
/// element equal to `c` among `n` elements, or guest `0`.
pub fn wmemchr(mem: &impl GuestMemory, s: u64, c: u32, n: u64) -> Result<u64, Fault> {
    // n * 4 must not overflow; the byte range then must be representable.
    let nbytes = n.checked_mul(4).ok_or(Fault(s))?;
    let (s, _nbytes) = checked_range(s, nbytes)?;
    let mut cursor = s;
    for _ in 0..n {
        if read_wc(mem, cursor)? == c {
            return Ok(cursor);
        }
        cursor += 4;
    }
    Ok(0)
}

/// `int wmemcmp(const wchar_t *a, const wchar_t *b, size_t n)` — sign-only comparison of
/// `n` elements as `unsigned` 32-bit values (conventional `-1/0/1` returned).
pub fn wmemcmp(mem: &impl GuestMemory, a: u64, b: u64, n: u64) -> Result<i32, Fault> {
    // Element counts stay counts: the byte ranges are validated separately.
    let abytes = n.checked_mul(4).ok_or(Fault(a))?;
    let bbytes = n.checked_mul(4).ok_or(Fault(b))?;
    let (a, _) = checked_range(a, abytes)?;
    let (b, _) = checked_range(b, bbytes)?;
    for i in 0..n {
        let wa = read_wc(mem, a + i * 4)?;
        let wb = read_wc(mem, b + i * 4)?;
        if wa != wb {
            return Ok(if wa < wb { -1 } else { 1 });
        }
    }
    Ok(0)
}

/// `int wctob(wint_t c)` — the single-byte form of `c` if `c` is one, else `EOF` (`-1`).
///
/// C/POSIX locale (UTF-8): exactly the code points `0..=0x7F` are single-byte.
pub fn wctob(c: u32) -> i32 {
    if c <= 0x7F {
        c as i32
    } else {
        -1
    }
}

/// UTF-8 length from a leading byte, per RFC 3629 / the Unicode standard.
/// Returns `None` for invalid leading bytes (`0x80..=0xC1`, `0xF8..=0xFF`).
fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

/// Outcome of one UTF-8 decode attempt.
enum Decode {
    /// A complete, valid scalar value and the bytes it consumed.
    Char(u32, u64),
    /// The bytes are structurally invalid (bad lead, bad continuation, overlong,
    /// surrogate, or out of range): caller sets EILSEQ.
    Invalid,
    /// A valid prefix but not enough bytes to complete the character.
    Incomplete,
}

/// Decode one UTF-8 sequence starting at `s`, reading at most `avail` bytes.
fn utf8_decode(mem: &impl GuestMemory, s: u64, avail: u64) -> Result<Decode, Fault> {
    let mut lead = [0u8; 1];
    mem.read(s, &mut lead)?;
    let len = match utf8_len(lead[0]) {
        Some(l) => l,
        None => return Ok(Decode::Invalid),
    };
    if avail < len as u64 {
        return Ok(Decode::Incomplete);
    }
    let mut buf = [0u8; 4];
    mem.read(s, &mut buf[..len])?;
    // Continuation-byte validation and range checks (overlongs, surrogates, > U+10FFFF).
    let cp = match len {
        1 => buf[0] as u32,
        2 => {
            if buf[1] & 0xC0 != 0x80 {
                return Ok(Decode::Invalid);
            }
            ((buf[0] as u32 & 0x1F) << 6) | (buf[1] as u32 & 0x3F)
        }
        3 => {
            if buf[1] & 0xC0 != 0x80 || buf[2] & 0xC0 != 0x80 {
                return Ok(Decode::Invalid);
            }
            let cp =
                ((buf[0] as u32 & 0x0F) << 12) | ((buf[1] as u32 & 0x3F) << 6) | (buf[2] as u32 & 0x3F);
            if cp < 0x800 || (0xD800..=0xDFFF).contains(&cp) {
                return Ok(Decode::Invalid);
            }
            cp
        }
        _ => {
            if buf[1] & 0xC0 != 0x80 || buf[2] & 0xC0 != 0x80 || buf[3] & 0xC0 != 0x80 {
                return Ok(Decode::Invalid);
            }
            let cp = ((buf[0] as u32 & 0x07) << 18)
                | ((buf[1] as u32 & 0x3F) << 12)
                | ((buf[2] as u32 & 0x3F) << 6)
                | (buf[3] as u32 & 0x3F);
            if !(0x10000..=0x10FFFF).contains(&cp) {
                return Ok(Decode::Invalid);
            }
            cp
        }
    };
    Ok(Decode::Char(cp, len as u64))
}

/// `size_t mbrtowc(wchar_t *pwc, const char *s, size_t n, mbstate_t *ps)`
///
/// C/POSIX locale, UTF-8. Returns:
/// * `0` when the byte(s) read encode the NUL character (and `L'\0'` is stored to `pwc`);
/// * the number of bytes consumed (1..=4) when a complete character is stored to `pwc`;
/// * `(size_t)-2` when the `n` bytes hold an incomplete-but-valid prefix;
/// * `(size_t)-1` for an invalid sequence, **with `errno = EILSEQ`** — EILSEQ is 84 in
///   Linux numbering (kernel UAPI; VERIFIED) — set through the context.
///
/// `s == NULL` (a flush request) with a stateless encoding is a no-op returning `0`
/// (C standard: "if s is a null pointer, the mbstate_t is reset"); a non-initial state is
/// rejected as [`BionicError::Unimplemented`] rather than guessed (module docs).
pub fn mbrtowc(
    ctx: &mut impl crate::context::GuestContext,
    pwc: u64,
    s: u64,
    n: u64,
    _ps: u64,
) -> Result<u64, crate::error::BionicError> {
    if s == 0 {
        return Ok(0); // stateless encoding: nothing to flush
    }
    if n == 0 {
        // C standard: when s is not null and n is 0, the call "is affected by" nothing and
        // returns (size_t)-2 unless the state is initial — our state is always initial, so
        // bionic returns 0 here only for the flush case; a 0-byte decode attempt reports
        // incomplete.
        return Ok(MB_INCOMPLETE);
    }
    let (s, n) = checked_range(s, n)?;
    match utf8_decode(ctx, s, n)? {
        Decode::Invalid => {
            ctx.set_errno(EILSEQ);
            Err(BionicError::Unimplemented("mbrtowc: EILSEQ"))
        }
        Decode::Incomplete => Ok(MB_INCOMPLETE),
        Decode::Char(cp, consumed) => {
            if cp == 0 {
                return Ok(0); // NUL character: nothing stored, return 0 (C standard)
            }
            if pwc != 0 {
                // Store the 32-bit wchar_t (guest is little-endian arm64).
                ctx.write(pwc, &cp.to_le_bytes())?;
            }
            Ok(consumed)
        }
    }
}

/// `int mbtowc(wchar_t *pwc, const char *s, size_t n)`
///
/// The non-restartable form. bionic implements it as `mbrtowc` over a private `mbstate_t`, and
/// folds both of that function's error returns into `-1` with `EILSEQ`:
///
/// ```c
/// rval = mbrtowc(pwc, s, n, &mbs);
/// if (rval == __MB_ERR_ILLEGAL_SEQUENCE || rval == __MB_ERR_INCOMPLETE_SEQUENCE) {
///   memset(&mbs, 0, sizeof(mbs)); errno = EILSEQ; return -1;
/// }
/// return rval;
/// ```
///
/// **The private state is nothing here and that is a fact rather than a simplification**: the
/// C/POSIX locale's encoding is UTF-8, which is stateless, so bionic's `mbs` is always in its
/// initial state between calls and there is nothing for a caller to observe. That is the same
/// argument [`mbrtowc`]'s `ps` rests on.
///
/// `s == NULL` asks whether encodings are state-dependent. They are not, so the answer is `0`.
///
/// # Errors
///
/// Only a guest memory [`Fault`]. `EILSEQ` is a *return value* the caller branches on, not a
/// refusal -- which is the difference between this and [`mbrtowc`], whose invalid-sequence arm
/// reports `Unimplemented` and would abort an M3 run where a device returns `-1`.
pub fn mbtowc(
    ctx: &mut impl crate::context::GuestContext,
    pwc: u64,
    s: u64,
    n: u64,
) -> Result<i32, Fault> {
    if s == 0 {
        return Ok(0); // UTF-8 has no shift states
    }
    if n == 0 {
        // No bytes to look at is bionic's "incomplete", which this form reports as -1/EILSEQ.
        ctx.set_errno(EILSEQ);
        return Ok(-1);
    }
    let (start, avail) = checked_range(s, n)?;
    match utf8_decode(ctx, start, avail)? {
        Decode::Invalid | Decode::Incomplete => {
            ctx.set_errno(EILSEQ);
            Ok(-1)
        }
        Decode::Char(cp, consumed) => {
            if cp == 0 {
                // A NUL character: C says store it and return 0. bionic returns `mbrtowc`'s 0
                // and `mbrtowc` stores nothing, so nothing is stored here either.
                return Ok(0);
            }
            if pwc != 0 {
                ctx.write(pwc, &cp.to_le_bytes())?;
            }
            // `consumed` is 1..=4, so the narrowing cannot lose anything.
            Ok(consumed as i32)
        }
    }
}

/// `size_t mbsrtowcs(wchar_t *dst, const char **src, size_t len, mbstate_t *ps)`
///
/// Convert a NUL-terminated multibyte string *pointed to by a guest pointer to a pointer*
/// into `len` wide elements at `dst` (or count them when `dst == 0`), advancing the guest
/// `*src` past what was consumed. Returns the number of elements written/counted, or
/// `(size_t)-1` with `errno = EILSEQ` on an invalid sequence.
///
/// Semantics per C: `dst` may be null (pure length measurement); when a NUL is reached the
/// guest `*src` is set to null and conversion stops. Invalid input: `(size_t)-1`, EILSEQ.
pub fn mbsrtowcs(
    ctx: &mut impl crate::context::GuestContext,
    dst: u64,
    src: u64,
    len: u64,
    _ps: u64,
) -> Result<u64, crate::error::BionicError> {
    if src == 0 {
        return Err(BionicError::InvalidArgument("mbsrtowcs"));
    }
    // Read the guest char** slot.
    let mut pp = [0u8; 8];
    ctx.read(src, &mut pp)?;
    let mut cursor = u64::from_le_bytes(pp);
    if cursor == 0 {
        return Err(BionicError::InvalidArgument("mbsrtowcs"));
    }

    let count_to = if dst == 0 { u64::MAX } else { len };
    let mut written = 0u64;
    let mut buf8 = [0u8; 1];
    loop {
        if written == count_to {
            // Out of destination room: *src is left pointing at the next unconverted byte.
            ctx.write(src, &cursor.to_le_bytes())?;
            return Ok(written);
        }
        // Peek the lead byte to terminate on NUL before decoding.
        ctx.read(cursor, &mut buf8)?;
        if buf8[0] == 0 {
            if dst != 0 {
                ctx.write(dst + written * 4, &0u32.to_le_bytes())?;
            }
            // *src := NULL
            ctx.write(src, &0u64.to_le_bytes())?;
            return Ok(written);
        }
        match utf8_decode(ctx, cursor, u64::MAX)? {
            Decode::Invalid => {
                ctx.set_errno(84); // EILSEQ
                return Ok(MB_ERR);
            }
            Decode::Incomplete => {
                // The terminator scan proves every byte up to the NUL is mapped, so an
                // incomplete sequence inside a NUL-terminated string cannot happen; if it
                // somehow does, report EILSEQ like an invalid sequence.
                ctx.set_errno(84);
                return Ok(MB_ERR);
            }
            Decode::Char(cp, consumed) => {
                if dst != 0 {
                    ctx.write(dst + written * 4, &cp.to_le_bytes())?;
                }
                written += 1;
                cursor += consumed;
            }
        }
    }
}
