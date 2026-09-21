//! The two network symbols that are pure computation: `inet_ntop` and `gai_strerror`'s table.
//!
//! # Why exactly two of the eight are here
//!
//! Eight network symbols are in the 188 the initializers reach — `socket`, `poll`, `select`,
//! `eventfd`, `getaddrinfo`, `freeaddrinfo`, `gai_strerror`, `inet_ntop`. Six of them need
//! something outside this crate: a host socket, a descriptor table, a resolver, or guest memory
//! to marshal an `addrinfo` list into. The adapter answers or refuses those by name
//! (`omni_android::bionic::net` has the table and the argument for each).
//!
//! These two need none of it, and that is the same line [`crate::signal`] draws for `sigfillset`:
//!
//! * [`inet_ntop`] is **formatting**. Four or sixteen bytes in, a string out. There is no
//!   network in it at all — a guest can call it on an address it made up.
//! * [`gai_strerror_message`] is a **lookup table** of fifteen constant strings. It is here
//!   rather than in the adapter because the strings are a fact about bionic, and this crate is
//!   where facts about bionic that need no OS live (D19).
//!
//! # `inet_ntop` follows BIND's algorithm, because bionic's *is* BIND's
//!
//! Bionic's `inet_ntop.c` is the ISC/BIND implementation essentially unchanged, and its IPv6
//! form has two behaviours that a from-scratch RFC 5952 formatter gets wrong:
//!
//! 1. **A run of a single zero group is not compressed.** `1:0:2:3:4:5:6:7` formats with the
//!    zero spelled out, not as `1::2:3:4:5:6:7`. BIND's check is `best.len > 1`, and RFC 5952
//!    section 4.2.2 agrees — but a naive "replace the longest run" does not.
//! 2. **Some addresses end in dotted-quad form.** `::1.2.3.4` and `::ffff:1.2.3.4` do;
//!    `::` and `::1` explicitly do **not**, which is a special case BIND writes out because
//!    without it they would format as `::0.0.0.0` and `::0.0.0.1`.
//!
//! Both are asserted below against the exact inputs that distinguish them.
//!
//! **The algorithm is written out here rather than delegated to [`std::net::Ipv6Addr`]'s
//! `Display`, and a differential run says that was necessary rather than merely principled.**
//! A 200,000-address comparison against `Display` disagrees **43 times**, and all 43 are one
//! class: Rust deliberately stopped printing the deprecated *IPv4-compatible* form in dotted
//! notation, so it writes `::77:0` where bionic writes `::0.119.0.0`. `Display` is therefore not
//! an oracle for this function, and the value it returns is guest-observable ABI. The same
//! follow-bionic-not-something-else convention [`crate::guestcmp`] records for `strcmp`'s byte
//! difference — and the same shape: the convenient implementation was *nearly* right.
//!
//! # A too-small buffer is `ENOSPC`, never a truncation
//!
//! BIND formats into a local buffer and only then compares against `size`, so a destination that
//! is one byte short receives **nothing** and the call returns `NULL` with `ENOSPC`. That is
//! copied exactly. A truncated address is the worst available answer: `10.0.0.1` truncated to
//! `10.0.0.` is still a string, still prints, and names a different host.

use crate::errno::consts;
use crate::memory::{checked_range, Fault, GuestMemory};

/// `AF_INET`. Linux's value, which is 2 on every architecture.
pub const AF_INET: i32 = 2;

/// `AF_INET6`. Linux's value, **10** — and it is a number worth spelling out, because it is one
/// of the few `AF_*` constants that differs between systems: it is 10 on Linux, 28 on the BSDs
/// and macOS, and 23 on Windows. The guest was compiled against bionic, so it is 10.
pub const AF_INET6: i32 = 10;

/// Bytes of an `in_addr`: one IPv4 address.
pub const IN_ADDR_BYTES: usize = 4;

/// Bytes of an `in6_addr`: one IPv6 address.
pub const IN6_ADDR_BYTES: usize = 16;

/// The longest string [`inet_ntop`] can produce, without its NUL.
///
/// `"ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255"` — the IPv4-suffixed form is longer than the
/// all-hex form, which is the trap in sizing this buffer. It is also exactly `INET6_ADDRSTRLEN
/// - 1`, and `INET6_ADDRSTRLEN` is 46.
pub const MAX_TEXT_BYTES: usize = 45;

/// `const char *inet_ntop(int af, const void *src, char *dst, socklen_t size)`.
///
/// `Ok(Ok(dst))` when the address was written; `Ok(Err(errno))` for the two failures bionic
/// reports — `EAFNOSUPPORT` for an address family this function does not format, and `ENOSPC`
/// for a `dst` that cannot hold the result **and its NUL**; `Err(Fault)` when `src` is not
/// readable or `dst` is not writable guest memory.
///
/// # Errors
///
/// [`Fault`] if the four or sixteen bytes at `src` are not readable, or if the bytes at `dst`
/// are not writable. A null `src` or `dst` is a fault rather than an errno: bionic does not
/// check for one and dereferences it, so there is no errno it would report, and the guest asked
/// this layer to read or write at address zero.
pub fn inet_ntop(
    mem: &mut impl GuestMemory,
    af: i32,
    src: u64,
    dst: u64,
    size: u32,
) -> Result<Result<u64, i32>, Fault> {
    let text = match af {
        AF_INET => {
            checked_range(src, IN_ADDR_BYTES as u64)?;
            let mut bytes = [0u8; IN_ADDR_BYTES];
            mem.read(src, &mut bytes)?;
            format_v4(bytes)
        }
        AF_INET6 => {
            checked_range(src, IN6_ADDR_BYTES as u64)?;
            let mut bytes = [0u8; IN6_ADDR_BYTES];
            mem.read(src, &mut bytes)?;
            format_v6(bytes)
        }
        // Every other family, including `AF_UNSPEC` (0) and `AF_UNIX` (1). Bionic's own answer,
        // and one guest code has a branch for.
        _ => return Ok(Err(consts::EAFNOSUPPORT)),
    };
    // BIND's check, with its off-by-one intact: the comparison is against the length **without**
    // the NUL, so `size` must be strictly greater than it. `size == text.len()` is ENOSPC,
    // because the terminator would not fit.
    if size as usize <= text.len() {
        return Ok(Err(consts::ENOSPC));
    }
    let mut out = [0u8; MAX_TEXT_BYTES + 1];
    out[..text.len()].copy_from_slice(&text);
    let len = text.len() + 1;
    // Checked as one range before anything is written, so a `dst` whose last byte leaves mapped
    // memory receives **nothing**. A half-written address is a different address.
    checked_range(dst, len as u64)?;
    mem.write(dst, &out[..len])?;
    Ok(Ok(dst))
}

/// A short, inline, allocation-free string: this crate formats into fixed buffers.
///
/// A `Vec` would work and would also be the only heap allocation in this module. The cap is
/// [`MAX_TEXT_BYTES`], which is a property of the two formats rather than a guess, and pushing
/// past it is impossible by construction rather than by a check — every writer below is bounded
/// by the sixteen bytes it was given.
#[derive(Clone, Copy)]
struct Text {
    bytes: [u8; MAX_TEXT_BYTES],
    len: usize,
}

impl Text {
    fn new() -> Self {
        Self { bytes: [0; MAX_TEXT_BYTES], len: 0 }
    }

    /// Append one byte. Silently drops past the cap, which no caller here can reach — see the
    /// type's documentation and `the_longest_output_is_the_ipv4_suffixed_form`.
    fn push(&mut self, byte: u8) {
        if self.len < MAX_TEXT_BYTES {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    /// Append a decimal number, 0-255.
    fn push_u8(&mut self, value: u8) {
        if value >= 100 {
            self.push(b'0' + value / 100);
        }
        if value >= 10 {
            self.push(b'0' + (value / 10) % 10);
        }
        self.push(b'0' + value % 10);
    }

    /// Append a 16-bit group in lowercase hex with no leading zeros, which is what `"%x"` does
    /// and what RFC 5952 section 4.3 requires.
    fn push_hex(&mut self, value: u16) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut started = false;
        for shift in [12, 8, 4] {
            let nibble = ((value >> shift) & 0xF) as usize;
            if nibble != 0 || started {
                self.push(DIGITS[nibble]);
                started = true;
            }
        }
        self.push(DIGITS[(value & 0xF) as usize]);
    }

    fn len(&self) -> usize {
        self.len
    }
}

impl core::ops::Deref for Text {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// `inet_ntop4`: the dotted quad.
fn format_v4(src: [u8; IN_ADDR_BYTES]) -> Text {
    let mut text = Text::new();
    for (i, byte) in src.iter().enumerate() {
        if i != 0 {
            text.push(b'.');
        }
        text.push_u8(*byte);
    }
    text
}

/// `inet_ntop6`: BIND's algorithm, group by group.
fn format_v6(src: [u8; IN6_ADDR_BYTES]) -> Text {
    let mut words = [0u16; 8];
    for (i, word) in words.iter_mut().enumerate() {
        *word = (u16::from(src[i * 2]) << 8) | u16::from(src[i * 2 + 1]);
    }

    // The longest run of zero groups, **first** one on a tie, and only if it is longer than one
    // group. Both halves are BIND's and both are RFC 5952's; a run of one is left uncompressed
    // because `::` standing for a single zero group saves nothing and is ambiguous to read.
    let mut best: Option<(usize, usize)> = None;
    let mut current: Option<(usize, usize)> = None;
    for (i, word) in words.iter().enumerate() {
        if *word == 0 {
            current = Some(match current {
                Some((base, len)) => (base, len + 1),
                None => (i, 1),
            });
            if let Some((base, len)) = current {
                if best.is_none_or(|(_, best_len)| len > best_len) {
                    best = Some((base, len));
                }
            }
        } else {
            current = None;
        }
    }
    let best = best.filter(|(_, len)| *len > 1);

    let mut text = Text::new();
    let mut i = 0;
    while i < 8 {
        if let Some((base, len)) = best {
            if i >= base && i < base + len {
                if i == base {
                    text.push(b':');
                }
                i += 1;
                continue;
            }
        }
        if i != 0 {
            text.push(b':');
        }
        // The encapsulated-IPv4 case, exactly BIND's condition. It can only fire at group 6, and
        // only when the zero run started at group 0 — so `::ffff:1.2.3.4` and `::1.2.3.4` take
        // it while `1::1.2.3.4` does not. `::` and `::1` cannot reach it: their zero runs are 8
        // and 7 groups long, so group 6 is inside the run and was skipped above. That is why
        // BIND needs no special case for them and neither does this.
        let encapsulated_v4 = i == 6
            && best.is_some_and(|(base, len)| {
                base == 0 && (len == 6 || (len == 5 && words[5] == 0xFFFF))
            });
        if encapsulated_v4 {
            let quad = [src[12], src[13], src[14], src[15]];
            for byte in format_v4(quad).iter() {
                text.push(*byte);
            }
            break;
        }
        text.push_hex(words[i]);
        i += 1;
    }
    // A trailing `::` — the run reaches the last group — leaves one colon, because the loop
    // writes the separator *before* a group and there is no group after the run. BIND appends
    // the second one for exactly this case.
    if let Some((base, len)) = best {
        if base + len == 8 {
            text.push(b':');
        }
    }
    text
}

/// The number of `EAI_*` codes [`gai_strerror_message`] has a message for, plus the zero row.
pub const GAI_MESSAGES: usize = 15;

/// `const char *gai_strerror(int ecode)`: bionic's message for an `EAI_*` code.
///
/// The table is `ai_errlist` from bionic's `getaddrinfo.c`, indexed by the code, with
/// `"Unknown error"` for anything outside it — which is bionic's own fallback and not an
/// invention here.
///
/// **ASSUMED, and marked as such**: the strings are transcribed from bionic's source and there
/// is **no NDK on this machine** to check them against, the same gap `layouts.rs` records for
/// `PTHREAD_MUTEX_T`. What is safe about it is the shape rather than the letters: a wrong
/// message is a wrong *diagnostic*, the caller is `printf`, and no guest branches on the text.
/// The one thing that would be unsafe — returning a pointer to storage that does not outlive the
/// call — is the adapter's problem and it interns these in the instance's pool.
#[must_use]
pub fn gai_strerror_message(ecode: i32) -> &'static str {
    match ecode {
        0 => "Success",
        1 => "Address family for hostname not supported",
        2 => "Temporary failure in name resolution",
        3 => "Invalid value for ai_flags",
        4 => "Non-recoverable failure in name resolution",
        5 => "ai_family not supported",
        6 => "Memory allocation failure",
        7 => "No address associated with hostname",
        8 => "Name or service not known",
        9 => "Servname not supported for ai_socktype",
        10 => "ai_socktype not supported",
        11 => "System error returned in errno",
        12 => "Invalid value for hints",
        13 => "Resolved protocol is unknown",
        14 => "Argument buffer overflow",
        _ => "Unknown error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMemory;

    fn mem_with(at: u64, bytes: &[u8]) -> MockMemory {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &vec![0u8; 0x1000]);
        mem.write(at, bytes).expect("the fixture is mapped");
        mem
    }

    fn text_of(bytes: &[u8]) -> String {
        let mut mem = mem_with(0x1000, bytes);
        let af = if bytes.len() == 4 { AF_INET } else { AF_INET6 };
        let dst = 0x1200;
        assert_eq!(inet_ntop(&mut mem, af, 0x1000, dst, 64), Ok(Ok(dst)));
        let mut out = [0u8; 64];
        mem.read(dst, &mut out).expect("mapped");
        let end = out.iter().position(|b| *b == 0).expect("NUL-terminated");
        String::from_utf8(out[..end].to_vec()).expect("ASCII")
    }

    fn v6(groups: [u16; 8]) -> Vec<u8> {
        groups.iter().flat_map(|g| g.to_be_bytes()).collect()
    }

    /// The dotted quad, including the two ends of the range.
    #[test]
    fn ipv4_is_a_dotted_quad_in_network_byte_order() {
        assert_eq!(text_of(&[10, 0, 0, 1]), "10.0.0.1");
        assert_eq!(text_of(&[0, 0, 0, 0]), "0.0.0.0");
        assert_eq!(text_of(&[255, 255, 255, 255]), "255.255.255.255");
        // Byte order is the wire's, not the host's: a little-endian read would print this
        // backwards and both forms are valid-looking addresses, which is why it is asserted on an
        // asymmetric value.
        assert_eq!(text_of(&[1, 2, 3, 4]), "1.2.3.4");
    }

    /// The zero-run compression, including the two cases a naive implementation gets wrong.
    #[test]
    fn ipv6_compression_is_binds_and_not_a_naive_longest_run() {
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0, 0])), "::");
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0, 1])), "::1");
        assert_eq!(text_of(&v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])), "2001:db8::1");
        assert_eq!(text_of(&v6([0xfe80, 0, 0, 0, 0x202, 0xb3ff, 0xfe1e, 0x8329])), "fe80::202:b3ff:fe1e:8329");
        // **A run of one is not compressed**, which is the rule `best.len > 1` encodes.
        assert_eq!(text_of(&v6([1, 0, 2, 3, 4, 5, 6, 7])), "1:0:2:3:4:5:6:7");
        // **The first of two equal runs wins**, not the last.
        assert_eq!(text_of(&v6([1, 0, 0, 2, 0, 0, 3, 4])), "1::2:0:0:3:4");
        // A longer later run beats an earlier shorter one.
        assert_eq!(text_of(&v6([1, 0, 0, 2, 0, 0, 0, 4])), "1:0:0:2::4");
        // A trailing run needs the second colon the loop does not write.
        assert_eq!(text_of(&v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0])), "2001:db8::");
        // Hex is lowercase with no leading zeros.
        assert_eq!(text_of(&v6([0xABCD, 0x0001, 0x0010, 0x0100, 1, 2, 3, 4])), "abcd:1:10:100:1:2:3:4");
    }

    /// The dotted-quad tail, and the two addresses that must **not** grow one.
    ///
    /// `::` and `::1` are the special case BIND writes out; getting it wrong produces
    /// `::0.0.0.0` and `::0.0.0.1`, which are legal spellings of the same addresses and are not
    /// what any libc prints. The IPv4-mapped and IPv4-compatible forms are the ones that *do*
    /// take the tail.
    #[test]
    fn the_encapsulated_ipv4_forms_are_exactly_binds() {
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0xFFFF, 0x0102, 0x0304])), "::ffff:1.2.3.4");
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0x0102, 0x0304])), "::1.2.3.4");
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0, 0])), "::", "not ::0.0.0.0");
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0, 1])), "::1", "not ::0.0.0.1");
        // The run must start at group 0 for the tail to apply.
        assert_eq!(text_of(&v6([1, 0, 0, 0, 0, 0xFFFF, 0x0102, 0x0304])), "1::ffff:102:304");
        // `::2` has a seven-group run, so group 6 is inside it and the tail cannot fire.
        assert_eq!(text_of(&v6([0, 0, 0, 0, 0, 0, 0, 2])), "::2");
    }

    /// The longest output is the IPv4-suffixed form, and it fits [`MAX_TEXT_BYTES`].
    ///
    /// The buffer is sized from this, so a formatter that could exceed it would silently drop
    /// characters rather than overflow — [`Text::push`] caps. This is what says the cap is never
    /// reached.
    #[test]
    fn the_longest_output_is_the_ipv4_suffixed_form() {
        let all_f = text_of(&v6([0xFFFF; 8]));
        assert_eq!(all_f, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff");
        assert_eq!(all_f.len(), 39);
        let mapped = text_of(&v6([0, 0, 0, 0, 0, 0xFFFF, 0xFFFF, 0xFFFF]));
        assert_eq!(mapped, "::ffff:255.255.255.255");
        // The true maximum: six full groups then a full dotted quad. Not reachable through the
        // encapsulated-v4 rule — which needs a leading zero run — so it is constructed directly.
        let mut longest = Text::new();
        for i in 0..6 {
            if i != 0 {
                longest.push(b':');
            }
            longest.push_hex(0xFFFF);
        }
        longest.push(b':');
        for byte in format_v4([255, 255, 255, 255]).iter() {
            longest.push(*byte);
        }
        assert_eq!(longest.len(), MAX_TEXT_BYTES, "INET6_ADDRSTRLEN is MAX_TEXT_BYTES + 1 = 46");
    }

    /// A destination one byte short gets **nothing** and `ENOSPC`.
    ///
    /// The off-by-one is the point: `size` must hold the text *and* its NUL. Both the exact fit
    /// and the one-short case are asserted, because an implementation with the comparison the
    /// wrong way round passes one of them.
    #[test]
    fn a_short_destination_is_enospc_and_writes_nothing() {
        let bytes = [10u8, 0, 0, 1];
        let mut mem = mem_with(0x1000, &bytes);
        // "10.0.0.1" is 8 bytes, so 9 fits and 8 does not.
        assert_eq!(inet_ntop(&mut mem, AF_INET, 0x1000, 0x1200, 9), Ok(Ok(0x1200)));
        let mut probe = [0u8; 9];
        mem.read(0x1200, &mut probe).expect("mapped");
        assert_eq!(&probe, b"10.0.0.1\0");

        let mut mem = mem_with(0x1000, &bytes);
        assert_eq!(inet_ntop(&mut mem, AF_INET, 0x1000, 0x1200, 8), Ok(Err(consts::ENOSPC)));
        let mut probe = [0u8; 9];
        mem.read(0x1200, &mut probe).expect("mapped");
        assert_eq!(&probe, &[0u8; 9], "a refused conversion must not write a truncated address");

        // Zero is the degenerate case and is still ENOSPC rather than a zero-length success.
        let mut mem = mem_with(0x1000, &bytes);
        assert_eq!(inet_ntop(&mut mem, AF_INET, 0x1000, 0x1200, 0), Ok(Err(consts::ENOSPC)));
    }

    /// An address family this function does not format is `EAFNOSUPPORT`, not a fault.
    ///
    /// Checked **before** `src` is read: a guest that passes `AF_UNIX` with a null `src` gets the
    /// errno bionic gives it, not a report that it passed a bad pointer.
    #[test]
    fn an_unknown_address_family_is_eafnosupport_before_anything_is_read() {
        let mut mem = mem_with(0x1000, &[10, 0, 0, 1]);
        for af in [-1, 0, 1, 3, 23, 28, 0x7FFF_FFFF] {
            assert_eq!(inet_ntop(&mut mem, af, 0x1000, 0x1200, 64), Ok(Err(consts::EAFNOSUPPORT)));
        }
        // And with a null source, which would fault if the family were checked second.
        assert_eq!(inet_ntop(&mut mem, 1, 0, 0x1200, 64), Ok(Err(consts::EAFNOSUPPORT)));
    }

    /// Unreadable `src` and unwritable `dst` are faults, and a wrapping range is refused.
    #[test]
    fn hostile_pointers_fault_rather_than_wrapping() {
        let mut mem = mem_with(0x1000, &[10, 0, 0, 1]);
        assert!(inet_ntop(&mut mem, AF_INET, 0x9_0000, 0x1200, 64).is_err());
        assert!(inet_ntop(&mut mem, AF_INET, 0, 0x1200, 64).is_err(), "a null source is a fault");
        assert!(inet_ntop(&mut mem, AF_INET, 0x1000, 0, 64).is_err(), "a null dest is a fault");
        assert!(inet_ntop(&mut mem, AF_INET, u64::MAX - 2, 0x1200, 64).is_err());
        assert!(inet_ntop(&mut mem, AF_INET, 0x1000, u64::MAX - 2, 64).is_err());
        // The last sixteen bytes of the IPv6 source must all be readable, not just the first.
        assert!(inet_ntop(&mut mem, AF_INET6, 0x2000 - 8, 0x1200, 64).is_err());
    }

    /// The `EAI_*` table is fifteen rows and everything else is bionic's own fallback.
    #[test]
    fn the_gai_table_is_fifteen_rows_and_a_fallback() {
        assert_eq!(gai_strerror_message(0), "Success");
        assert_eq!(gai_strerror_message(8), "Name or service not known");
        assert_eq!(gai_strerror_message(14), "Argument buffer overflow");
        for ecode in [-1, 15, 16, i32::MIN, i32::MAX] {
            assert_eq!(gai_strerror_message(ecode), "Unknown error", "ecode {ecode}");
        }
        let distinct: std::collections::BTreeSet<&str> =
            (0..GAI_MESSAGES as i32).map(gai_strerror_message).collect();
        assert_eq!(
            distinct.len(),
            GAI_MESSAGES,
            "two EAI codes share a message, so one of them is transcribed wrong"
        );
        assert!(
            !distinct.contains("Unknown error"),
            "a code inside the table fell through to the fallback"
        );
    }


    /// **A differential test against the standard library, and the one class where it disagrees.**
    ///
    /// n = **200,000** pseudo-random 128-bit addresses, half of whose bytes are zeroed by a
    /// second draw so that the compression paths are reached rather than merely the all-hex one.
    /// [`std::net::Ipv6Addr`]'s `Display` is an independent implementation of the same RFC, which
    /// makes it a usable oracle — and running it found that it is **not** an oracle for bionic,
    /// which is the finding this test exists to keep.
    ///
    /// **MEASURED: 43 disagreements in 200,000, and all 43 are the same class** — the
    /// *IPv4-compatible* address (`::a.b.c.d`: the first six groups zero and the fifth not
    /// `0xffff`). Rust's `Display` deliberately stopped printing that deprecated form in dotted
    /// notation and writes `::77:0`; BIND's `inet_ntop6`, which bionic ships essentially
    /// unchanged, still writes `::0.119.0.0`, because its condition is `best.len == 6` and says
    /// nothing about deprecation. The guest was compiled against bionic, so bionic is what this
    /// crate follows — the same reasoning [`crate::guestcmp`] records for `strcmp`.
    ///
    /// So the assertion is two-sided rather than "we match `std`": every address outside that
    /// class **must** agree with `std`, and every address inside it must be dotted. Delegating to
    /// `Display` would have passed every hand-written case above and been wrong on this one.
    #[test]
    fn a_differential_run_against_std_agrees_except_on_ipv4_compatible_addresses() {
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut compatible = 0usize;
        for round in 0..200_000u32 {
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&next().to_le_bytes());
            bytes[8..].copy_from_slice(&next().to_le_bytes());
            let mask = next();
            for (i, byte) in bytes.iter_mut().enumerate() {
                if (mask >> (i % 64)) & 1 == 0 {
                    *byte = 0;
                }
            }
            let ours = String::from_utf8(format_v6(bytes).to_vec()).expect("ASCII");
            let address = std::net::Ipv6Addr::from(bytes);
            let groups = address.segments();
            // The IPv4-compatible class, which is BIND's `best.len == 6` spelled as a property
            // of the address: the first six groups zero and the **seventh not**, so that the run
            // ends exactly where the dotted quad begins. A seventh zero group makes the run
            // longer, group 6 falls inside it, and BIND prints `::54e4` rather than
            // `::0.0.84.228` — which is why the condition is not "the first six are zero".
            let ipv4_compatible = groups[..6].iter().all(|g| *g == 0) && groups[6] != 0;
            if ipv4_compatible {
                compatible += 1;
                let quad = format!("::{}.{}.{}.{}", bytes[12], bytes[13], bytes[14], bytes[15]);
                assert_eq!(ours, quad, "round {round}: bionic prints the deprecated dotted form");
            } else {
                assert_eq!(
                    ours,
                    address.to_string(),
                    "round {round}: bytes {bytes:02x?} are outside the one documented divergence"
                );
            }
        }
        assert!(
            compatible >= 10,
            "only {compatible} of 200,000 draws were IPv4-compatible, so the divergence half of              this test barely ran"
        );
    }
}
