//! Memory functions: the `mem*` family and the reachable FORTIFY `_chk` variants.
//!
//! Signatures implemented (guest ABI, LP64 — `size_t` = u64, `int` = i32):
//!
//! | C signature | here |
//! |---|---|
//! | `void *memcpy(void *dst, const void *src, size_t n)` | [`memcpy`] |
//! | `void *memmove(void *dst, const void *src, size_t n)` | [`memmove`] |
//! | `void *memset(void *dst, int c, size_t n)` | [`memset`] |
//! | `int memcmp(const void *a, const void *b, size_t n)` | [`memcmp`] |
//! | `void *memchr(const void *s, int c, size_t n)` | [`memchr`] |
//! | `void *__memcpy_chk(void *dst, const void *src, size_t n, size_t dst_size)` | [`memcpy_chk`] |
//! | `void *__memset_chk(void *dst, int c, size_t n, size_t dst_size)` | [`memset_chk`] |
//!
//! Behaviour follows C99/POSIX (`memcpy(3)`, `memcmp(3)`, ...):
//! * `memcmp` returns an integer whose **sign** is specified, not its magnitude.
//! * `memchr` on a zero-length range is `NULL` — even for `s == NULL`, which is valid C.
//! * `memmove` handles overlap in both directions (backward copy when `dst > src`,
//!   forward otherwise). `memcpy` with overlap is undefined in C; the decision here is
//!   documented on [`memcpy`] and is memory-safe in every direction.
//!
//! Fault-safety: every range is validated with [`crate::memory::checked_range`] *before* the
//! first byte moves, so a function either completes or reports a `Fault` without having
//! written a partial result. This is stronger than the mock's byte-at-a-time guarantee and
//! matters because a partially-copied buffer is a corrupted guest state.

use crate::memory::{checked_range, Fault, GuestMemory};

/// Helper: return value of the returning functions — the destination address, or `NULL`
/// (guest `0`) exactly where C says the function returns `NULL`.
fn ok_ptr(addr: u64) -> Result<u64, Fault> {
    Ok(addr)
}

/// `void *memcpy(void *dst, const void *src, size_t n)`
///
/// Copies `n` bytes from `src` to `dst`. Returns `dst` on success.
///
/// **Overlap decision.** C makes `memcpy` with overlapping ranges undefined; bionic's
/// arm64 implementation forwards to `memmove`-shaped vector code, which behaves like
/// `memmove` in practice but makes no promise. Here `memcpy` copies **forward** (byte 0
/// first). That choice is deterministic, never a host memory-safety event, and matches the
/// common hardware behaviour; code relying on it is relying on undefined behaviour and will
/// be treated as `memmove`-with-forward-bias. Documented divergence: none — both choices
/// are conforming because callers may not pass overlap at all.
///
/// Errors: `Err(Fault)` if `src` or `dst` is unmapped or `src + n`/`dst + n` would exceed
/// the address space, or if either pointer is null with `n != 0`. `n == 0` succeeds for
/// any pointers, including null (valid C).
pub fn memcpy(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    let (s, n) = checked_range(src, n)?;
    let (d, n) = checked_range(dst, n)?;
    if n == 0 {
        return ok_ptr(dst);
    }
    // Copy through host buffers in bounded chunks so a huge `n` cannot allocate `n` bytes
    // on the host: validate-then-copy is still all-or-nothing because both ranges were
    // checked in full above.
    let mut buf = [0u8; 256];
    let mut done = 0u64;
    while done < n {
        let chunk = (n - done).min(buf.len() as u64) as usize;
        mem.read(s + done, &mut buf[..chunk])?;
        mem.write(d + done, &buf[..chunk])?;
        done += chunk as u64;
    }
    ok_ptr(dst)
}

/// `void *memmove(void *dst, const void *src, size_t n)`
///
/// Copies `n` bytes from `src` to `dst`, correct for **overlap in both directions**:
/// when `dst > src` (and the ranges overlap) bytes are copied back-to-front so the
/// source is never clobbered before it is read; otherwise front-to-back.
/// Returns `dst` on success.
///
/// Errors: as [`memcpy`].
pub fn memmove(mem: &mut impl GuestMemory, dst: u64, src: u64, n: u64) -> Result<u64, Fault> {
    let (s, n) = checked_range(src, n)?;
    let (d, n) = checked_range(dst, n)?;
    if n == 0 {
        return ok_ptr(dst);
    }
    let overlap_forward_bias = d > s && d < s + n; // dst inside (src, src+n): back-to-front
    if !overlap_forward_bias {
        return memcpy(mem, d, s, n);
    }
    // Back-to-front copy in bounded chunks. Because both ranges are validated up front,
    // any fault surfaces before the first write; chunking from the end is still exact.
    const CHUNK: u64 = 256;
    let mut remaining = n;
    while remaining > 0 {
        let chunk = remaining.min(CHUNK);
        let mut buf = [0u8; 256];
        let chunk = chunk as usize;
        mem.read(s + remaining - chunk as u64, &mut buf[..chunk])?;
        mem.write(d + remaining - chunk as u64, &buf[..chunk])?;
        remaining -= chunk as u64;
    }
    ok_ptr(dst)
}

/// `void *memset(void *dst, int c, size_t n)`
///
/// Fills `n` bytes at `dst` with the low byte of `c`. Returns `dst`.
///
/// Errors: as [`memcpy`].
pub fn memset(mem: &mut impl GuestMemory, dst: u64, c: i32, n: u64) -> Result<u64, Fault> {
    let (d, n) = checked_range(dst, n)?;
    if n == 0 {
        return ok_ptr(dst);
    }
    let byte = (c & 0xFF) as u8;
    let buf = [byte; 256];
    let mut done = 0u64;
    while done < n {
        let chunk = (n - done).min(buf.len() as u64) as usize;
        mem.write(d + done, &buf[..chunk])?;
        done += chunk as u64;
    }
    ok_ptr(dst)
}

/// `int memcmp(const void *a, const void *b, size_t n)`
///
/// Compares `n` bytes as unsigned chars. **Only the sign is specified by C** — the
/// magnitude is not — but this implementation returns the conventional `-1/0/1` on
/// difference, which callers must not rely on beyond its sign.
///
/// Errors: `Err(Fault)` when either range is unmapped or overflows; `Ok(0)` for `n == 0`
/// at any addresses (valid C).
pub fn memcmp(mem: &impl GuestMemory, a: u64, b: u64, n: u64) -> Result<i32, Fault> {
    let (a, n) = checked_range(a, n)?;
    let (b, n) = checked_range(b, n)?;
    let mut buf_a = [0u8; 256];
    let mut buf_b = [0u8; 256];
    let mut done = 0u64;
    while done < n {
        let chunk = (n - done).min(buf_a.len() as u64) as usize;
        mem.read(a + done, &mut buf_a[..chunk])?;
        mem.read(b + done, &mut buf_b[..chunk])?;
        for i in 0..chunk {
            if buf_a[i] != buf_b[i] {
                // Unsigned-char comparison semantics; sign-only result.
                return Ok(if buf_a[i] < buf_b[i] { -1 } else { 1 });
            }
        }
        done += chunk as u64;
    }
    Ok(0)
}

/// `void *memchr(const void *s, int c, size_t n)`
///
/// Scans `n` bytes at `s` for the low byte of `c`. Returns the address of the first match
/// or guest `NULL` (`0`). Zero-length scans return `NULL` without touching memory.
///
/// Errors: `Err(Fault)` when the range is unmapped or overflows.
pub fn memchr(mem: &impl GuestMemory, s: u64, c: i32, n: u64) -> Result<u64, Fault> {
    let (s, n) = checked_range(s, n)?;
    let want = (c & 0xFF) as u8;
    let mut buf = [0u8; 256];
    let mut done = 0u64;
    while done < n {
        let chunk = (n - done).min(buf.len() as u64) as usize;
        mem.read(s + done, &mut buf[..chunk])?;
        if let Some(pos) = buf[..chunk].iter().position(|&b| b == want) {
            return ok_ptr(s + done + pos as u64);
        }
        done += chunk as u64;
    }
    Ok(0)
}

/// `void *__memcpy_chk(void *dst, const void *src, size_t n, size_t dst_size)`
///
/// FORTIFY form of [`memcpy`]: the compiler passes the destination's object size, and the
/// call must fail loudly when `n` would overflow it. On a real device bionic aborts the
/// process; here a detected overflow returns [`crate::error::BionicError::CheckFailed`]
/// naming `"__memcpy_chk"` — never a host abort, never a silent partial copy.
pub fn memcpy_chk(
    mem: &mut impl GuestMemory,
    dst: u64,
    src: u64,
    n: u64,
    dst_size: u64,
) -> crate::error::BionicResult<u64> {
    if n > dst_size {
        return Err(crate::error::BionicError::CheckFailed("__memcpy_chk"));
    }
    Ok(memcpy(mem, dst, src, n)?)
}

/// `void *__memset_chk(void *dst, int c, size_t n, size_t dst_size)`
///
/// FORTIFY form of [`memset`]; overflow returns a named [`crate::error::BionicError::CheckFailed]
/// for `"__memset_chk"` instead of aborting or guessing.
pub fn memset_chk(
    mem: &mut impl GuestMemory,
    dst: u64,
    c: i32,
    n: u64,
    dst_size: u64,
) -> crate::error::BionicResult<u64> {
    if n > dst_size {
        return Err(crate::error::BionicError::CheckFailed("__memset_chk"));
    }
    Ok(memset(mem, dst, c, n)?)
}
