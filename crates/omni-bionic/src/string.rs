//! String functions: the reachable `str*` family, the FORTIFY `_chk` variants, and error
//! strings. Wide/multibyte functions live in [`crate::wide`], locale handles in
//! [`crate::locale`].
//!
//! Signatures implemented (guest ABI, LP64 — `size_t` = u64):
//!
//! | C signature | here |
//! |---|---|
//! | `size_t strlen(const char *s)` | [`strlen`] |
//! | `size_t __strlen_chk(const char *s, size_t size)` | [`strlen_chk`] |
//! | `size_t strnlen(const char *s, size_t n)` | [`strnlen`] |
//! | `int strcmp(const char *a, const char *b)` | [`strcmp`] |
//! | `int strncmp(const char *a, const char *b, size_t n)` | [`strncmp`] |
//! | `char *strcpy(char *dst, const char *src)` | [`strcpy`] |
//! | `char *strncpy(char *dst, const char *src, size_t n)` | [`strncpy`] |
//! | `char *__strncpy_chk(char *dst, const char *src, size_t n, size_t dst_size)` | [`strncpy_chk`] |
//! | `char *__strncpy_chk2(char *dst, const char *src, size_t n, size_t dst_size, size_t src_size)` | [`strncpy_chk2`] |
//! | `char *strcat(char *dst, const char *src)` | [`strcat`] |
//! | `char *strncat(char *dst, const char *src, size_t n)` | [`strncat`] |
//! | `char *__strcat_chk(char *dst, const char *src, size_t dst_size)` | [`strcat_chk`] |
//! | `char *strchr(const char *s, int c)` | [`strchr`] |
//! | `char *strrchr(const char *s, int c)` | [`strrchr`] |
//! | `char *strstr(const char *h, const char *n)` | [`strstr`] |
//! | `int strcasecmp(const char *a, const char *b)` | [`strcasecmp`] |
//! | `int strncasecmp(const char *a, const char *b, size_t n)` | [`strncasecmp`] |
//! | `size_t strspn(const char *s, const char *accept)` | [`strspn`] |
//! | `size_t strcspn(const char *s, const char *reject)` | [`strcspn`] |
//! | `char *__gnu_strerror_r(int err, char *buf, size_t len)` | [`gnu_strerror_r`] |
//! | `int strerror_r(int err, char *buf, size_t len)` | [`strerror_r`] |
//!
//! Bionic-specific `_chk` contracts (VERIFIED against bionic's `include/string.h` +
//! `upstream-openbsd/bionic/*.c` behaviour as of the NDK the engine targets):
//! * `__strlen_chk(s, size)`: returns `strlen(s)`; if `strlen(s) >= size`, abort on device —
//!   here [`crate::error::BionicError::CheckFailed`]. (`>=`, not `>`: bionic passes the
//!   object size, and a string filling the whole object with no room for NUL is the bug.)
//! * `__strncpy_chk2(dst, src, n, dst_size, src_size)`: bionic's two-source-size variant —
//!   aborts if `n > dst_size`, and aborts on the **source** side only if the copy would really
//!   read `src[src_size]`, i.e. there is no NUL in the first `min(n, src_size)` bytes. `n >
//!   src_size` alone is NOT a failure: that is `strncpy(dst, src, sizeof dst)` with a shorter
//!   source, which bionic copies and NUL-pads.
//! * `__strcat_chk(dst, src, dst_size)`: aborts if the combined length would exceed
//!   `dst_size` (i.e. `strlen(dst) + strlen(src) + 1 > dst_size` — the `+1` for NUL is
//!   bionic's documented intent).
//!
//! Scanning rule (task rule 4): every loop below reads at least one byte per iteration
//! through the trait, so each terminates at a NUL or a `Fault`; no unbounded scan exists.
//! String bounds are validated with [`crate::memory::checked_range`] from the terminator's
//! address **before any write**, so `strcpy`/`strcat`/`strncpy` either complete or fault
//! without a partial result. Length computations themselves are capped by a `NUL` never
//! being findable past a region end: an unterminated string faults at the region boundary
//! (guest address space, unlike the host heap, is bounded by mapping).

use crate::ctype::to_lower_ascii;
use crate::memory::{checked_range, Fault, GuestMemory};

// ---------------------------------------------------------------------------
// Bounded-length read helper and the mapping probe.
// ---------------------------------------------------------------------------

/// Read `len` bytes at `addr` into a fresh `Vec`, chunked (a huge length must not need a
/// huge host allocation before it starts). The range is caller-validated.
fn read_vec(mem: &impl GuestMemory, addr: u64, len: u64) -> Result<Vec<u8>, Fault> {
    let mut out = Vec::with_capacity(len.min(4096) as usize);
    let mut buf = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        let chunk = (len - done).min(buf.len() as u64) as usize;
        mem.read(addr + done, &mut buf[..chunk])?;
        out.extend_from_slice(&buf[..chunk]);
        done += chunk as u64;
    }
    Ok(out)
}

/// Probe that every byte of `range_addr .. range_addr + range_len` is *mapped* by reading
/// it (chunked), discarding the values.
///
/// Why this exists: [`crate::memory::checked_range`] proves a range is representable in the
/// address space, not that it is mapped. The copy functions promise **no partial write on
/// fault** — but a `GuestMemory` implementation is free to move bytes before faulting. The
/// probe reads the whole destination first, so a faulting destination is detected before
/// the first byte is written; after a successful probe, the writes themselves cannot fault
/// (the mapping cannot shrink mid-call). Cost: each destination byte is read twice.
fn probe_mapped(mem: &impl GuestMemory, addr: u64, len: u64) -> Result<(), Fault> {
    let mut buf = [0u8; 256];
    let mut done = 0u64;
    while done < len {
        let chunk = (len - done).min(buf.len() as u64) as usize;
        mem.read(addr + done, &mut buf[..chunk])?;
        done += chunk as u64;
    }
    Ok(())
}

/// Find the NUL terminator of the string starting at `s`. Returns the *address of the NUL*.
///
/// Reads are **one byte per trait call**. This is deliberate, not an optimisation miss: a
/// bulk `read(256)` would overread past a region end that the string itself never crosses
/// (a NUL on the last mapped byte must be findable, and the crate has no way to ask a
/// generic `GuestMemory` how far the mapping extends). One byte per call is also exactly
/// the task's termination rule — every iteration reads ≥ 1 byte through the trait, so the
/// loop ends at a NUL or a `Fault` and can never hang.
fn find_nul(mem: &impl GuestMemory, s: u64) -> Result<u64, Fault> {
    if s == 0 {
        return Err(Fault(0));
    }
    let mut probe = [0u8; 1];
    let mut cursor = s;
    loop {
        mem.read(cursor, &mut probe)?;
        if probe[0] == 0 {
            return Ok(cursor);
        }
        cursor += 1;
    }
}

/// Like [`find_nul`], but stops *early* (returning `None`) if a NUL is not found within
/// `max_len` bytes — used by the bounded functions (`strnlen`, `strncmp`, ...).
fn find_nul_bounded(
    mem: &impl GuestMemory,
    s: u64,
    max_len: u64,
) -> Result<Option<u64>, Fault> {
    if s == 0 {
        return Err(Fault(0));
    }
    if max_len == 0 {
        return Ok(None);
    }
    // One byte per trait call, as in [`find_nul`]: a bulk read would overread past a
    // region end the *string* never crosses (bounded functions must tolerate strings that
    // end exactly at a mapping boundary), and byte-at-a-time keeps the termination rule.
    let mut probe = [0u8; 1];
    let mut cursor = s;
    let mut remaining = max_len;
    while remaining > 0 {
        mem.read(cursor, &mut probe)?;
        if probe[0] == 0 {
            return Ok(Some(cursor));
        }
        cursor += 1;
        remaining -= 1;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// strlen family
// ---------------------------------------------------------------------------

/// `size_t strlen(const char *s)`
///
/// Returns the length of the NUL-terminated string at `s` (bytes before the NUL).
///
/// Errors: `Err(Fault(0))` for a null `s`; `Err(Fault(addr))` when the string runs into
/// unmapped memory (the fault address is the first unmapped byte scanned).
pub fn strlen(mem: &impl GuestMemory, s: u64) -> Result<u64, Fault> {
    let nul = find_nul(mem, s)?;
    Ok(nul - s)
}

/// `size_t __strlen_chk(const char *s, size_t size)`
///
/// Bionic FORTIFY form of `strlen`: computes `strlen(s)` but fails the check when the
/// length is `>= size` (on a device this aborts; here a named
/// [`crate::error::BionicError::CheckFailed`]). A correct call passes the size of the
/// object `s` points into, so a string exactly filling it has no room for its NUL and is
/// already corrupt.
pub fn strlen_chk(mem: &impl GuestMemory, s: u64, size: u64) -> crate::error::BionicResult<u64> {
    let len = strlen(mem, s)?;
    if len >= size {
        return Err(crate::error::BionicError::CheckFailed("__strlen_chk"));
    }
    Ok(len)
}

/// `char *__strchr_chk(const char *s, int c, size_t s_len)` -- bionic's FORTIFY `strchr`
/// (`libc/bionic/fortify.cpp`): walk from `s` with `s_len` bytes of budget, returning the first
/// byte equal to `(char)c` -- the terminating NUL included, when `c` is 0 -- or NULL at the NUL;
/// a budget that runs out first is `__fortify_fatal("strchr: prevented read past end of
/// buffer")`, reported here as [`BionicError::CheckFailed`](crate::error::BionicError::CheckFailed).
///
/// Errors: `CheckFailed` as above; a [`Fault`] on unmapped memory.
pub fn strchr_chk(
    mem: &impl GuestMemory,
    s: u64,
    c: i32,
    s_len: u64,
) -> crate::error::BionicResult<u64> {
    let wanted = c as u8;
    let mut at = s;
    let mut left = s_len;
    loop {
        if left == 0 {
            return Err(crate::error::BionicError::CheckFailed("__strchr_chk"));
        }
        let mut byte = [0u8; 1];
        mem.read(at, &mut byte)?;
        if byte[0] == wanted {
            return Ok(at);
        }
        if byte[0] == 0 {
            return Ok(0);
        }
        at = at.checked_add(1).ok_or(Fault(at))?;
        left -= 1;
    }
}

/// `size_t strnlen(const char *s, size_t n)`
///
/// Returns `strlen(s)` if it is `< n`, else `n`. Never scans past `n` bytes: a string
/// without a NUL in its first `n` bytes yields `n` without faulting *inside those bytes*,
/// but bytes before the boundary must still be mapped (each is read once).
///
/// Errors: as [`strlen`] when the scanned bytes are unmapped or `s` is null.
pub fn strnlen(mem: &impl GuestMemory, s: u64, n: u64) -> Result<u64, Fault> {
    match find_nul_bounded(mem, s, n)? {
        Some(nul) => Ok(nul - s),
        None => Ok(n),
    }
}

// ---------------------------------------------------------------------------
// strcmp / strncmp / strcasecmp / strncasecmp
// ---------------------------------------------------------------------------

/// Shared byte-walk producing `(byte_a, byte_b)` pairs after applying `fold` to both,
/// stopping at a (folded) difference, a NUL in either string, or the byte limit.
/// Returns `Ok(None)` when the compared prefixes are equal (both ended, or `limit` bytes
/// matched); `limit = u64::MAX` is the unbounded `strcmp`/`strcasecmp` walk, which still
/// terminates: a NUL is found or a fault ends it. Reads are one byte per trait call per
/// string (see [`find_nul`] for why).
///
/// The fold happens **inside** the loop: `strcasecmp` compares case-folded bytes, so a
/// raw difference (`H` vs `h`) is not a difference until the fold says so. `strcmp` passes
/// the identity fold.
fn walk_pair(
    mem: &impl GuestMemory,
    a: u64,
    b: u64,
    limit: u64,
    fold: fn(u8) -> u8,
) -> Result<Option<(u8, u8)>, Fault> {
    if a == 0 || b == 0 {
        return Err(Fault(if a == 0 { a } else { b }));
    }
    let mut probe_a = [0u8; 1];
    let mut probe_b = [0u8; 1];
    let mut done = 0u64;
    while done < limit {
        mem.read(a + done, &mut probe_a)?;
        mem.read(b + done, &mut probe_b)?;
        let (ca, cb) = (probe_a[0], probe_b[0]);
        if ca == 0 || cb == 0 {
            // Whichever string ended, this is the final comparison position. NUL does not
            // participate in folding: fold(NUL) is NUL for any C-locale fold.
            return Ok(if ca == cb { None } else { Some((ca, cb)) });
        }
        let (fa, fb) = (fold(ca), fold(cb));
        if fa != fb {
            return Ok(Some((fa, fb)));
        }
        done += 1;
    }
    // `limit` bytes compared equal (bounded case only).
    Ok(None)
}

/// `int strcmp(const char *a, const char *b)`
///
/// Lexicographic comparison as unsigned chars, up to and including the first NUL.
/// **Only the sign is specified by C**; bionic returns the byte difference and we
/// match bionic (see the comment in the body).
///
/// Errors: `Err(Fault)` when either string is null or runs into unmapped memory.
pub fn strcmp(mem: &impl GuestMemory, a: u64, b: u64) -> Result<i32, Fault> {
    match walk_pair(mem, a, b, u64::MAX, |b| b)? {
        None => Ok(0),
        // Bionic's strcmp returns the byte difference (c - d), not glibc's ±1. The C
        // standard only fixes the SIGN; bionic fixes the magnitude. We match bionic.
        Some((ca, cb)) => Ok(ca as i32 - cb as i32),
    }
}

/// `size_t strlcpy(char *dst, const char *src, size_t dsize)` -- OpenBSD's, which bionic builds:
/// copy up to `dsize - 1` bytes and NUL-terminate when `dsize != 0`; return `strlen(src)`.
///
/// Errors: `Err(Fault)` on unmapped memory or a null `src`.
pub fn strlcpy(mem: &mut impl GuestMemory, dst: u64, src: u64, dsize: u64) -> Result<u64, Fault> {
    let len = strlen(mem, src)?;
    if dsize != 0 {
        let copy = len.min(dsize - 1);
        let mut bytes = vec![0u8; usize::try_from(copy).map_err(|_| Fault(src))?];
        mem.read(src, &mut bytes)?;
        bytes.push(0);
        mem.write(dst, &bytes)?;
    }
    Ok(len)
}

/// `size_t strxfrm(char *dst, const char *src, size_t n)` -- OpenBSD's, which bionic builds: "since
/// locales are unimplemented, this is just a copy" -- `strlen(src)` for `n == 0`, else
/// [`strlcpy`]. bionic's `strxfrm_l` is this.
///
/// Errors: as [`strlcpy`].
pub fn strxfrm(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    if n == 0 {
        return strlen(mem, src);
    }
    strlcpy(mem, dst, src, n)
}

/// `int strncmp(const char *a, const char *b, size_t n)`
///
/// Like [`strcmp`] but compares at most `n` bytes. `n == 0` returns `0` without touching
/// memory (valid C even for null pointers). Result convention matches bionic (byte
/// difference) — see [`strcmp`].
///
/// Errors: `Err(Fault)` when a scanned byte is unmapped or either pointer is null and
/// `n != 0`.
pub fn strncmp(mem: &impl GuestMemory, a: u64, b: u64, n: u64) -> Result<i32, Fault> {
    if n == 0 {
        return Ok(0);
    }
    match walk_pair(mem, a, b, n, |b| b)? {
        None => Ok(0),
        // Same bionic byte-difference convention as strcmp.
        Some((ca, cb)) => Ok(ca as i32 - cb as i32),
    }
}

/// `int strcasecmp(const char *a, const char *b)` — C/POSIX locale only.
///
/// Case-insensitive [`strcmp`] in the C locale (ASCII `A-Z` fold to `a-z`; bytes ≥ 0x80
/// compare as themselves, unsigned).
pub fn strcasecmp(mem: &impl GuestMemory, a: u64, b: u64) -> Result<i32, Fault> {
    match walk_pair(mem, a, b, u64::MAX, to_lower_ascii)? {
        None => Ok(0),
        Some((la, lb)) => Ok(if la < lb { -1 } else { 1 }),
    }
}

/// `int strncasecmp(const char *a, const char *b, size_t n)` — C/POSIX locale only.
pub fn strncasecmp(mem: &impl GuestMemory, a: u64, b: u64, n: u64) -> Result<i32, Fault> {
    if n == 0 {
        return Ok(0);
    }
    match walk_pair(mem, a, b, n, to_lower_ascii)? {
        None => Ok(0),
        Some((la, lb)) => Ok(if la < lb { -1 } else { 1 }),
    }
}

// ---------------------------------------------------------------------------
// strcpy / strncpy / strcat / strncat (+ _chk)
// ---------------------------------------------------------------------------

/// `char *strcpy(char *dst, const char *src)`
///
/// Copies `src` (including its NUL) to `dst`. Returns `dst`.
///
/// Fault-safety: the source length is found first (terminating at the NUL *or* a fault),
/// then the whole `dst` range is validated with [`crate::memory::checked_range`], and only
/// then are bytes written. A fault can never leave a partial copy.
///
/// Overlap: `strcpy` with overlapping arguments is UB in C (bionic aborts under FORTIFY).
/// Here the copy proceeds byte-forward from the validated snapshot — deterministic and
/// host-safe; no promise beyond that is made.
pub fn strcpy(mem: &mut impl GuestMemory, dst: u64, src: u64) -> Result<u64, Fault> {
    let nul = find_nul(mem, src)?;
    let len = nul - src + 1; // include the NUL
    let (s, len) = checked_range(src, len)?;
    let (d, _) = checked_range(dst, len)?;
    let bytes = read_vec(mem, s, len)?;
    probe_mapped(mem, d, len)?; // prove the destination is fully mapped before writing
    mem.write(d, &bytes)?;
    Ok(dst)
}

/// `char *strncpy(char *dst, const char *src, size_t n)`
///
/// Copies at most `n` bytes from `src`, stopping after the NUL **and padding `dst` with
/// NULs up to `n` total bytes written** when `strlen(src) < n` (C-standard padding, not a
/// typo: `strncpy` always writes exactly `n` bytes unless `src` ends first... which is the
/// same thing: it writes exactly `n` bytes, either source bytes or NUL padding). Returns
/// `dst`. `dst` is not NUL-terminated by this function when `strlen(src) >= n`.
///
/// Errors: fault before first write, as [`strcpy`]; a null `src` with `n != 0` faults.
pub fn strncpy(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    let (d, n) = checked_range(dst, n)?;
    if n == 0 {
        return Ok(dst);
    }
    // How much of src is available: min(strlen(src), n - 1) source bytes + terminator.
    let src_nul = find_nul_bounded(mem, src, n)?;
    let copy_len = match src_nul {
        Some(nul) => nul - src, // copy slen bytes + 1 NUL, then NUL-pad to n
        None => n,              // no NUL in first n bytes: copy all n, no terminator
    };
    // Validate the whole write range (dst was already validated), read the source block.
    let bytes = read_vec(mem, src, copy_len)?;
    // Single validated write: copy block + NUL padding.
    let mut out = Vec::with_capacity(n as usize);
    out.extend_from_slice(&bytes);
    if src_nul.is_some() {
        out.push(0);
        out.resize(n as usize, 0);
    }
    probe_mapped(mem, d, n)?; // prove the destination is fully mapped before writing
    mem.write(d, &out)?;
    Ok(dst)
}

/// `char *__strncpy_chk(char *dst, const char *src, size_t n, size_t dst_size)`
///
/// Bionic FORTIFY form of [`strncpy`]: `n > dst_size` fails the check (named
/// [`crate::error::BionicError::CheckFailed`]) instead of overflowing the destination.
pub fn strncpy_chk(
    mem: &mut impl GuestMemory,
    dst: u64,
    src: u64,
    n: u64,
    dst_size: u64,
) -> crate::error::BionicResult<u64> {
    if n > dst_size {
        return Err(crate::error::BionicError::CheckFailed("__strncpy_chk"));
    }
    Ok(strncpy(mem, dst, src, n)?)
}

/// `char *__strncpy_chk2(char *dst, const char *src, size_t n, size_t dst_size, size_t src_size)`
///
/// Bionic's two-size variant. The destination check is `n > dst_size`, and the **source** check
/// is per byte actually read: bionic's loop aborts only when it is about to read `src[src_size]`,
/// which it never reaches if the source's NUL comes first.
///
/// # `n > src_size` was the check and it was wrong
///
/// This used to fail whenever `n > src_size`, which aborts the single commonest FORTIFY shape
/// there is: `strncpy(dst, src, sizeof dst)` where `src` is a smaller object. Bionic copies the
/// source up to its NUL and NUL-pads the rest of `n`; nothing reads past the source at all.
/// **Found by M4's gate**, where `JNI_OnLoad` does exactly that and the refusal stopped
/// jni-surface.md section 8 step 6 -- a check that was wrong in the direction that refuses
/// legitimate calls, which is the safe direction to be wrong in and still a defect.
///
/// A source with no NUL in its first `min(n, src_size)` bytes and `n > src_size` **does** fail:
/// that is the case where bionic's loop really would read `src[src_size]`.
pub fn strncpy_chk2(
    mem: &mut impl GuestMemory,
    dst: u64,
    src: u64,
    n: u64,
    dst_size: u64,
    src_size: u64,
) -> crate::error::BionicResult<u64> {
    if n > dst_size {
        return Err(crate::error::BionicError::CheckFailed("__strncpy_chk2"));
    }
    let (d, n) = checked_range(dst, n)?;
    if n == 0 {
        return Ok(dst);
    }
    // Bionic reads at most this many source bytes: it stops at the NUL and it stops before
    // `src_size`. The scan is bounded by both, so an unterminated source cannot run away.
    let readable = n.min(src_size);
    let src_nul = find_nul_bounded(mem, src, readable)?;
    let copy_len = match src_nul {
        Some(nul) => nul - src,
        None if n > src_size => {
            // The loop would have reached `src[src_size]` without having seen a NUL, which is
            // where bionic calls `__fortify_fatal`.
            return Err(crate::error::BionicError::CheckFailed("__strncpy_chk2"));
        }
        None => n,
    };
    let bytes = read_vec(mem, src, copy_len)?;
    let mut out = Vec::with_capacity(n as usize);
    out.extend_from_slice(&bytes);
    if src_nul.is_some() {
        // C's padding rule: `strncpy` writes exactly `n` bytes, the tail as NULs.
        out.push(0);
        out.resize(n as usize, 0);
    }
    probe_mapped(mem, d, n)?;
    mem.write(d, &out)?;
    Ok(dst)
}

/// `char *strcat(char *dst, const char *src)`
///
/// Appends `src` (including its NUL) after the string already at `dst`. Returns `dst`.
///
/// Fault-safety: `strlen(dst)` is found, then the combined range is validated, then
/// written — no partial append on fault.
pub fn strcat(mem: &mut impl GuestMemory, dst: u64, src: u64) -> Result<u64, Fault> {
    let dst_nul = find_nul(mem, dst)?;
    let src_nul = find_nul(mem, src)?;
    let src_len = src_nul - src + 1; // include NUL
    let (s, src_len) = checked_range(src, src_len)?;
    let (d, _) = checked_range(dst_nul, src_len)?;
    let bytes = read_vec(mem, s, src_len)?;
    probe_mapped(mem, d, src_len)?; // prove the append tail is fully mapped before writing
    mem.write(d, &bytes)?;
    Ok(dst) // C: strcat returns its dst argument, not the append position
}

/// `char *__strcat_chk(char *dst, const char *src, size_t dst_size)`
///
/// Bionic FORTIFY form of [`strcat`]: fails the check when
/// `strlen(dst) + strlen(src) + 1 > dst_size` (the appended string including its NUL must
/// fit the destination object). Named [`crate::error::BionicError::CheckFailed`] for
/// `"__strcat_chk"` instead of a device abort.
pub fn strcat_chk(
    mem: &mut impl GuestMemory,
    dst: u64,
    src: u64,
    dst_size: u64,
) -> crate::error::BionicResult<u64> {
    let dst_nul = find_nul(mem, dst)?;
    let src_nul = find_nul(mem, src)?;
    let need = (dst_nul - dst) + (src_nul - src) + 1;
    if need > dst_size {
        return Err(crate::error::BionicError::CheckFailed("__strcat_chk"));
    }
    // Re-use strcat's logic on the (now known-safe) lengths; skip its re-scan by calling
    // through — the extra scan is bounded by the same validated ranges.
    Ok(strcat(mem, dst, src)?)
}

/// `char *strncat(char *dst, const char *src, size_t n)`
///
/// Appends at most `n` bytes from `src`, then always a NUL (so up to `n + 1` bytes are
/// written). Returns `dst`. Unlike `strncpy`, `strncat` NUL-terminates.
pub fn strncat(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    let dst_nul = find_nul(mem, dst)?;
    // Read at most n source bytes, stopping at src's NUL if earlier.
    let src_nul_in_n = find_nul_bounded(mem, src, n)?;
    let copy_len = match src_nul_in_n {
        Some(nul) => nul - src,
        None => n,
    };
    let (d, _) = checked_range(dst_nul, copy_len + 1)?; // + NUL
    let bytes = read_vec(mem, src, copy_len)?;
    let mut out = Vec::with_capacity(copy_len as usize + 1);
    out.extend_from_slice(&bytes);
    out.push(0);
    probe_mapped(mem, d, copy_len + 1)?; // prove the tail is mapped before writing
    mem.write(d, &out)?;
    Ok(dst)
}

// ---------------------------------------------------------------------------
// strchr / strrchr / strstr / strspn / strcspn
// ---------------------------------------------------------------------------

/// `char *strchr(const char *s, int c)`
///
/// Finds the first occurrence of the byte `(unsigned char)c` **in the string** `s`, and the
/// string ends at its terminator. The terminator is itself a candidate — C makes it "part of the
/// string" for this function specifically — so `strchr(s, 0)` returns the terminator's address.
/// A byte that is not in the string returns guest `0`.
///
/// # The scan stops at the NUL, and this is the one thing here that was wrong
///
/// This loop used to have **no terminator arm at all**: it read forward until it found `c` or
/// faulted. A search for a byte the string does not contain therefore walked *past* the
/// terminator and kept going until it left the mapping, and the refusal it produced named an
/// address in whatever happened to follow.
///
/// C 7.24.5.2 is explicit that the search is over "the string pointed to by `s`", and the string
/// is the bytes up to and including its terminator. `strchr` returning a null pointer when the
/// byte is absent is the whole of how every caller detects absence.
///
/// **MEASURED, and it is the defect that stopped M6.** `libroblox.so` embeds OpenSSL, whose
/// `crypto/core_namemap.c` tokenises an algorithm-name list by calling `strchr(names, ':')` in a
/// loop at guest `0x029f7748`. Most names contain no colon. The scan ran off the end of the
/// guest allocation, the refusal unwound out of OpenSSL **while it held a global lock**, and
/// every later acquirer of that lock spun for ever — which is how a one-line omission in a search
/// function presented as §8 row 21 hanging four milestones away, with a graphics subsystem that
/// had never been asked for anything getting the blame.
///
/// The `want` comparison stays **above** the terminator check: `strchr(s, 0)` must find the
/// terminator rather than report it as the end of the search.
pub fn strchr(mem: &impl GuestMemory, s: u64, c: i32) -> Result<u64, Fault> {
    if s == 0 {
        return Err(Fault(0));
    }
    let want = (c & 0xFF) as u8;
    let mut probe = [0u8; 1];
    let mut cursor = s;
    loop {
        mem.read(cursor, &mut probe)?;
        if probe[0] == want {
            return Ok(cursor);
        }
        if probe[0] == 0 {
            // The end of the string, and `c` was not in it. A **fault** here would be this layer
            // walking past a terminator the guest put there on purpose; an unterminated string
            // still faults, because then there is no terminator to stop at.
            return Ok(0);
        }
        cursor += 1;
    }
}

/// `char *strrchr(const char *s, int c)`
///
/// Finds the **last** occurrence of `(unsigned char)c` in `s` (NUL included as a candidate).
/// Returns guest `0` when not found. Scans the whole string to its NUL, tracking the last
/// match; terminates at NUL or fault like every other loop here.
pub fn strrchr(mem: &impl GuestMemory, s: u64, c: i32) -> Result<u64, Fault> {
    if s == 0 {
        return Err(Fault(0));
    }
    let want = (c & 0xFF) as u8;
    let nul = find_nul(mem, s)?;
    // The scan window [s, nul] is fully mapped (find_nul proved it), so read it at once.
    let bytes = read_vec(mem, s, nul - s + 1)?;
    match bytes.iter().rposition(|&b| b == want) {
        Some(pos) => Ok(s + pos as u64),
        None => Ok(0),
    }
}

/// `char *strstr(const char *h, const char *n)`
///
/// Finds the first occurrence of the NUL-terminated needle `n` in the haystack `h`.
/// Returns `h` when the needle is empty (C: an empty string is found at the start);
/// guest `0` when not found.
pub fn strstr(mem: &impl GuestMemory, h: u64, n: u64) -> Result<u64, Fault> {
    if h == 0 || n == 0 {
        return Err(Fault(if h == 0 { h } else { n }));
    }
    let needle_nul = find_nul(mem, n)?;
    let needle_len = needle_nul - n;
    if needle_len == 0 {
        return Ok(h);
    }    let needle = read_vec(mem, n, needle_len)?;
    // Walk the haystack one byte at a time (see [`find_nul`] for why not in bulk): every
    // iteration reads ≥ 1 byte through the trait, so the loop ends at the haystack's NUL
    // or a fault. A candidate match window is re-read through the trait byte-by-byte too,
    // which correctly faults if a match window would run off the mapping (a match cannot
    // legally extend past the haystack's terminator, so this only triggers on corrupt
    // guest state, and a fault is the honest report).
    let mut probe = [0u8; 1];
    let mut cursor = h;
    loop {
        mem.read(cursor, &mut probe)?;
        if probe[0] == 0 {
            // End of haystack: nothing left to match.
            return Ok(0);
        }
        if probe[0] == needle[0] {
            // Candidate at `cursor`: verify the rest of the needle.
            let mut matched = true;
            for (k, &nb) in needle.iter().enumerate().skip(1) {
                let mut byte = [0u8; 1];
                mem.read(cursor + k as u64, &mut byte)?;
                if byte[0] != nb {
                    matched = false;
                    break;
                }
            }
            if matched {
                return Ok(cursor);
            }
        }
        cursor += 1;
    }
}

/// Length of the initial segment of `s` made only of bytes in `set` (`in_set_counts`) or
/// not in the set. Shared by [`strspn`]/[`strcspn`]. One byte per trait call (see
/// [`find_nul`]); the set is read fully once (its NUL bounds it).
fn span_walk(
    mem: &impl GuestMemory,
    s: u64,
    set_addr: u64,
    in_set_counts: bool,
) -> Result<u64, Fault> {
    if s == 0 || set_addr == 0 {
        return Err(Fault(if s == 0 { s } else { set_addr }));
    }
    let set_nul = find_nul(mem, set_addr)?;
    let set = read_vec(mem, set_addr, set_nul - set_addr)?;
    let mut probe = [0u8; 1];
    let mut cursor = s;
    let mut count = 0u64;
    loop {
        mem.read(cursor, &mut probe)?;
        let b = probe[0];
        if b == 0 {
            return Ok(count);
        }
        let member = set.contains(&b);
        if member == in_set_counts {
            count += 1;
            cursor += 1;
        } else {
            return Ok(count);
        }
    }
}

/// `size_t strspn(const char *s, const char *accept)`
///
/// Length of the initial segment of `s` made only of bytes in `accept`.
pub fn strspn(mem: &impl GuestMemory, s: u64, accept: u64) -> Result<u64, Fault> {
    span_walk(mem, s, accept, true)
}

/// `char *strpbrk(const char *s, const char *accept)`
///
/// The first byte of `s` that is in `accept`, or NULL: `s + strcspn(s, accept)` unless that lands
/// on the terminating NUL.
pub fn strpbrk(mem: &impl GuestMemory, s: u64, accept: u64) -> Result<u64, Fault> {
    let at = s + span_walk(mem, s, accept, false)?;
    let mut byte = [0u8; 1];
    mem.read(at, &mut byte)?;
    Ok(if byte[0] == 0 { 0 } else { at })
}

/// `size_t strcspn(const char *s, const char *reject)`
///
/// Length of the initial segment of `s` made only of bytes **not** in `reject`.
pub fn strcspn(mem: &impl GuestMemory, s: u64, reject: u64) -> Result<u64, Fault> {
    span_walk(mem, s, reject, false)
}

// ---------------------------------------------------------------------------
// Error strings
// ---------------------------------------------------------------------------

/// The C/POSIX error messages this crate returns, byte-for-byte the strings glibc/bionic
/// use for the codes reachable here (VERIFIED against the glibc `string`/`sys_errlist`
/// wording, which bionic matches for these codes; each tested literal is asserted in
/// `tests/string_tests.rs` against the POSIX-specified text).
///
/// Unknown codes return bionic's fallback shape `Unknown error N` (bionic
/// `__strerror_r` produces `Unknown error <n>`; glibc `<n>: Unknown error` is NOT used).
const ERRNO_STRINGS: &[(i32, &str)] = &[
    (crate::errno::consts::EPERM, "Operation not permitted"),
    (crate::errno::consts::ENOENT, "No such file or directory"),
    (crate::errno::consts::EINTR, "Interrupted system call"),
    (crate::errno::consts::EBADF, "Bad file descriptor"),
    (crate::errno::consts::ENOMEM, "Out of memory"),
    (crate::errno::consts::EACCES, "Permission denied"),
    (crate::errno::consts::EINVAL, "Invalid argument"),
    (crate::errno::consts::EDOM, "Numerical argument out of domain"),
    (crate::errno::consts::ERANGE, "Numerical result out of range"),
    (crate::errno::consts::ENOSYS, "Function not implemented"),
];

/// POSIX message for `errnum`, or bionic's `Unknown error N` fallback.
pub fn strerror_message(errnum: i32) -> &'static str {
    ERRNO_STRINGS
        .iter()
        .find(|(code, _)| *code == errnum)
        .map(|(_, msg)| *msg)
        .unwrap_or("Unknown error")
}

/// `int strerror_r(int errnum, char *buf, size_t buflen)` — the **POSIX** form.
///
/// bionic exports both spellings and they differ in what they return: this one answers `0` or
/// `ERANGE`, where [`gnu_strerror_r`] answers a `char *`. Getting them the wrong way round turns
/// every success into a pointer the caller will dereference, or every pointer into a `0` the
/// caller reads as success and then prints an empty buffer — which is why they are two functions
/// here rather than one with a flag.
///
/// bionic's own implementation writes with `strlcpy` and returns `ERANGE` when the message did
/// not fit, **without touching `errno`** (its `ErrnoRestorer` puts back whatever was there). So
/// truncation is reported in the return value only, and this does the same: a caller that ignores
/// the result gets a NUL-terminated prefix, which is what a device gives it.
///
/// `buflen == 0` or a null `buf` cannot even hold a NUL; that is `ERANGE`, since nothing was
/// written and the message did not fit.
pub fn strerror_r(
    mem: &mut impl GuestMemory,
    errnum: i32,
    buf: u64,
    buflen: u64,
) -> crate::error::BionicResult<i32> {
    if buflen == 0 || buf == 0 {
        return Ok(crate::errno::consts::ERANGE);
    }
    let (b, len) = checked_range(buf, buflen)?;
    let message: Vec<u8> = if ERRNO_STRINGS.iter().any(|(c, _)| *c == errnum) {
        strerror_message(errnum).as_bytes().to_vec()
    } else {
        format!("Unknown error {errnum}").into_bytes()
    };
    // `strlcpy`: copy what fits, always terminate, and report the length it *wanted*.
    let room = (len - 1) as usize;
    let copied = message.len().min(room);
    let mut out = message[..copied].to_vec();
    out.push(0);
    mem.write(b, &out)?;
    if message.len() >= len as usize {
        return Ok(crate::errno::consts::ERANGE);
    }
    Ok(0)
}

/// `char *__gnu_strerror_r(int errnum, char *buf, size_t buflen)`
///
/// bionic's GNU-flavoured `strerror_r`: writes the message for `errnum` into the caller's
/// `buf` and **returns `buf` itself** (this is the GNU semantics; POSIX `strerror_r`
/// returns an `int` — bionic exports both under different names).
///
/// Truncation: when the message does not fit, the C functions leave a NUL-terminated
/// prefix. GNU `strerror_r` does not set errno; POSIX variants set ERANGE. This crate
/// follows the GNU form: truncate to `buflen - 1` bytes + NUL, no errno touch, return `buf`.
///
/// `buflen == 0` or a null `buf`: nothing can be written, not even a NUL. The GNU contract
/// has no defined result; here it returns [`crate::error::BionicError::InvalidArgument`]
/// naming the function rather than pretending.
pub fn gnu_strerror_r(
    mem: &mut impl GuestMemory,
    errnum: i32,
    buf: u64,
    buflen: u64,
) -> crate::error::BionicResult<u64> {
    if buflen == 0 || buf == 0 {
        return Err(crate::error::BionicError::InvalidArgument("__gnu_strerror_r"));
    }
    let (b, len) = checked_range(buf, buflen)?;
    let msg = strerror_message(errnum);
    let mut out: Vec<u8> = msg.as_bytes().to_vec();
    if errnum < 0 || !ERRNO_STRINGS.iter().any(|(c, _)| *c == errnum) {
        // bionic: "Unknown error <n>" (no colon; glibc-style "<n>: Unknown error" unused).
        out = format!("Unknown error {errnum}").into_bytes();
    }
    out.resize((len - 1) as usize, 0);
    out.push(0);
    out.truncate(len as usize);
    mem.write(b, &out)?;
    Ok(b)
}
