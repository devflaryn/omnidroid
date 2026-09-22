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

/// `int wcscmp(const wchar_t *s1, const wchar_t *s2)` -- FreeBSD's, which bionic builds: the
/// first differing pair's difference **as `unsigned int`**, converted to `int` ("XXX assumes
/// wchar_t = int"). bionic's `wcscoll` and `wcscoll_l` are this.
///
/// Errors: `Err(Fault)` on unmapped memory or a null pointer.
pub fn wcscmp(mem: &impl GuestMemory, s1: u64, s2: u64) -> Result<i32, Fault> {
    if s1 == 0 || s2 == 0 {
        return Err(Fault(0));
    }
    let mut at = 0u64;
    loop {
        let a = read_wc(mem, s1.wrapping_add(at))?;
        let b = read_wc(mem, s2.wrapping_add(at))?;
        if a != b {
            return Ok(a.wrapping_sub(b) as i32);
        }
        if a == 0 {
            return Ok(0);
        }
        at += 4;
    }
}

/// `size_t wcslcpy(wchar_t *dst, const wchar_t *src, size_t dsize)` -- OpenBSD's: [`wcslen`]
/// of `src`, having copied up to `dsize - 1` characters and a terminating `L'\0'` when
/// `dsize != 0`.
///
/// Errors: `Err(Fault)` on unmapped memory or a null `src`.
pub fn wcslcpy(mem: &mut impl GuestMemory, dst: u64, src: u64, dsize: u64) -> Result<u64, Fault> {
    let len = wcslen(mem, src)?;
    if dsize != 0 {
        let copy = len.min(dsize - 1);
        let bytes = usize::try_from(copy * 4).map_err(|_| Fault(src))?;
        let mut buf = vec![0u8; bytes];
        mem.read(src, &mut buf)?;
        buf.extend_from_slice(&0u32.to_le_bytes());
        mem.write(dst, &buf)?;
    }
    Ok(len)
}

/// `size_t wcsxfrm(wchar_t *dst, const wchar_t *src, size_t n)` -- OpenBSD's, which bionic builds:
/// [`wcslen`] for `n == 0`, else [`wcslcpy`]. bionic's `wcsxfrm_l` is this.
///
/// Errors: as [`wcslcpy`].
pub fn wcsxfrm(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    if n == 0 {
        return wcslen(mem, src);
    }
    wcslcpy(mem, dst, src, n)
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

/// `sizeof(mbstate_t)` on LP64 bionic: `unsigned char __seq[4]` and four reserved bytes.
pub const MBSTATE_BYTES: usize = 8;

/// `size_t mbrtoc32(char32_t *pc32, const char *s, size_t n, mbstate_t *ps)` -- **bionic's own
/// algorithm, ported**, with the conversion state kept where bionic keeps it: in the guest's
/// `mbstate_t` at `state` (the caller resolves a NULL `ps` to its private one, as bionic's
/// `static mbstate_t __private_state` is). bionic's `mbrtowc` is exactly this, because its
/// `wchar_t` is UTF-32.
///
/// Ported from `libc/bionic/mbrtoc32.cpp` at `android-13.0.0_r1` -- the platform this host
/// reports (SDK 33) -- and `private/bionic_mbstate.h`:
///
/// * the state's `__seq[3]` set is `EINVAL`, returning `(size_t)-1`;
/// * `s == NULL` is `s = "", n = 1, pc32 = NULL`;
/// * `n == 0` returns `0` (Android 13's reading; later bionic returns `(size_t)-2`);
/// * an initial state and an ASCII byte is the fast path: store it, return 1 (0 for NUL);
/// * otherwise the lead byte (the state's first byte, if a sequence is in progress) gives the
///   length; the bytes still wanted are appended to the state, a non-continuation byte in the
///   middle being `EILSEQ`; too few bytes is `(size_t)-2` **with the state kept**, so the next call
///   finishes the character;
/// * a complete sequence decodes, overlong forms, surrogates and anything above U+10FFFF are
///   `EILSEQ`, and the return is the number of bytes **this call** consumed (0 for NUL).
///
/// Every error path resets the state, as bionic's `mbstate_reset_and_return_illegal` does. The
/// return is the `size_t` bionic returns, `(size_t)-1` and `(size_t)-2` included: those are
/// answers a caller branches on, not refusals.
///
/// # Errors
///
/// Only a guest memory [`Fault`] (reading `s` or the state, writing `pc32` or the state).
pub fn mbrtoc32(
    ctx: &mut impl crate::context::GuestContext,
    pc32: u64,
    s: u64,
    n: u64,
    state: u64,
) -> Result<u64, Fault> {
    const ILLEGAL: u64 = u64::MAX;
    const INCOMPLETE: u64 = u64::MAX - 1;
    fn reset(ctx: &mut impl crate::context::GuestContext, state: u64) -> Result<(), Fault> {
        ctx.write(state, &[0u8; 4])
    }
    let mut seq = [0u8; 4];
    ctx.read(state, &mut seq)?;
    if seq[3] != 0 {
        ctx.set_errno(crate::errno::consts::EINVAL);
        reset(ctx, state)?;
        return Ok(ILLEGAL);
    }
    let empty = s == 0;
    let (mut s, mut n, mut pc32) = (s, n, pc32);
    if empty {
        n = 1;
        pc32 = 0;
    }
    if n == 0 {
        return Ok(0);
    }
    // `s == NULL` reads as "", whose only byte is NUL.
    fn read_at(
        ctx: &mut impl crate::context::GuestContext,
        empty: bool,
        at: u64,
    ) -> Result<u8, Fault> {
        if empty {
            return Ok(0);
        }
        let mut byte = [0u8; 1];
        ctx.read(at, &mut byte)?;
        Ok(byte[0])
    }
    let initial = |seq: &[u8; 4]| seq == &[0u8; 4];
    let first = read_at(ctx, empty, s)?;
    if initial(&seq) && first & !0x7f == 0 {
        if pc32 != 0 {
            ctx.write(pc32, &u32::from(first).to_le_bytes())?;
        }
        return Ok(u64::from(first != 0));
    }
    let bytes_so_far = if seq[2] != 0 {
        3
    } else if seq[1] != 0 {
        2
    } else if seq[0] != 0 {
        1
    } else {
        0
    };
    let lead = if bytes_so_far > 0 { seq[0] } else { first };
    let (mask, length, lower_bound): (u8, usize, u32) = if lead & 0xe0 == 0xc0 {
        (0x1f, 2, 0x80)
    } else if lead & 0xf0 == 0xe0 {
        (0x0f, 3, 0x800)
    } else if lead & 0xf8 == 0xf0 {
        (0x07, 4, 0x10000)
    } else {
        ctx.set_errno(EILSEQ);
        reset(ctx, state)?;
        return Ok(ILLEGAL);
    };
    let bytes_wanted = length - bytes_so_far;
    let mut i = 0usize;
    while (i as u64) < n.min(bytes_wanted as u64) {
        let byte = read_at(ctx, empty, s)?;
        if !initial(&seq) && byte & 0xc0 != 0x80 {
            ctx.set_errno(EILSEQ);
            reset(ctx, state)?;
            return Ok(ILLEGAL);
        }
        seq[bytes_so_far + i] = byte;
        ctx.write(state + (bytes_so_far + i) as u64, &[byte])?;
        s = s.wrapping_add(1);
        i += 1;
    }
    if i < bytes_wanted {
        return Ok(INCOMPLETE);
    }
    let mut c32 = u32::from(seq[0] & mask);
    for byte in &seq[1..length] {
        c32 = (c32 << 6) | u32::from(byte & 0x3f);
    }
    if c32 < lower_bound || (0xd800..=0xdfff).contains(&c32) || c32 > 0x10ffff {
        ctx.set_errno(EILSEQ);
        reset(ctx, state)?;
        return Ok(ILLEGAL);
    }
    if pc32 != 0 {
        ctx.write(pc32, &c32.to_le_bytes())?;
    }
    reset(ctx, state)?;
    Ok(if c32 == 0 { 0 } else { bytes_wanted as u64 })
}

/// `(size_t)-1`: bionic's `BIONIC_MULTIBYTE_RESULT_ILLEGAL_SEQUENCE`.
pub const MB_ILLEGAL: u64 = u64::MAX;
/// `(size_t)-2`: bionic's `BIONIC_MULTIBYTE_RESULT_INCOMPLETE_SEQUENCE`.
pub const MB_INCOMPLETE_SEQUENCE: u64 = u64::MAX - 1;
/// bionic's `MB_LEN_MAX` (`limits.h`): the longest UTF-8 sequence.
const MB_LEN_MAX: u64 = 4;
/// `WEOF`.
pub const WEOF: u32 = 0xFFFF_FFFF;

fn read_byte(ctx: &mut impl crate::context::GuestContext, at: u64) -> Result<u8, Fault> {
    let mut byte = [0u8; 1];
    ctx.read(at, &mut byte)?;
    Ok(byte[0])
}

fn read_word(ctx: &mut impl crate::context::GuestContext, at: u64) -> Result<u64, Fault> {
    let mut bytes = [0u8; 8];
    ctx.read(at, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_wchar(ctx: &mut impl crate::context::GuestContext, at: u64) -> Result<u32, Fault> {
    let mut bytes = [0u8; 4];
    ctx.read(at, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn state_is_initial(ctx: &mut impl crate::context::GuestContext, state: u64) -> Result<bool, Fault> {
    let mut seq = [0u8; 4];
    ctx.read(state, &mut seq)?;
    Ok(seq == [0; 4])
}

fn state_bytes_so_far(ctx: &mut impl crate::context::GuestContext, state: u64) -> Result<u64, Fault> {
    let mut seq = [0u8; 4];
    ctx.read(state, &mut seq)?;
    Ok(if seq[2] != 0 {
        3
    } else if seq[1] != 0 {
        2
    } else {
        u64::from(seq[0] != 0)
    })
}

/// `mbstate_reset_and_return_illegal(EILSEQ, state)`.
fn illegal(ctx: &mut impl crate::context::GuestContext, state: u64) -> Result<u64, Fault> {
    ctx.set_errno(EILSEQ);
    ctx.write(state, &[0u8; 4])?;
    Ok(MB_ILLEGAL)
}

/// `mbstate_reset_and_return(value, state)`.
fn reset_and(ctx: &mut impl crate::context::GuestContext, state: u64, value: u64) -> Result<u64, Fault> {
    ctx.write(state, &[0u8; 4])?;
    Ok(value)
}

/// The UTF-8 bytes `c32rtomb` writes for a non-ASCII `c32`, or `None` past `0x1FFFFF` -- bionic's
/// ranges, which (unlike `mbrtoc32`'s) do not exclude surrogates or `0x110000..=0x1FFFFF`.
fn c32_encode(c32: u32) -> Option<([u8; 4], usize)> {
    let (lead, length) = if c32 & !0x7ff == 0 {
        (0xc0u8, 2)
    } else if c32 & !0xffff == 0 {
        (0xe0, 3)
    } else if c32 & !0x1f_ffff == 0 {
        (0xf0, 4)
    } else {
        return None;
    };
    let mut out = [0u8; 4];
    let mut rest = c32;
    for i in (1..length).rev() {
        out[i] = (rest & 0x3f) as u8 | 0x80;
        rest >>= 6;
    }
    out[0] = (rest & 0xff) as u8 | lead;
    Some((out, length))
}

/// `size_t c32rtomb(char *s, char32_t c32, mbstate_t *ps)` -- bionic's, ported from
/// `android-13.0.0_r1`, over the guest's state at `state`: `s == NULL` resets and returns 1; a NUL
/// is stored and returns 1; a non-initial state is `EILSEQ`; ASCII is one byte; otherwise the
/// shortest UTF-8 for anything up to `0x1FFFFF`, and `EILSEQ` past it. bionic's `wcrtomb` is this.
///
/// # Errors
///
/// Only a guest memory [`Fault`].
pub fn c32rtomb(
    ctx: &mut impl crate::context::GuestContext,
    s: u64,
    c32: u32,
    state: u64,
) -> Result<u64, Fault> {
    if s == 0 {
        return reset_and(ctx, state, 1);
    }
    if c32 == 0 {
        ctx.write(s, &[0])?;
        return reset_and(ctx, state, 1);
    }
    if !state_is_initial(ctx, state)? {
        return illegal(ctx, state);
    }
    if c32 & !0x7f == 0 {
        ctx.write(s, &[c32 as u8])?;
        return Ok(1);
    }
    let Some((bytes, length)) = c32_encode(c32) else {
        ctx.set_errno(EILSEQ);
        return Ok(MB_ILLEGAL);
    };
    ctx.write(s, &bytes[..length])?;
    Ok(length as u64)
}

/// `size_t mbsnrtowcs(wchar_t *dst, const char **src, size_t nmc, size_t len, mbstate_t *ps)` --
/// bionic's (`libc/bionic/wchar.cpp`, `android-13.0.0_r1`), ported line for line over guest
/// memory: `src` is the guest address of the `const char *`, which is read and written back as
/// bionic's is; every multibyte character goes through [`mbrtoc32`] with the same `state`. A NULL
/// `dst` measures. bionic's `mbsrtowcs` is this with `nmc = SIZE_MAX`.
///
/// # Errors
///
/// Only a guest memory [`Fault`].
pub fn mbsnrtowcs(
    ctx: &mut impl crate::context::GuestContext,
    dst: u64,
    src: u64,
    nmc: u64,
    len: u64,
    state: u64,
) -> Result<u64, Fault> {
    let s = read_word(ctx, src)?;
    if nmc > 0 && state_bytes_so_far(ctx, state)? > 0 && read_byte(ctx, s)? < 0x80 {
        return illegal(ctx, state);
    }
    let (mut i, mut o) = (0u64, 0u64);
    if dst == 0 {
        while i < nmc {
            let byte = read_byte(ctx, s.wrapping_add(i))?;
            let r = if byte < 0x80 {
                if byte == 0 {
                    return reset_and(ctx, state, o);
                }
                1
            } else {
                let r = mbrtoc32(ctx, 0, s.wrapping_add(i), nmc - i, state)?;
                if r == MB_ILLEGAL || r == MB_INCOMPLETE_SEQUENCE {
                    return illegal(ctx, state);
                }
                if r == 0 {
                    return reset_and(ctx, state, o);
                }
                r
            };
            i += r;
            o += 1;
        }
        return reset_and(ctx, state, o);
    }
    while i < nmc && o < len {
        let byte = read_byte(ctx, s.wrapping_add(i))?;
        let at = dst.wrapping_add(4 * o);
        let r = if byte < 0x80 {
            ctx.write(at, &u32::from(byte).to_le_bytes())?;
            if byte == 0 {
                ctx.write(src, &0u64.to_le_bytes())?;
                return reset_and(ctx, state, o);
            }
            1
        } else {
            let r = mbrtoc32(ctx, at, s.wrapping_add(i), nmc - i, state)?;
            if r == MB_ILLEGAL {
                ctx.write(src, &s.wrapping_add(i).to_le_bytes())?;
                return illegal(ctx, state);
            }
            if r == MB_INCOMPLETE_SEQUENCE {
                ctx.write(src, &s.wrapping_add(nmc).to_le_bytes())?;
                return illegal(ctx, state);
            }
            if r == 0 {
                ctx.write(src, &0u64.to_le_bytes())?;
                return reset_and(ctx, state, o);
            }
            r
        };
        i += r;
        o += 1;
    }
    ctx.write(src, &s.wrapping_add(i).to_le_bytes())?;
    reset_and(ctx, state, o)
}

/// `size_t wcsnrtombs(char *dst, const wchar_t **src, size_t nwc, size_t len, mbstate_t *ps)` --
/// bionic's, ported as [`mbsnrtowcs`] is: a non-initial state is `EILSEQ`; a NULL `dst` measures;
/// a character that might not fit the `len - o` bytes left is encoded aside and the loop stops
/// when it does not fit; `*src` is advanced by the characters consumed, or set to NULL at the
/// terminating NUL. bionic's `wcsrtombs` is this with `nwc = SIZE_MAX`.
///
/// # Errors
///
/// Only a guest memory [`Fault`].
pub fn wcsnrtombs(
    ctx: &mut impl crate::context::GuestContext,
    dst: u64,
    src: u64,
    nwc: u64,
    len: u64,
    state: u64,
) -> Result<u64, Fault> {
    if !state_is_initial(ctx, state)? {
        return illegal(ctx, state);
    }
    let s = read_word(ctx, src)?;
    let (mut i, mut o) = (0u64, 0u64);
    if dst == 0 {
        while i < nwc {
            let wc = read_wchar(ctx, s.wrapping_add(4 * i))?;
            let r = if wc < 0x80 {
                if wc == 0 {
                    return Ok(o);
                }
                1
            } else {
                match c32_encode(wc) {
                    Some((_, length)) => length as u64,
                    None => {
                        ctx.set_errno(EILSEQ);
                        return Ok(MB_ILLEGAL);
                    }
                }
            };
            i += 1;
            o += r;
        }
        return Ok(o);
    }
    while i < nwc && o < len {
        let wc = read_wchar(ctx, s.wrapping_add(4 * i))?;
        let at = dst.wrapping_add(o);
        let r = if wc < 0x80 {
            ctx.write(at, &[wc as u8])?;
            if wc == 0 {
                ctx.write(src, &0u64.to_le_bytes())?;
                return Ok(o);
            }
            1
        } else {
            let Some((bytes, length)) = c32_encode(wc) else {
                ctx.set_errno(EILSEQ);
                ctx.write(src, &s.wrapping_add(4 * i).to_le_bytes())?;
                return Ok(MB_ILLEGAL);
            };
            let length = length as u64;
            if len - o < MB_LEN_MAX && length > len - o {
                break;
            }
            ctx.write(at, &bytes[..length as usize])?;
            length
        };
        i += 1;
        o += r;
    }
    ctx.write(src, &s.wrapping_add(4 * i).to_le_bytes())?;
    Ok(o)
}

/// `wint_t btowc(int c)` -- bionic's (OpenBSD's `btowc.c`): `EOF` is `WEOF`; otherwise the byte
/// `(char)c` through `mbrtowc` with a fresh state, and anything but a one-byte answer is `WEOF`.
/// In UTF-8 that is: the byte itself below `0x80`, `WEOF` from `0x80` up.
#[must_use]
pub const fn btowc(c: i32) -> u32 {
    if c == -1 {
        return WEOF;
    }
    let byte = c as u8;
    if byte < 0x80 {
        byte as u32
    } else {
        WEOF
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
