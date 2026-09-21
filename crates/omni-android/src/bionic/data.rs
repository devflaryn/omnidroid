//! The eighteen `STT_OBJECT` imports: placement, size, and contents.
//!
//! These are not functions. The guest loads from them, and a relocation has already written their
//! addresses into its `GOT`, so there is nothing to dispatch and nothing to refuse at call time —
//! whatever bytes are here *are* the answer. That makes them the one part of the compatibility
//! layer where Global Constraint 1's "no plausible value" has to be satisfied by choosing the
//! right bytes rather than by raising an error.
//!
//! # Deriving the set rather than trusting a count
//!
//! D17 scopes the reachable imports at 170 functions plus **18 `STT_OBJECT` data objects**, and
//! that number is right. The *membership* recorded alongside it was not: `crates/omni-android/
//! tests/libroblox.rs` listed `timezone` and `tzname`, which the reachable-import list puts in its
//! "never referenced from the Tier C closure at all" section, and omitted
//! `AMEDIAFORMAT_KEY_STRIDE` and `AMEDIAFORMAT_KEY_WIDTH`, which it does not. Two wrong, two
//! missing, count unchanged — which is exactly why a count is not a specification.
//!
//! The set here is derived: every symbol in the first six sections of
//! `docs/research/init-reachable-imports.txt` whose `.dynsym` entry in `libroblox.so` is
//! `STT_OBJECT`. `crates/omni-android/tests/libroblox.rs` re-derives it from the real library and
//! fails if this table disagrees.
//!
//! # Sizes: three that are exact, one that is derived, and why the derived one is safe
//!
//! * `in6addr_any` and `in6addr_loopback` are `struct in6_addr`, sixteen bytes, contents fixed by
//!   RFC 4291 and by `IN6ADDR_ANY_INIT` / `IN6ADDR_LOOPBACK_INIT`.
//! * `__stack_chk_guard` is a `uintptr_t`, and its *value* is load-bearing: D13 programs the same
//!   canary into `TPIDR_EL0 + 0x28` for every guest thread, and a function that loads the global
//!   form must see the same number as one that loads the TLS form. [`GuestProcess::stack_guard`]
//!   carries it and a zero is refused, because a zero canary compares equal to a zeroed stack slot
//!   and `omni-cpu` refuses to generate one for the same reason.
//! * `environ`, `stdin`, `stdout`, `stderr` and the ten `AMEDIAFORMAT_KEY_*` are pointers: eight
//!   bytes each, pointing at something that has to exist.
//! * `__sF` is the derived one — see [`FILE_BYTES`].
//!
//! # What was measured, and what it corrects
//!
//! [`BoundaryBuilder::declare_data`](crate::BoundaryBuilder::declare_data) is documented against
//! the worry that "`__sF` is an array of three `FILE`s that the guest reaches as `__sF + addend`,
//! so a pointer-sized cell would be silently too small". **Measured against the real library: it
//! does not, and neither does anything else.** Each of the eighteen has exactly one relocation
//! against it — `R_AARCH64_GLOB_DAT` (type 1025) — and every addend is **zero**. The `GOT` holds
//! the object's base and any subscripting happens in guest code at run time.
//!
//! That does not make the size irrelevant, it moves where it matters: `&__sF[2]` is `__sF + 2 *
//! sizeof(FILE)` computed by an instruction rather than by the loader, so the object still has to
//! be three `FILE`s wide or the third stream overlaps whatever follows. The reasoning the doc
//! comment gives is sound; only its evidence was wrong, and it is recorded here rather than
//! quietly fixed.

use std::sync::Arc;

use omni_mem::GuestAddr;

use crate::boundary::BoundaryBuilder;
use crate::error::{AbiError, AbiResult};
use crate::mem::{Blame, GuestMem};

use super::Bionic;

/// Bytes of one bionic `FILE` on LP64.
///
/// **Derived from bionic's `struct __sFILE`, field by field, and not verified against an NDK** —
/// there is none on this machine. The arithmetic:
///
/// | offset | field |
/// |---|---|
/// | 0 | `unsigned char *_p` |
/// | 8 | `int _r`, `int _w` |
/// | 16 | `short _flags`, `short _file`, 4 bytes of padding |
/// | 24 | `struct __sbuf _bf` — a pointer and a `size_t` |
/// | 40 | `int _lbfsize`, 4 bytes of padding |
/// | 48 | `void *_cookie` |
/// | 56 | `_close`, `_read`, `_seek`, `_write` — four function pointers |
/// | 88 | `struct __sbuf _ext` |
/// | 104 | `unsigned char *_up` |
/// | 112 | `int _ur`, `_ubuf[3]`, `_nbuf[1]` |
/// | 120 | `struct __sbuf _lb` |
/// | 136 | `int _blksize`, 4 bytes of padding |
/// | 144 | `fpos_t _offset` |
/// | **152** | end |
///
/// **Why an error here cannot be silent, which is the only reason it is acceptable to state a
/// derived number.** A `FILE`'s contents are opaque: every function that would interpret them is
/// `Unbound` or refuses by name.
///
/// **That sentence used to name `fclose`, `fread`, `fwrite` and `fopen` as "none is implemented".
/// Phase 3b implemented all four**, so the evidence given here was stale for a whole phase while
/// the conclusion stayed true for a *different* reason — the stronger one `bionic/stdio.rs` states:
/// a `FILE *` is a **key into a host-side table**, its bytes are written once to zero and never
/// read, so a wrong size yields a wrong *address* that refuses by name rather than a wrong answer.
/// So the only thing a wrong stride can make wrong is the arithmetic `stdout == &__sF[1]`, and
/// that arithmetic is wrong **consistently**: this module places `stdout` at `__sF + FILE_BYTES`
/// using the same number guest code would use, so the two agree with each other whatever the real
/// value is. The first code that actually reads a field out of a `FILE` is the phase that
/// implements stdio, and it is the phase that must confirm this number against a real header.
pub const FILE_BYTES: usize = 152;

/// One `STT_OBJECT` import: how much room it needs and how it must be aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataObject {
    /// The symbol name, exactly as `.dynstr` spells it.
    pub symbol: &'static str,
    /// Bytes of the data area it needs.
    pub len: usize,
    /// Alignment its address must have.
    pub align: usize,
}

/// The ten `AMEDIAFORMAT_KEY_*` imports and the key strings they point at.
///
/// These are `const char *` variables in `libmediandk`, and the strings are the ones
/// `android.media.MediaFormat`'s `KEY_*` constants carry — a published, stable part of the
/// platform. Handing the guest a null or a wrong string here would be a silent wrong answer: the
/// engine passes them to `AMediaFormat_setInt32` and friends, every one of which is `Unbound`, so
/// the *use* fails by name — but a `strcmp` against one of these, or a hash of it, would not.
pub const MEDIA_FORMAT_KEYS: [(&str, &str); 10] = [
    ("AMEDIAFORMAT_KEY_BIT_RATE", "bitrate"),
    ("AMEDIAFORMAT_KEY_CHANNEL_COUNT", "channel-count"),
    ("AMEDIAFORMAT_KEY_COLOR_FORMAT", "color-format"),
    ("AMEDIAFORMAT_KEY_FRAME_RATE", "frame-rate"),
    ("AMEDIAFORMAT_KEY_HEIGHT", "height"),
    ("AMEDIAFORMAT_KEY_I_FRAME_INTERVAL", "i-frame-interval"),
    ("AMEDIAFORMAT_KEY_MIME", "mime"),
    ("AMEDIAFORMAT_KEY_SAMPLE_RATE", "sample-rate"),
    ("AMEDIAFORMAT_KEY_STRIDE", "stride"),
    ("AMEDIAFORMAT_KEY_WIDTH", "width"),
];

/// The eighteen, with the sizes above. **`__sF` first**, because `stdin`, `stdout` and `stderr`
/// are pointers into it and the declaration order is what fixes their addresses.
pub static DATA_OBJECTS: &[DataObject] = &[
    DataObject { symbol: "__sF", len: 3 * FILE_BYTES, align: 8 },
    DataObject { symbol: "stdin", len: 8, align: 8 },
    DataObject { symbol: "stdout", len: 8, align: 8 },
    DataObject { symbol: "stderr", len: 8, align: 8 },
    DataObject { symbol: "environ", len: 8, align: 8 },
    DataObject { symbol: "__stack_chk_guard", len: 8, align: 8 },
    DataObject { symbol: "in6addr_any", len: 16, align: 8 },
    DataObject { symbol: "in6addr_loopback", len: 16, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_BIT_RATE", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_CHANNEL_COUNT", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_COLOR_FORMAT", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_FRAME_RATE", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_HEIGHT", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_I_FRAME_INTERVAL", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_MIME", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_SAMPLE_RATE", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_STRIDE", len: 8, align: 8 },
    DataObject { symbol: "AMEDIAFORMAT_KEY_WIDTH", len: 8, align: 8 },
];

/// The facts about the guest process that only the host running it knows.
///
/// One field today. It is a struct rather than an argument so that the phase which adds a real
/// environment, an `auxv` or a program name adds a field instead of changing every caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestProcess {
    /// The stack canary D13 programmed into `TPIDR_EL0 + 0x28` for every thread of this guest.
    ///
    /// Read it from the backend that owns the guest's TLS arena. A zero is refused, not stored.
    pub stack_guard: u64,
}

/// Declare all eighteen and fill them in.
///
/// Returns how many were declared.
pub(super) fn install(
    bionic: &Bionic,
    builder: &BoundaryBuilder,
    process: &GuestProcess,
) -> AbiResult<usize> {
    if process.stack_guard == 0 {
        return Err(AbiError::Refused {
            symbol: "__stack_chk_guard".to_string(),
            address: 0,
            why: "a zero stack canary compares equal to a zeroed stack slot, so a guest stack \
                  overflow that wrote zeroes would pass every `__stack_chk_fail` check — \
                  `omni-cpu` refuses to generate one and this refuses to store one"
                .to_string(),
        });
    }

    let mut addresses: Vec<(&'static str, GuestAddr)> = Vec::with_capacity(DATA_OBJECTS.len());
    for object in DATA_OBJECTS {
        addresses.push((object.symbol, builder.declare_data(object.symbol, object.len, object.align)?));
    }
    let at = |symbol: &str| -> GuestAddr {
        addresses
            .iter()
            .find(|(name, _)| *name == symbol)
            .map(|(_, address)| *address)
            .expect("every symbol in DATA_OBJECTS was just declared")
    };

    // The boundary's data area lives in the same guest space this instance mapped its arena in. A
    // caller that paired a builder with the wrong space gets a typed `BadPointer` out of the first
    // write rather than a silently unfilled object.
    let mem = GuestMem::new(Arc::clone(&bionic.space));
    let blame = |symbol: &'static str, address: GuestAddr| Blame::new(symbol, address, 0);

    // `__sF`: three `FILE`s, zeroed. A zeroed bionic `FILE` has `_flags == 0`, which is what that
    // library's own `__sfp` calls a free slot — so the bytes say "not an open stream", which is
    // true, rather than describing one that could be written to.
    let sf = at("__sF");
    mem.write_bytes(sf, &vec![0u8; 3 * FILE_BYTES], blame("__sF", sf))?;

    // `stdin`, `stdout`, `stderr`: `FILE *` variables, pointing at the three members of `__sF`.
    // That is what bionic does, and it is what makes `stdout == &__sF[1]` hold for a guest
    // translation unit compiled against an old NDK header where `stdout` *was* that macro.
    //
    // **Each one is also registered as a stream over its POSIX descriptor**, which is what makes
    // `fputs(s, stdout)` and `fileno(stderr)` work in phase 3b. The registration is keyed by the
    // guest address, so the `FILE` bytes here stay zeroed and are never interpreted — see
    // `stdio`'s module documentation for why that is what keeps `FILE_BYTES` harmless.
    for (index, symbol) in ["stdin", "stdout", "stderr"].into_iter().enumerate() {
        let cell = at(symbol);
        let stream = sf + index * FILE_BYTES;
        mem.write_u64(cell, stream as u64, blame(symbol, cell))?;
        // `index` is 0, 1, 2 -- which are STDIN_FILENO, STDOUT_FILENO and STDERR_FILENO, and that
        // correspondence is the whole reason the three are declared in this order.
        bionic.register_stream(stream, index as i32);
    }

    // `environ`: `char **environ`. It points at a vector of `char *` terminated by a null, and
    // this guest process has no environment at all — so the vector is one null.
    //
    // **Not a stub, and the difference is worth stating.** An empty environment is a *fact* about
    // a process that was started with none, and `environ = NULL` would be the wrong answer:
    // POSIX-shaped code walks `environ` without checking it first, and a null there is a crash in
    // guest code rather than a refusal here. When a later phase gives the guest a real
    // environment, this is the cell that changes.
    let empty_environ = bionic.reserve("environ", 8)?;
    let environ = at("environ");
    mem.write_u64(environ, empty_environ as u64, blame("environ", environ))?;

    let guard = at("__stack_chk_guard");
    mem.write_u64(guard, process.stack_guard, blame("__stack_chk_guard", guard))?;

    // `in6addr_any` is `::` — sixteen zero bytes. `in6addr_loopback` is `::1`.
    let any = at("in6addr_any");
    mem.write_bytes(any, &[0u8; 16], blame("in6addr_any", any))?;
    let loopback = at("in6addr_loopback");
    let mut ones = [0u8; 16];
    ones[15] = 1;
    mem.write_bytes(loopback, &ones, blame("in6addr_loopback", loopback))?;

    for (symbol, key) in MEDIA_FORMAT_KEYS {
        let string = bionic.intern(symbol, key.as_bytes())?;
        let cell = at(symbol);
        // `symbol` is `&'static str` from the table, so the blame borrow outlives the call.
        mem.write_u64(cell, string as u64, Blame::new(symbol, cell, 0))?;
    }

    Ok(DATA_OBJECTS.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set is eighteen, every symbol appears once, and every length is a multiple of its
    /// alignment so that packing them cannot make one straddle the next.
    #[test]
    fn the_data_table_is_eighteen_distinct_symbols_with_usable_sizes() {
        assert_eq!(DATA_OBJECTS.len(), 18, "D17's reachable STT_OBJECT count");
        let mut seen = std::collections::BTreeSet::new();
        for object in DATA_OBJECTS {
            assert!(seen.insert(object.symbol), "`{}` is declared twice", object.symbol);
            assert!(object.len > 0, "`{}` has no size", object.symbol);
            assert!(object.align.is_power_of_two(), "`{}`", object.symbol);
            assert_eq!(object.len % object.align, 0, "`{}`", object.symbol);
        }
        assert_eq!(seen.len(), 18);
    }

    /// `__sF` is three `FILE`s and the three stream pointers are spaced by one, or `stderr` is
    /// somebody else's object.
    #[test]
    fn the_sf_array_is_three_files_wide() {
        let sf = DATA_OBJECTS.iter().find(|o| o.symbol == "__sF").expect("__sF is declared");
        assert_eq!(sf.len, 3 * FILE_BYTES);
        assert_eq!(FILE_BYTES, 152, "bionic's LP64 `struct __sFILE`, derived field by field");
        assert_eq!(FILE_BYTES % 8, 0, "a FILE ends on a pointer boundary");
    }

    /// The media keys are the ten reachable ones, each with a non-empty distinct string.
    #[test]
    fn the_media_format_keys_are_ten_distinct_non_empty_strings() {
        assert_eq!(MEDIA_FORMAT_KEYS.len(), 10);
        let mut strings = std::collections::BTreeSet::new();
        for (symbol, key) in MEDIA_FORMAT_KEYS {
            assert!(!key.is_empty(), "`{symbol}` would point at an empty string");
            assert!(strings.insert(key), "two keys share the string {key:?}");
            assert!(
                DATA_OBJECTS.iter().any(|o| o.symbol == symbol),
                "`{symbol}` has a string but no data object"
            );
        }
    }
}
