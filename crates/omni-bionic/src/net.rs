//! The network symbols that are pure computation: `inet_ntop`, `inet_pton` and `gai_strerror`'s
//! table.
//!
//! [`inet_pton`] joined the other two in M6 and is the only one of the three the eight below
//! does not contain — the initializers never reach it, and a guest **worker thread** calling it
//! is what found it. Its argument for living here is [`inet_ntop`]'s read backwards: parsing an
//! address is text in and sixteen bytes out, with no network in it at all.
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

// =========================================================== the other direction: `inet_pton`
//
// Added in M6, and the measurement that asked for it is a guest worker thread this layer killed:
// `GuestThreadFailure { thread: 7, start_routine: 0x2217f04 (image-relative), why: "the guest
// called the imported symbol `inet_pton` through its thunk at 0x2c6fd3e3270, and nothing in the
// compatibility layer implements it" }`. That is the same start routine `strcspn` was found on,
// which is the engine's URL and address handling; `inet_ntop` had existed since phase 3d and the
// direction the engine actually needed first was the one nothing had written.

/// The size of [`Scan`]'s cache, and therefore the highest index of `src` it can answer for.
///
/// **The parse does not need this bound and never reaches it** — which is the honest statement
/// and is worth making, because the opposite one is the believable thing to write here. Both
/// parsers below are self-limiting: `inet_pton4` refuses a value past 255 (so at most three
/// digits per octet) and a fifth octet, and `inet_pton6` refuses a fifth hex digit in a group,
/// a second `::`, and a ninth group. An unterminated `src` therefore produces **0** after a
/// handful of bytes, not a walk to the end of the mapping — the failure VERIFICATION.md entry
/// 14 records for `strchr`, which left an OpenSSL allocation and unwound while the guest held a
/// global lock. `the_scan_reads_no_further_than_the_byte_that_decides` asserts that directly,
/// with each mapping ending one byte past the character that decides.
///
/// So this is the **array's** bound and not a semantic check: [`Scan::seen`] is fixed-size, and
/// an index past it would be a host panic unwinding out of an import, which is the one failure
/// this layer exists never to produce (entry 13). Entry 12 says an unreachable `if` should be a
/// `debug_assert!` rather than kept as reassurance; that does not apply to a *total* match arm
/// standing between a guest-driven index and a panic, and the arm is [`Ch::TooLong`] rather
/// than a fabricated NUL so that even if it were reached the answer would be the right one: no
/// address of either family is 64 characters long, so the text is not an address.
///
/// **MEASURED** by `the_scans_furthest_reach_is_well_inside_its_cache`: the furthest byte any
/// input drives the scan to is **46**, against the 45 of the longest text that can succeed
/// ([`MAX_TEXT_BYTES`]).
const MAX_SCAN_BYTES: usize = 64;

/// The cache holds the longest text that can succeed **and** its terminator, so the cap can
/// never be what refuses a real address.
///
/// A **compile-time** assertion rather than a test, because both sides are constants — the same
/// form `omni_android::bionic::net` uses for its `READY_MASK` check. The runtime half of the
/// claim, that no input drives the scan anywhere near here, is measured by
/// `the_scans_furthest_reach_is_well_inside_its_cache`.
const _: () = assert!(MAX_SCAN_BYTES > MAX_TEXT_BYTES + 1);

/// One byte of `src`, or a reason there is not one.
///
/// [`Ch::End`] and [`Ch::TooLong`] are kept apart deliberately: a terminator can end a
/// *successful* parse and the cap never can. Collapsing them — returning a zero byte past the
/// cap — would let a 64-byte text terminate as though the guest had written a NUL there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ch {
    /// The byte at that index.
    Byte(u8),
    /// The NUL that ends the string.
    End,
    /// The index is past [`MAX_SCAN_BYTES`], so no address of either family lives here.
    TooLong,
}

/// A forward, byte-at-a-time read of the guest's `src`, remembering what it has read.
///
/// Two properties, and both are about guest memory rather than about parsing:
///
/// 1. **It never reads past the byte that decides the answer.** Every parse below stops on the
///    first character it can reject, and [`Scan::at`] fetches only up to the index it is asked
///    for — so `inet_pton(AF_INET, "!", ...)` reads one byte, and a `src` whose mapping ends
///    after it answers 0 rather than faulting. This is bionic's own behaviour: its loops are
///    `while ((ch = *src++) != '\0')` with a `return (0)` in the default arm.
/// 2. **It never reads past the terminator**, so a string that ends one byte before the end of
///    a mapping is read exactly to its NUL.
///
/// The cache is what makes the embedded-IPv4 form work: BIND's `inet_pton6` keeps a `curtok`
/// pointer at the start of the current group and, on seeing a `.`, re-parses from there with
/// `inet_pton4`. That rewind lands inside bytes already read, so it costs no second guest read.
struct Scan<'m, M: GuestMemory> {
    mem: &'m M,
    base: u64,
    seen: [u8; MAX_SCAN_BYTES],
    /// How many bytes of `seen` have been read from the guest. A NUL, once read, is the last.
    len: usize,
}

impl<'m, M: GuestMemory> Scan<'m, M> {
    fn new(mem: &'m M, base: u64) -> Self {
        Self { mem, base, seen: [0; MAX_SCAN_BYTES], len: 0 }
    }

    /// The byte at `index`, reading forward from the guest only as far as it must.
    ///
    /// # Errors
    ///
    /// [`Fault`] if a byte up to and including `index` is not readable, which includes a null
    /// `src` (bionic dereferences it) and a `src` so high that the walk would wrap `u64`.
    fn at(&mut self, index: usize) -> Result<Ch, Fault> {
        while self.len <= index {
            if self.len == MAX_SCAN_BYTES {
                return Ok(Ch::TooLong);
            }
            // `checked_range` rather than a bare add: a null `src` is a fault at zero and a
            // `src` near `u64::MAX` must not wrap into low memory mid-walk.
            let (at, _) = checked_range(self.base, self.len as u64 + 1)?;
            let mut byte = [0u8; 1];
            self.mem.read(at + self.len as u64, &mut byte)?;
            self.seen[self.len] = byte[0];
            self.len += 1;
            if byte[0] == 0 {
                break;
            }
        }
        // The loop can leave `len <= index` only by having read the terminator first, and no
        // caller asks for an index past one it has already been told is `End`.
        match self.seen.get(index).copied().filter(|_| index < self.len) {
            Some(0) | None => Ok(Ch::End),
            Some(byte) => Ok(Ch::Byte(byte)),
        }
    }
}

/// One hex digit's value, either case. `None` for everything else, including non-ASCII.
fn hex_value(ch: u8) -> Option<u32> {
    match ch {
        b'0'..=b'9' => Some(u32::from(ch - b'0')),
        b'a'..=b'f' => Some(u32::from(ch - b'a') + 10),
        b'A'..=b'F' => Some(u32::from(ch - b'A') + 10),
        _ => None,
    }
}

/// `inet_pton4`: four decimal octets and **nothing else**, starting at `start`.
///
/// `Ok(None)` is "not an IPv4 address", which is the whole of what `inet_pton` reports as 0.
///
/// # This is the strict parser, and every strictness is a separate refusal
///
/// `inet_pton` is not `inet_aton`, and the difference is the reason it exists. `inet_aton`
/// accepts `0x7f.1`, `0177.0.0.1` and `2130706433`; `inet_pton` accepts exactly one spelling:
///
/// * **No leading zeros.** `01.2.3.4` is refused, by BIND's `saw_digit && *tp == 0`. This is the
///   classic ambiguity rather than a stylistic rule — a reader who takes `0177.0.0.1` as octal
///   and a reader who takes it as decimal disagree about which host it names, and the pair has
///   produced real CVEs in parsers that differed from the one enforcing an allow-list.
/// * **No shorthand.** `127.1` is refused by `octets < 4`, although `inet_aton` expands it to
///   `127.0.0.1`.
/// * **No hex, no octal**, because the only accepted characters are `0`-`9` and `.`.
/// * **No surrounding space and no trailing characters**: every byte up to the terminator must
///   be one of those, so `" 1.2.3.4"` and `"1.2.3.4\n"` are both refused by the default arm.
/// * **Each octet 0-255**, refused the moment a third digit would carry it past.
///
/// # Errors
///
/// [`Fault`] if a byte the parse must look at is not readable.
fn parse_v4<M: GuestMemory>(
    scan: &mut Scan<'_, M>,
    start: usize,
) -> Result<Option<[u8; IN_ADDR_BYTES]>, Fault> {
    let mut tmp = [0u8; IN_ADDR_BYTES];
    // Which octet is being accumulated. BIND's `tp`, as an index.
    let mut octet = 0usize;
    let mut octets = 0usize;
    let mut saw_digit = false;
    let mut i = start;
    loop {
        let ch = match scan.at(i)? {
            Ch::End => break,
            Ch::TooLong => return Ok(None),
            Ch::Byte(byte) => byte,
        };
        i += 1;
        if ch.is_ascii_digit() {
            let value = u32::from(tmp[octet]) * 10 + u32::from(ch - b'0');
            // **The leading-zero refusal.** A digit after a zero octet means the octet was
            // written `0x`, and `0` followed by anything is a second spelling of a number that
            // already has one.
            if saw_digit && tmp[octet] == 0 {
                return Ok(None);
            }
            if value > 255 {
                return Ok(None);
            }
            tmp[octet] = value as u8;
            if !saw_digit {
                octets += 1;
                if octets > 4 {
                    return Ok(None);
                }
                saw_digit = true;
            }
        } else if ch == b'.' && saw_digit {
            // A fifth octet cannot start, which is also what keeps `octet` inside `tmp`.
            if octets == 4 {
                return Ok(None);
            }
            octet += 1;
            tmp[octet] = 0;
            saw_digit = false;
        } else {
            // Everything else, including a `.` with no digit before it (`".1.2.3"`, `"1..2.3"`)
            // and the space or newline a caller trimmed badly.
            return Ok(None);
        }
    }
    if octets < 4 {
        return Ok(None);
    }
    Ok(Some(tmp))
}

/// `inet_pton6`: BIND's algorithm, which is bionic's, over the same scan.
///
/// `Ok(None)` is "not an IPv6 address".
///
/// # What it accepts, and the seven shapes it refuses
///
/// Accepted: eight groups of one to four hex digits, either case, separated by `:`; **one**
/// `::` standing for one or more all-zero groups, including `::` alone for the unspecified
/// address; and a trailing dotted quad, parsed by [`parse_v4`]'s rules, in place of the last
/// two groups (`::ffff:192.0.2.1`).
///
/// Refused, each by its own line below:
///
/// 1. a group of **five or more** hex digits (`seen_xdigits > 4`), which would otherwise take
///    only its low sixteen bits and name a different address;
/// 2. a **second** `::`, which makes the number of groups each one stands for undecidable;
/// 3. a leading `:` that is not part of `::` (`":1:2:..."`);
/// 4. a trailing `:` that is not part of `::` (`"1:2:3:4:5:6:7:"`);
/// 5. a **ninth** group, by the same `tp + 2 > endp` that bounds the sixteen bytes;
/// 6. a `::` in an address that is **already eight groups long** (`tp == endp`), because it
///    would then stand for zero groups and `::` is defined to stand for at least one;
/// 7. fewer than eight groups with no `::` at all (`tp != endp`).
///
/// # Errors
///
/// [`Fault`] if a byte the parse must look at is not readable.
fn parse_v6<M: GuestMemory>(
    scan: &mut Scan<'_, M>,
) -> Result<Option<[u8; IN6_ADDR_BYTES]>, Fault> {
    let mut tmp = [0u8; IN6_ADDR_BYTES];
    // Bytes written so far is `tp`, and the end of the address is `endp` — BIND's two pointers,
    // as indices.
    let endp = IN6_ADDR_BYTES;
    let mut tp = 0usize;
    // Where the zero run sits, once one is seen. BIND's `colonp`.
    let mut colonp: Option<usize> = None;

    // **A leading `::` is the one place the scan looks ahead**, and it is refusal 3: a `:` at
    // index 0 is only legal as the first half of `::`, so the parse starts at the *second*
    // colon and the loop below sees it as an empty group.
    let mut i = 0usize;
    if scan.at(0)? == Ch::Byte(b':') {
        if scan.at(1)? != Ch::Byte(b':') {
            return Ok(None);
        }
        i = 1;
    }
    // The start of the group being read, for the dotted-quad rewind.
    let mut curtok = i;
    let mut seen_xdigits = 0u32;
    let mut val = 0u32;

    loop {
        let ch = match scan.at(i)? {
            Ch::End => break,
            Ch::TooLong => return Ok(None),
            Ch::Byte(byte) => byte,
        };
        i += 1;
        if let Some(digit) = hex_value(ch) {
            val = (val << 4) | digit;
            seen_xdigits += 1;
            // Refusal 1.
            if seen_xdigits > 4 {
                return Ok(None);
            }
            continue;
        }
        if ch == b':' {
            curtok = i;
            if seen_xdigits == 0 {
                // Refusal 2.
                if colonp.is_some() {
                    return Ok(None);
                }
                colonp = Some(tp);
                continue;
            }
            // Refusal 4: a `:` with the terminator straight after it separates nothing.
            if scan.at(i)? == Ch::End {
                return Ok(None);
            }
            // Refusal 5.
            if tp + 2 > endp {
                return Ok(None);
            }
            tmp[tp] = (val >> 8) as u8;
            tmp[tp + 1] = val as u8;
            tp += 2;
            seen_xdigits = 0;
            val = 0;
            continue;
        }
        // The embedded IPv4 tail. The quad occupies the last four bytes, so it only fits while
        // two groups are still unwritten, and it must run to the terminator — `parse_v4`
        // requires that — which is why the loop ends here rather than continuing.
        if ch == b'.' && tp + IN_ADDR_BYTES <= endp {
            if let Some(quad) = parse_v4(scan, curtok)? {
                tmp[tp..tp + IN_ADDR_BYTES].copy_from_slice(&quad);
                tp += IN_ADDR_BYTES;
                seen_xdigits = 0;
                break;
            }
        }
        return Ok(None);
    }

    if seen_xdigits != 0 {
        // Refusal 5 again, for the last group, which has no separator after it to be caught at.
        if tp + 2 > endp {
            return Ok(None);
        }
        tmp[tp] = (val >> 8) as u8;
        tmp[tp + 1] = val as u8;
        tp += 2;
    }
    if let Some(colonp) = colonp {
        // Refusal 6: an address that already fills sixteen bytes leaves `::` standing for
        // nothing, and `::` stands for one group or more.
        if tp == endp {
            return Ok(None);
        }
        // Slide everything after the run to the end, zeroing what it leaves behind. Written out
        // rather than as a rotate for the same reason BIND writes it out: the source and the
        // destination overlap.
        let n = tp - colonp;
        for k in 1..=n {
            tmp[endp - k] = tmp[colonp + n - k];
            tmp[colonp + n - k] = 0;
        }
        tp = endp;
    }
    // Refusal 7.
    if tp != endp {
        return Ok(None);
    }
    Ok(Some(tmp))
}

/// `int inet_pton(int af, const char *src, void *dst)`.
///
/// `Ok(Ok(true))` is the **1** bionic returns when `src` was converted and written to `dst`;
/// `Ok(Ok(false))` is the **0** it returns when `src` is not a valid address for `af`;
/// `Ok(Err(EAFNOSUPPORT))` is the **-1** with that errno for a family that is neither
/// `AF_INET` nor `AF_INET6`. The three are kept distinct in the type because the caller's
/// branches are distinct: 0 is a parse answer and not an error, so the adapter must not set
/// errno for it.
///
/// # `dst` is written only on success, and written once
///
/// Both parsers accumulate into a local array and only a complete address reaches guest memory
/// — which is BIND's shape (`memcpy(dst, tmp, NS_INADDRSZ)` after the last check) and is the
/// half of this function a caller can be harmed by silently. A caller that ignores the return
/// value must not find three octets of the address it asked for and one of whatever it had
/// there before: that is a *different address*, it is a plausible one, and nothing downstream
/// would say where it came from. The same argument [`inet_ntop`] makes for refusing to
/// truncate, in the direction where the bytes are binary and nobody will read them.
///
/// The family is checked **before** `src` is read, as in [`inet_ntop`] and as in bionic's own
/// `switch (af)`, so `inet_pton(AF_UNIX, NULL, dst)` is `EAFNOSUPPORT` rather than a fault.
///
/// # Errors
///
/// [`Fault`] if a byte of `src` the parse must look at is not readable — which includes a null
/// `src`, since bionic dereferences one — or if the four or sixteen bytes at `dst` are not
/// writable. A `src` that is merely *unterminated* is not a fault: see [`MAX_SCAN_BYTES`].
pub fn inet_pton(
    mem: &mut impl GuestMemory,
    af: i32,
    src: u64,
    dst: u64,
) -> Result<Result<bool, i32>, Fault> {
    let mut address = [0u8; IN6_ADDR_BYTES];
    let len = match af {
        AF_INET => {
            let mut scan = Scan::new(&*mem, src);
            match parse_v4(&mut scan, 0)? {
                Some(quad) => {
                    address[..IN_ADDR_BYTES].copy_from_slice(&quad);
                    IN_ADDR_BYTES
                }
                None => return Ok(Ok(false)),
            }
        }
        AF_INET6 => {
            let mut scan = Scan::new(&*mem, src);
            match parse_v6(&mut scan)? {
                Some(bytes) => {
                    address.copy_from_slice(&bytes);
                    IN6_ADDR_BYTES
                }
                None => return Ok(Ok(false)),
            }
        }
        // Every other family, including `AF_UNSPEC` (0) and `AF_UNIX` (1) — bionic's default
        // arm, and the only case of the three that sets errno.
        _ => return Ok(Err(consts::EAFNOSUPPORT)),
    };
    // Checked as one range before anything is written, exactly as `inet_ntop` checks `dst`.
    checked_range(dst, len as u64)?;
    mem.write(dst, &address[..len])?;
    Ok(Ok(true))
}

/// The number of `EAI_*` codes [`gai_strerror_message`] has a message for, plus the zero row.
pub const GAI_MESSAGES: usize = 15;

/// `EAI_NONAME` — "Name or service not known", the code a resolver returns for a host that does
/// not resolve.
///
/// Android's `netdb.h` numbers the `EAI_*` codes from 1 upwards, where glibc numbers them
/// downwards from -1, so this is one of the constants where taking the host's value would be
/// silently wrong. **It is corroborated inside this file**: [`gai_strerror_message`]'s table is
/// bionic's own `ai_errlist` and row 8 is "Name or service not known", which is the string
/// `EAI_NONAME` carries. A numbering that was wrong would not put that message at that index.
pub const EAI_NONAME: i32 = 8;

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


    // ------------------------------------------------------------------------- `inet_pton`

    /// Put `text` (NUL-terminated) at 0x1000 with a poisoned 16-byte destination at 0x1200.
    ///
    /// The poison is the point: every assertion below that a parse failed also asserts the
    /// destination is **untouched**, and a zeroed destination could not tell "wrote nothing"
    /// from "wrote the unspecified address".
    const POISON: u8 = 0xA5;

    fn pton_mem(text: &[u8]) -> MockMemory {
        let mut mem = MockMemory::new();
        mem.map(0x1000, &vec![0u8; 0x1000]);
        mem.write(0x1000, text).expect("the fixture is mapped");
        mem.write(0x1200, &[POISON; IN6_ADDR_BYTES]).expect("the fixture is mapped");
        mem
    }

    /// Parse `text` for `af`, asserting the destination is untouched whenever it fails.
    ///
    /// Returns the bytes written on success, `None` on a 0 from `inet_pton`.
    fn pton(af: i32, text: &str) -> Option<Vec<u8>> {
        let mut with_nul = text.as_bytes().to_vec();
        with_nul.push(0);
        let mut mem = pton_mem(&with_nul);
        let len = if af == AF_INET { IN_ADDR_BYTES } else { IN6_ADDR_BYTES };
        let answer = inet_pton(&mut mem, af, 0x1000, 0x1200).expect("the fixture is mapped");
        let mut out = [0u8; IN6_ADDR_BYTES];
        mem.read(0x1200, &mut out).expect("mapped");
        match answer {
            Ok(true) => Some(out[..len].to_vec()),
            Ok(false) => {
                assert_eq!(
                    out,
                    [POISON; IN6_ADDR_BYTES],
                    "`{text}` was refused and still wrote to dst: a caller that ignores the \
                     return value would read a different address"
                );
                None
            }
            Err(errno) => panic!("`{text}` for af {af} answered errno {errno}"),
        }
    }

    fn v4(text: &str) -> Option<Vec<u8>> {
        pton(AF_INET, text)
    }

    fn p6(text: &str) -> Option<Vec<u8>> {
        pton(AF_INET6, text)
    }

    /// Expected sixteen bytes from eight groups.
    fn bytes6(groups: [u16; 8]) -> Vec<u8> {
        groups.iter().flat_map(|g| g.to_be_bytes()).collect()
    }

    /// The four accepted IPv4 spellings, in network order.
    ///
    /// Byte order is asserted on an asymmetric value for the same reason [`inet_ntop`]'s test
    /// is: `1.2.3.4` reversed is also a valid-looking address.
    #[test]
    fn ipv4_accepts_four_decimal_octets_in_network_byte_order() {
        assert_eq!(v4("1.2.3.4"), Some(vec![1, 2, 3, 4]));
        assert_eq!(v4("0.0.0.0"), Some(vec![0, 0, 0, 0]));
        assert_eq!(v4("255.255.255.255"), Some(vec![255, 255, 255, 255]));
        assert_eq!(v4("10.0.0.1"), Some(vec![10, 0, 0, 1]));
        assert_eq!(v4("192.0.2.1"), Some(vec![192, 0, 2, 1]));
        // A single zero octet is legal; it is a zero with anything *after* it that is not.
        assert_eq!(v4("0.10.0.255"), Some(vec![0, 10, 0, 255]));
    }

    /// **The strictness, rule by rule — `inet_pton` is not `inet_aton`.**
    ///
    /// Each line names the form `inet_aton` would accept and the address it would produce, so
    /// that a future implementation that "helpfully" widened this parser fails here with the
    /// reason written beside it. POSIX says `inet_pton` accepts only the dotted-decimal
    /// notation of four octets; everything below is outside it.
    #[test]
    fn ipv4_refuses_every_form_inet_aton_would_have_widened_to() {
        // **Leading zeros.** `inet_aton` reads `01` as octal 1 and `010` as octal 8, so the two
        // readings of `010.1.1.1` name different hosts. This is the ambiguity behind the
        // parser-disagreement CVEs, and refusing the spelling outright is the only fix that
        // does not depend on which reading the *other* parser chose.
        assert_eq!(v4("01.2.3.4"), None);
        assert_eq!(v4("1.02.3.4"), None);
        assert_eq!(v4("1.2.3.04"), None);
        assert_eq!(v4("010.1.1.1"), None);
        assert_eq!(v4("00.0.0.0"), None);
        assert_eq!(v4("0.0.0.00"), None);
        // Shorthand. `inet_aton("127.1")` is 127.0.0.1 and `inet_aton("127")` is 0.0.0.127.
        assert_eq!(v4("127.1"), None);
        assert_eq!(v4("127"), None);
        assert_eq!(v4("1.2.3"), None);
        assert_eq!(v4("1.2"), None);
        // Hex and octal prefixes: `inet_aton("0x7f.0.0.1")` is 127.0.0.1.
        assert_eq!(v4("0x7f.0.0.1"), None);
        assert_eq!(v4("0X1.2.3.4"), None);
        assert_eq!(v4("1.2.3.0xff"), None);
        // Too many octets, and an octet out of range.
        assert_eq!(v4("1.2.3.4.5"), None);
        assert_eq!(v4("256.1.1.1"), None);
        assert_eq!(v4("1.1.1.256"), None);
        assert_eq!(v4("999.1.1.1"), None);
        assert_eq!(v4("1.2.3.1000"), None);
        // Surrounding space and trailing characters. A caller that trimmed badly must be told.
        assert_eq!(v4(" 1.2.3.4"), None);
        assert_eq!(v4("1.2.3.4 "), None);
        assert_eq!(v4("1.2.3.4\n"), None);
        assert_eq!(v4("\t1.2.3.4"), None);
        assert_eq!(v4("1.2.3.4:80"), None);
        assert_eq!(v4("+1.2.3.4"), None);
        assert_eq!(v4("-1.2.3.4"), None);
        // Misplaced and doubled dots, and the empty string.
        assert_eq!(v4(".1.2.3"), None);
        assert_eq!(v4("1..2.3"), None);
        assert_eq!(v4("1.2.3."), None);
        assert_eq!(v4(""), None);
        assert_eq!(v4("."), None);
        // An IPv6 address is not an IPv4 address, whichever way round it is asked.
        assert_eq!(v4("::1"), None);
        assert_eq!(v4("::ffff:1.2.3.4"), None);
    }

    /// The accepted IPv6 spellings: full, compressed, and the embedded quad.
    #[test]
    fn ipv6_accepts_full_compressed_and_embedded_ipv4_forms() {
        assert_eq!(p6("1:2:3:4:5:6:7:8"), Some(bytes6([1, 2, 3, 4, 5, 6, 7, 8])));
        assert_eq!(
            p6("2001:db8:85a3:0:0:8a2e:370:7334"),
            Some(bytes6([0x2001, 0xdb8, 0x85a3, 0, 0, 0x8a2e, 0x370, 0x7334]))
        );
        // `::` alone is the unspecified address: sixteen zero bytes, not a refusal.
        assert_eq!(p6("::"), Some(vec![0u8; 16]));
        assert_eq!(p6("::1"), Some(bytes6([0, 0, 0, 0, 0, 0, 0, 1])));
        assert_eq!(p6("1::"), Some(bytes6([1, 0, 0, 0, 0, 0, 0, 0])));
        assert_eq!(p6("2001:db8::1"), Some(bytes6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1])));
        assert_eq!(p6("1::8"), Some(bytes6([1, 0, 0, 0, 0, 0, 0, 8])));
        assert_eq!(p6("1:2::7:8"), Some(bytes6([1, 2, 0, 0, 0, 0, 7, 8])));
        // A `::` standing for exactly one group, which is legal input although `inet_ntop`
        // would never *print* it that way.
        assert_eq!(p6("1:2:3:4:5:6::8"), Some(bytes6([1, 2, 3, 4, 5, 6, 0, 8])));
        assert_eq!(p6("1::3:4:5:6:7:8"), Some(bytes6([1, 0, 3, 4, 5, 6, 7, 8])));
        // Hex is case-insensitive, and a group may be one to four digits with leading zeros —
        // which IS legal here, unlike an IPv4 octet, because hex groups are fixed-width fields.
        assert_eq!(p6("ABCD:ef01:0001:0010:0100:1000:000a:000B"), Some(bytes6([
            0xabcd, 0xef01, 1, 0x10, 0x100, 0x1000, 0xa, 0xb
        ])));
        assert_eq!(p6("fe80::202:b3ff:fe1e:8329"), Some(bytes6([
            0xfe80, 0, 0, 0, 0x202, 0xb3ff, 0xfe1e, 0x8329
        ])));
        assert_eq!(p6("ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"), Some(vec![0xFF; 16]));
    }

    /// The embedded IPv4 tail, and that it is parsed by **[`parse_v4`]'s** rules and not looser
    /// ones.
    ///
    /// This is the one place the two parsers meet, and it is where a second, laxer IPv4 parser
    /// would hide: `::ffff:010.0.0.1` must be refused for exactly the reason `010.0.0.1` is.
    #[test]
    fn the_embedded_ipv4_tail_uses_the_strict_octet_rules() {
        assert_eq!(p6("::ffff:192.0.2.1"), Some(bytes6([0, 0, 0, 0, 0, 0xffff, 0xc000, 0x201])));
        assert_eq!(p6("::1.2.3.4"), Some(bytes6([0, 0, 0, 0, 0, 0, 0x102, 0x304])));
        assert_eq!(p6("::ffff:0:1.2.3.4"), Some(bytes6([0, 0, 0, 0, 0xffff, 0, 0x102, 0x304])));
        assert_eq!(
            p6("1:2:3:4:5:6:1.2.3.4"),
            Some(bytes6([1, 2, 3, 4, 5, 6, 0x102, 0x304])),
            "six groups then a quad is the longest form there is"
        );
        // The strict octet rules, inherited rather than re-stated.
        assert_eq!(p6("::ffff:010.0.0.1"), None, "a leading zero is a leading zero here too");
        assert_eq!(p6("::ffff:256.0.0.1"), None);
        assert_eq!(p6("::ffff:1.2.3"), None, "no shorthand inside the tail either");
        assert_eq!(p6("::ffff:1.2.3.4.5"), None);
        assert_eq!(p6("::ffff:0x1.2.3.4"), None);
        // A quad with seven groups before it has nowhere to go: it needs the last four bytes.
        assert_eq!(p6("1:2:3:4:5:6:7:1.2.3.4"), None);
        // A bare quad is not an IPv6 address, although every character in it is accepted.
        assert_eq!(p6("1.2.3.4"), None);
        // A quad that is not last.
        assert_eq!(p6("::1.2.3.4:5"), None);
        assert_eq!(p6("1.2.3.4::"), None);
    }

    /// **The seven IPv6 refusals, one assertion group each.**
    ///
    /// Written as the doc comment on [`parse_v6`] numbers them, so a row that stops firing can
    /// be traced to the line it came from rather than to "some IPv6 test".
    #[test]
    fn ipv6_refuses_each_of_the_seven_malformed_shapes() {
        // 1. Five or more hex digits in a group. Taking the low sixteen bits would make
        //    `12345::` parse as `2345::`, which is a real address and the wrong one.
        assert_eq!(p6("12345::"), None);
        assert_eq!(p6("1:2:3:4:5:6:7:12345"), None);
        assert_eq!(p6("00000::1"), None);
        // 2. Two `::`. How many groups each stands for is undecidable, so there is no address
        //    to return.
        assert_eq!(p6("1::2::3"), None);
        assert_eq!(p6("::1::"), None);
        assert_eq!(p6("::::"), None);
        assert_eq!(p6(":::"), None);
        // 3. A leading `:` that is not part of `::`.
        assert_eq!(p6(":1:2:3:4:5:6:7"), None);
        assert_eq!(p6(":"), None);
        assert_eq!(p6(":1::"), None);
        // 4. A trailing `:` that is not part of `::`.
        assert_eq!(p6("1:2:3:4:5:6:7:"), None);
        assert_eq!(p6("1::2:"), None);
        assert_eq!(p6("1:"), None);
        // 5. A ninth group.
        assert_eq!(p6("1:2:3:4:5:6:7:8:9"), None);
        assert_eq!(p6("1:2:3:4:5:6:7:8:"), None);
        // 6. A `::` in an address that is already eight groups long. `::` stands for **one or
        //    more** zero groups, so here it stands for none and the text is not an address —
        //    the case a "just skip the empty group" implementation accepts.
        assert_eq!(p6("1:2:3:4:5:6:7:8::"), None);
        assert_eq!(p6("::1:2:3:4:5:6:7:8"), None);
        assert_eq!(p6("1:2:3:4::5:6:7:8"), None);
        // 7. Fewer than eight groups with no `::` at all.
        assert_eq!(p6("1:2:3:4:5:6:7"), None);
        assert_eq!(p6("1"), None);
        assert_eq!(p6(""), None);
        // And the characters that are in neither alphabet.
        assert_eq!(p6("g::1"), None);
        assert_eq!(p6("1::2 "), None);
        assert_eq!(p6(" ::1"), None);
        assert_eq!(p6("1::2%eth0"), None, "a scope id is `getaddrinfo`'s job, not this one");
        assert_eq!(p6("[::1]"), None);
        assert_eq!(p6("::1/128"), None);
    }

    /// **A refused parse writes nothing at all** — asserted on `dst` rather than on the answer.
    ///
    /// [`pton`] checks this for every `None` above, so this test is the one that would fail if
    /// [`pton`] itself stopped checking: it reads the destination directly, for the failure
    /// that gets furthest before it gives up. `1:2:3:4:5:6:7:8:9` fills all sixteen bytes of
    /// the local array and is refused by the ninth group, and `::ffff:1.2.3.999` is refused
    /// inside the embedded quad with twelve bytes already accumulated — an implementation that
    /// wrote through to `dst` as it went would leave both looking like real addresses.
    #[test]
    fn a_refused_parse_leaves_the_destination_exactly_as_it_was() {
        for text in ["1:2:3:4:5:6:7:8:9", "::ffff:1.2.3.999", "1:2:3:4:5:6:7:8::", "01.2.3.4"] {
            let af = if text.contains(':') { AF_INET6 } else { AF_INET };
            let mut with_nul = text.as_bytes().to_vec();
            with_nul.push(0);
            let mut mem = pton_mem(&with_nul);
            assert_eq!(
                inet_pton(&mut mem, af, 0x1000, 0x1200),
                Ok(Ok(false)),
                "`{text}` must be refused"
            );
            let mut out = [0u8; IN6_ADDR_BYTES];
            mem.read(0x1200, &mut out).expect("mapped");
            assert_eq!(out, [POISON; IN6_ADDR_BYTES], "`{text}` wrote to dst and must not have");
        }
        // The converse, so that the poison is known to be observable: a success does write.
        let mut mem = pton_mem(b"1.2.3.4\0");
        assert_eq!(inet_pton(&mut mem, AF_INET, 0x1000, 0x1200), Ok(Ok(true)));
        let mut out = [0u8; IN6_ADDR_BYTES];
        mem.read(0x1200, &mut out).expect("mapped");
        assert_eq!(&out[..4], &[1, 2, 3, 4]);
        assert_eq!(&out[4..], &[POISON; 12], "AF_INET writes four bytes, not sixteen");
    }

    /// An address family this function does not parse is `EAFNOSUPPORT`, checked before `src`.
    #[test]
    fn an_unknown_family_is_eafnosupport_before_src_is_read() {
        let mut mem = pton_mem(b"1.2.3.4\0");
        for af in [-1, 0, 1, 3, 23, 28, 0x7FFF_FFFF] {
            assert_eq!(inet_pton(&mut mem, af, 0x1000, 0x1200), Ok(Err(consts::EAFNOSUPPORT)));
        }
        // With a null `src`, which would fault if the family were checked second.
        assert_eq!(inet_pton(&mut mem, 1, 0, 0x1200), Ok(Err(consts::EAFNOSUPPORT)));
        // And nothing was written for any of them.
        let mut out = [0u8; IN6_ADDR_BYTES];
        mem.read(0x1200, &mut out).expect("mapped");
        assert_eq!(out, [POISON; IN6_ADDR_BYTES]);
    }

    /// **The scan stops at the byte that decides the answer, and not one further.**
    ///
    /// The detector, not a watch: each case maps the text so that its mapping **ends** right
    /// after the deciding byte, so a parser that read one byte more would fault instead of
    /// answering. VERIFICATION.md entry 14 is the whole reason this is asserted — `strchr`
    /// walking past its terminator left an OpenSSL allocation and deadlocked the runtime, and
    /// a parser is the same walk with more ways to keep going.
    #[test]
    fn the_scan_reads_no_further_than_the_byte_that_decides() {
        // `(text without a terminator, family, how many bytes may be read)`.
        let cases: &[(&[u8], i32, usize)] = &[
            // The first character is not a digit, so `inet_pton4` is done after one byte.
            (b"!", AF_INET, 1),
            // `01` is refused at the second byte by the leading-zero rule.
            (b"01", AF_INET, 2),
            // `256` is refused at the third digit, before any dot.
            (b"256", AF_INET, 3),
            // A leading `:` not followed by `:` is refused after two bytes.
            (b":1", AF_INET6, 2),
            // A fifth hex digit.
            (b"12345", AF_INET6, 5),
            // A second `::`.
            (b"1::2::", AF_INET6, 6),
            // A character in neither alphabet.
            (b"1:2:g", AF_INET6, 5),
        ];
        for (text, af, reach) in cases {
            let mut mem = MockMemory::new();
            // Exactly `reach` bytes mapped and **no terminator**: the byte after the last one
            // the parse may read is unmapped, so reading it is a fault the assertion catches.
            mem.map(0x1000, &text[..*reach]);
            mem.map(0x2000, &[0u8; 16]);
            assert_eq!(
                inet_pton(&mut mem, *af, 0x1000, 0x2000),
                Ok(Ok(false)),
                "`{}` must be decided within {reach} bytes, not walked past",
                String::from_utf8_lossy(text)
            );
        }
    }

    /// **An unterminated `src` is an answer, not a fault and not a walk.**
    ///
    /// A guest that hands this function a page of plausible characters with no NUL anywhere in
    /// it must get 0. The mapping is exactly one page with nothing after it, so a parser that
    /// kept going while the characters stayed in its alphabet would fault at the page's end
    /// rather than answer — and the four fills are chosen to be in the alphabet: digits and
    /// dots for `AF_INET`, hex digits and colons for `AF_INET6`.
    #[test]
    fn an_unterminated_src_is_refused_rather_than_walked_to_the_end_of_the_mapping() {
        for (af, fill) in [(AF_INET, b'1'), (AF_INET6, b'a'), (AF_INET, b'.'), (AF_INET6, b':')] {
            let mut mem = MockMemory::new();
            mem.map(0x1000, &vec![fill; 0x1000]);
            mem.map(0x3000, &[0u8; 16]);
            assert_eq!(
                inet_pton(&mut mem, af, 0x1000, 0x3000),
                Ok(Ok(false)),
                "4096 bytes of `{}` with no terminator, af {af}",
                fill as char
            );
        }
    }

    /// A [`GuestMemory`] that records the highest offset from `base` anyone read.
    ///
    /// Read-only interposition: `write` goes straight through, so the destination checks in the
    /// tests above behave identically.
    struct Watched {
        inner: MockMemory,
        base: u64,
        furthest: std::cell::Cell<Option<u64>>,
    }

    impl GuestMemory for Watched {
        fn read(&self, addr: u64, buf: &mut [u8]) -> Result<(), Fault> {
            if addr >= self.base && !buf.is_empty() {
                let last = addr - self.base + buf.len() as u64 - 1;
                let high = self.furthest.get().map_or(last, |seen| seen.max(last));
                self.furthest.set(Some(high));
            }
            self.inner.read(addr, buf)
        }

        fn write(&mut self, addr: u64, buf: &[u8]) -> Result<(), Fault> {
            self.inner.write(addr, buf)
        }
    }

    /// **The furthest byte of `src` any input drives the scan to, measured rather than argued.**
    ///
    /// [`MAX_SCAN_BYTES`] is the size of a fixed array that a guest-supplied string indexes
    /// into, so "the parse cannot reach the end of it" is a claim that has to be checked rather
    /// than reasoned about in a comment — VERIFICATION.md entry 14 is what a reasoned-about
    /// claim in this exact position cost. The corpus is the shapes that get furthest before
    /// they are decided: the longest text that succeeds, the longest that fails, the
    /// dotted-quad tail (which **re-reads** from the start of its group, so it reaches further
    /// than the character that triggered it), and a ninth group.
    ///
    /// **MEASURED: 46**, with the cap at 64.
    #[test]
    fn the_scans_furthest_reach_is_well_inside_its_cache() {
        let corpus = [
            // The longest text that can succeed: six groups then a full quad, 45 characters.
            (AF_INET6, "ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.255"),
            // The same with a `::`, which spends a character without writing a group and so
            // pushes the quad's re-read one byte further right.
            (AF_INET6, "ffff::ffff:ffff:ffff:ffff:255.255.255.255"),
            (AF_INET6, "1::ffff:ffff:ffff:ffff:ffff:255.255.255.255"),
            // A dotted tail that fails at its last octet, so the re-read runs to the end.
            (AF_INET6, "ffff:ffff:ffff:ffff:ffff:ffff:255.255.255.999"),
            // Eight full groups, then a ninth.
            (AF_INET6, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            (AF_INET6, "::ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff"),
            (AF_INET6, "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff:fffff"),
            // The longest IPv4 text, and the longest that fails at its last character.
            (AF_INET, "255.255.255.255"),
            (AF_INET, "255.255.255.255.255"),
            (AF_INET, "255.255.255.256"),
        ];
        let mut furthest = 0u64;
        for (af, text) in corpus {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            let mut inner = MockMemory::new();
            inner.map(0x1000, &bytes);
            inner.map(0x2000, &[0u8; 16]);
            let mut mem = Watched { inner, base: 0x1000, furthest: std::cell::Cell::new(None) };
            // Every one of these must be *decided*, not faulted: the mapping ends at the NUL,
            // so a scan that read one byte past it would fail here first.
            assert!(
                inet_pton(&mut mem, af, 0x1000, 0x2000).is_ok(),
                "`{text}` read past its own terminator"
            );
            let reach = mem.furthest.get().expect("src was read");
            furthest = furthest.max(reach);
        }
        assert_eq!(furthest, 46, "the measured reach changed; the constant's doc quotes it");
        assert!(
            (furthest as usize) < MAX_SCAN_BYTES,
            "the scan can reach its cache's end, where the answer would come from the cap \
             rather than from the address"
        );
        // The other half of the claim -- that the cap is above the longest text that can
        // succeed -- is both constants and is asserted at compile time beside the constant.
    }

    /// Hostile pointers fault rather than wrapping, as they do for [`inet_ntop`].
    #[test]
    fn pton_hostile_pointers_fault() {
        let mut mem = pton_mem(b"1.2.3.4\0");
        assert!(inet_pton(&mut mem, AF_INET, 0, 0x1200).is_err(), "a null source is a fault");
        assert!(inet_pton(&mut mem, AF_INET, 0x9_0000, 0x1200).is_err());
        assert!(inet_pton(&mut mem, AF_INET, u64::MAX - 2, 0x1200).is_err());
        assert!(inet_pton(&mut mem, AF_INET, 0x1000, 0).is_err(), "a null dest is a fault");
        assert!(inet_pton(&mut mem, AF_INET, 0x1000, u64::MAX - 2).is_err());
        // The last of the sixteen destination bytes must be writable, not just the first.
        let mut mem = pton_mem(b"::1\0");
        assert!(inet_pton(&mut mem, AF_INET6, 0x1000, 0x2000 - 8).is_err());
        // A destination four bytes from the end of the mapping is fine for AF_INET and not for
        // AF_INET6, which is the same range check seen from the other side.
        let mut mem = pton_mem(b"1.2.3.4\0");
        assert_eq!(inet_pton(&mut mem, AF_INET, 0x1000, 0x2000 - 4), Ok(Ok(true)));
    }

    /// **A round trip through [`inet_ntop`], over 200,000 pseudo-random addresses.**
    ///
    /// Not a differential test and deliberately not one: the two functions are in this file and
    /// agreeing with itself proves nothing about bionic (VERIFICATION.md entry 7). What it
    /// *does* prove is the property a guest depends on and neither function can check alone —
    /// **`inet_pton(inet_ntop(a)) == a` for every address** — so a group written to the wrong
    /// byte, a compression run reinflated at the wrong offset, or an endianness flip in either
    /// direction shows up as a mismatch on an exact address rather than as a hand-written case
    /// somebody thought to try. The `::` reinflation in particular is a hand-written loop over
    /// overlapping bytes, and the hand-written cases above reach only a few of its offsets.
    ///
    /// n = 200,000, half the bytes zeroed by a second draw so the compressed and dotted forms
    /// are reached rather than only the all-hex one, and both counts are asserted to be
    /// non-trivial so that the interesting half cannot quietly stop running.
    #[test]
    fn every_address_inet_ntop_prints_is_one_inet_pton_reads_back() {
        let mut state: u64 = 0x0fed_cba9_8765_4321;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut compressed = 0usize;
        let mut dotted = 0usize;
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
            let text = String::from_utf8(format_v6(bytes).to_vec()).expect("ASCII");
            if text.contains("::") {
                compressed += 1;
            }
            if text.contains('.') {
                dotted += 1;
            }
            assert_eq!(
                p6(&text),
                Some(bytes.to_vec()),
                "round {round}: inet_ntop printed `{text}` for {bytes:02x?}"
            );
        }
        assert!(compressed >= 1000, "only {compressed} of 200,000 used `::`");
        assert!(dotted >= 10, "only {dotted} of 200,000 took the dotted tail");
        // The same round trip for IPv4, where every one of the 2^32 addresses has exactly one
        // spelling — so a sample of the four extremes and a stride across the space is enough.
        for seed in (0..=u32::MAX).step_by(0x0001_0007) {
            let bytes = seed.to_be_bytes();
            let text = String::from_utf8(format_v4(bytes).to_vec()).expect("ASCII");
            assert_eq!(v4(&text), Some(bytes.to_vec()), "`{text}`");
        }
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
