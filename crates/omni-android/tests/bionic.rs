//! **The bionic adapter, driven by real translated ARM64 code.**
//!
//! Every test here writes A64 instructions, lets the translating backend run them, and asserts on
//! the *value* the guest got back. A test that only checked `run` returned `Ok` would pass against
//! a handler that returned zero for everything, which is precisely the failure Global Constraint 1
//! is about.
//!
//! The hostile cases are in the second half of the file, and they are not an afterthought: four of
//! five foundation tasks in this project shipped a hostile-input defect their own passing suites
//! could not see, and every function bound here takes a guest pointer or a guest length.
//!
//! ```text
//! cargo test -p omni-android --test bionic --release
//! ```

#![cfg(target_arch = "x86_64")]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::{AbiError, Boundary};
use omni_cpu::{ExitReason, GuestCpu};

/// A guest, a bionic instance over its address space, and the boundary with every handler bound.
struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    boundary: Arc<Boundary>,
}

fn fixture() -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    let boundary = builder.finish();
    Fixture { guest, bionic, boundary }
}

impl Fixture {
    /// The thunk address a symbol was given.
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary
            .slot_named(symbol)
            .unwrap_or_else(|| panic!("`{symbol}` is not bound"))
            .address
    }

    /// Run `entry` with this instance published to the calling thread.
    fn run(&self, cpu: &mut dyn GuestCpu, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _active = self.bionic.activate().expect("a thread block");
        self.boundary.run(cpu, entry, BUDGET)
    }

    /// Put a NUL-terminated C string at `at` and return `at`.
    fn cstring(&self, at: omni_cpu::GuestAddr, text: &[u8]) -> omni_cpu::GuestAddr {
        let mut bytes = text.to_vec();
        bytes.push(0);
        self.guest.write_bytes(at, &bytes);
        at
    }

    /// Read a NUL-terminated C string back out of guest memory.
    fn read_cstring(&self, at: omni_cpu::GuestAddr) -> Vec<u8> {
        let mut out = Vec::new();
        for offset in 0..4096 {
            let byte = (self.guest.read_u64((at + offset) & !7) >> (8 * ((at + offset) & 7))) as u8;
            if byte == 0 {
                break;
            }
            out.push(byte);
        }
        out
    }
}

/// A program that loads `setup`, calls `symbol`, stores `X0` at `data + 0`, and returns.
fn call_one(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let thunk = f.thunk(symbol);
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    setup(&mut asm);
    asm.bl(thunk);
    asm.mov(22, f.guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());
    entry
}

/// Run a one-call program and return what the guest stored from `X0`.
fn value_of(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> u64 {
    let entry = call_one(f, symbol, setup);
    let mut cpu = f.guest.thread(&f.boundary);
    let exit = f.run(&mut cpu, entry).expect("the run must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    f.guest.read_u64(f.guest.data)
}

/// Run a one-call program that is expected to fail, and return the refusal.
fn refusal_of(f: &Fixture, symbol: &str, setup: impl FnOnce(&mut Asm)) -> AbiError {
    let entry = call_one(f, symbol, setup);
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => error,
        Ok(exit) => panic!("`{symbol}` completed with {exit:?} where a refusal was required"),
    }
}

// =================================================================== the tables

/// Every symbol bound here is one the 3,594 initializers actually reach, no symbol is bound
/// twice, and the two dispatch paths are disjoint.
///
/// The list is the specification (`ARCHITECTURE.md` section 5), so a handler bound under a name
/// that is not in it is either a typo — which would leave the real symbol `Unbound` and the typo
/// unreachable, both silently — or scope creep.
#[test]
fn every_bound_symbol_is_in_the_reachable_set_and_is_bound_once() {
    let reachable = reachable_imports();
    let mut seen = std::collections::BTreeSet::new();
    for symbol in Bionic::bound_symbols() {
        assert!(
            reachable.contains(symbol),
            "`{symbol}` is bound but is not in the first six sections of \
             docs/research/init-reachable-imports.txt"
        );
        assert!(seen.insert(symbol), "`{symbol}` is bound twice");
    }
    assert_eq!(
        seen.len(),
        Bionic::bound_symbols().count(),
        "the two tables must not share a symbol: one address can only have one binding"
    );
}

/// The count, stated exactly, with the two tables separated.
///
/// **A pinned figure, not a target.** It exists so that adding or losing a binding is a visible
/// change rather than a number in a report nobody re-derives — this project has had four wrong
/// counts reach its decision record.
#[test]
fn the_bound_count_is_exactly_what_this_phase_claims() {
    let symbols: Vec<&str> = Bionic::bound_symbols().collect();
    assert_eq!(symbols.len(), 86, "bound symbols: {symbols:?}");
}

/// Parse the first six sections of the reachable-import list: the 188 symbols that are
/// statically reachable from the 3,594 `init_array` roots.
fn reachable_imports() -> std::collections::BTreeSet<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/research/init-reachable-imports.txt");
    let text = std::fs::read_to_string(path).expect("the reachable-import list");
    let mut out = std::collections::BTreeSet::new();
    let mut section = 0usize;
    for line in text.lines() {
        if line.starts_with("###") {
            section += 1;
            continue;
        }
        let symbol = line.trim();
        if symbol.is_empty() || section == 0 || section > 6 {
            continue;
        }
        out.insert(symbol.to_string());
    }
    assert_eq!(out.len(), 188, "the reachable set's first six sections are 188 symbols");
    out
}

// =================================================================== strings and memory

#[test]
fn strlen_reads_a_guest_string_through_a_real_thunk() {
    let _guard = serialized();
    let f = fixture();
    let at = f.cstring(f.guest.data + 0x100, b"the quick brown fox");
    let n = value_of(&f, "strlen", |asm| {
        asm.mov(0, at as u64);
    });
    assert_eq!(n, 19);
}

/// **`strcmp` returns bionic's byte difference, and the adapter must not touch it.**
///
/// The C standard fixes only the sign; bionic fixes the magnitude, and `omni-bionic` follows
/// bionic (pinned there by mutation row `bionic-B1`). A handler that normalised to `-1/0/1`
/// would be changing something guest code can observe. `'a' - 'z'` is `-25`, and it has to arrive
/// in `X0` **sign-extended**, because `Ret::u64` would deliver `0xFFFF_FFE7` as a large positive
/// `int` and turn every "less than" into "greater than".
#[test]
fn strcmp_delivers_the_byte_difference_sign_extended() {
    let _guard = serialized();
    let f = fixture();
    let a = f.cstring(f.guest.data + 0x100, b"a");
    let b = f.cstring(f.guest.data + 0x140, b"z");
    let less = value_of(&f, "strcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, b as u64);
    });
    assert_eq!(less as i64, -25, "'a' - 'z' = -25, not -1 and not 0xFFFFFFE7");
    let more = value_of(&f, "strcmp", |asm| {
        asm.mov(0, b as u64);
        asm.mov(1, a as u64);
    });
    assert_eq!(more as i64, 25);
    let same = value_of(&f, "strcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, a as u64);
    });
    assert_eq!(same, 0);
}

/// `memcmp` carries the same convention, through a different module.
#[test]
fn memcmp_delivers_the_byte_difference_sign_extended() {
    let _guard = serialized();
    let f = fixture();
    let a = f.guest.data + 0x100;
    let b = f.guest.data + 0x140;
    f.guest.write_bytes(a, &[1, 2, 3, 4]);
    f.guest.write_bytes(b, &[1, 2, 200, 4]);
    let diff = value_of(&f, "memcmp", |asm| {
        asm.mov(0, a as u64);
        asm.mov(1, b as u64);
        asm.mov(2, 4);
    });
    assert_eq!(diff as i64, 3 - 200, "bionic's byte difference, not a clamped -1");
}

#[test]
fn memcpy_moves_bytes_and_returns_its_destination() {
    let _guard = serialized();
    let f = fixture();
    let src = f.guest.data + 0x100;
    let dst = f.guest.data + 0x200;
    let payload: Vec<u8> = (0..64u8).map(|i| i.wrapping_mul(7).wrapping_add(3)).collect();
    f.guest.write_bytes(src, &payload);
    let returned = value_of(&f, "memcpy", |asm| {
        asm.mov(0, dst as u64);
        asm.mov(1, src as u64);
        asm.mov(2, 64);
    });
    assert_eq!(returned, dst as u64, "memcpy returns dst");
    let mut copied = vec![0u8; 64];
    for (index, slot) in copied.iter_mut().enumerate() {
        *slot = (f.guest.read_u64((dst + index) & !7) >> (8 * ((dst + index) & 7))) as u8;
    }
    assert_eq!(copied, payload);
}

/// **`long` is 64-bit on the guest and 32-bit on host Windows.** A `strtol` handler that returned
/// 32 bits would truncate, and the truncation is invisible for every small value — which is every
/// value a casual test uses.
#[test]
fn strtol_returns_a_full_64_bit_long() {
    let _guard = serialized();
    let f = fixture();
    let text = f.cstring(f.guest.data + 0x100, b"1234567890123");
    let value = value_of(&f, "strtol", |asm| {
        asm.mov(0, text as u64);
        asm.mov(1, 0); // endptr
        asm.mov(2, 10); // base
    });
    assert_eq!(value, 1_234_567_890_123, "a value that does not fit in 32 bits");
    let negative = f.cstring(f.guest.data + 0x140, b"-9007199254740993");
    let value = value_of(&f, "strtol", |asm| {
        asm.mov(0, negative as u64);
        asm.mov(1, 0);
        asm.mov(2, 10);
    });
    assert_eq!(value as i64, -9_007_199_254_740_993);
}

// =================================================================== errno, really

/// **`errno` is guest-visible storage, and this proves it end to end.**
///
/// The guest calls `strtol` on a number too large for a `long`, which POSIX says sets `ERANGE`;
/// then it calls `__errno()` and dereferences what comes back. Nothing about this works unless
/// the per-thread block is mapped, `__errno` returns *this* thread's slot, and `set_errno` wrote
/// through to it.
///
/// `ERANGE` is asserted as **34**, the Linux number. The host is Windows, where `ERANGE` is 34 as
/// well — so this assertion would pass by luck. `ETIMEDOUT` is the one that would not (110 against
/// Windows' 121/10060), and `omni-bionic`'s `errno.rs` pins every constant.
#[test]
fn errno_is_written_where_the_guest_can_read_it() {
    let _guard = serialized();
    let f = fixture();
    let text = f.cstring(f.guest.data + 0x100, b"999999999999999999999999");
    let strtol = f.thunk("strtol");
    let errno = f.thunk("__errno");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, text as u64);
    asm.mov(1, 0);
    asm.mov(2, 10);
    asm.bl(strtol);
    asm.push(str_imm(0, 22, 0)); // the saturated value
    asm.bl(errno);
    asm.push(str_imm(0, 22, 8)); // the errno cell's address
    asm.push(ldr_w(1, 0, 0)); // *errno
    asm.push(str_imm(1, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    assert_eq!(f.guest.read_u64(f.guest.data) as i64, i64::MAX, "strtol saturates on overflow");
    let cell = f.guest.read_u64(f.guest.data + 8) as usize;
    assert_eq!(cell, f.bionic.arena(), "the first attached thread gets the first block");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), 34, "ERANGE, the Linux value");
}

/// `strerror` returns a pointer into **this thread's** scratch, and the bytes are really there.
///
/// `EINVAL` is used rather than `ETIMEDOUT` because `omni-bionic`'s message table holds ten codes
/// and 110 is not one of them: it answers `Unknown error`, which is bionic's own fallback shape
/// and is pinned here as the second half of the test rather than quietly avoided.
#[test]
fn strerror_returns_a_readable_message_in_per_thread_scratch() {
    let _guard = serialized();
    let f = fixture();
    let pointer = value_of(&f, "strerror", |asm| {
        asm.mov(0, 22); // EINVAL, the Linux value
    }) as usize;
    assert!(pointer >= f.bionic.arena(), "the message must live in the adapter's own arena");
    assert_eq!(f.read_cstring(pointer), b"Invalid argument");

    // A code the table does not carry falls back rather than returning an empty string or a stale
    // one: the previous call's message is still in the buffer and must be overwritten.
    let pointer = value_of(&f, "strerror", |asm| {
        asm.mov(0, 110); // ETIMEDOUT: a Linux value the table does not hold
    }) as usize;
    assert_eq!(f.read_cstring(pointer), b"Unknown error");
}

// =================================================================== pthread

#[test]
fn pthread_self_is_stable_and_is_never_the_reserved_zero() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "pthread_self", |_| {});
    let second = value_of(&f, "pthread_self", |_| {});
    assert_ne!(first, 0, "GuestThreadId::NONE is reserved and no live thread has it");
    assert_eq!(first, second, "one host thread keeps one pthread_t");
}

/// An uncontended `pthread_mutex_init` / `lock` / `unlock` cycle, all three through real thunks,
/// with the guest observing the return code of each.
#[test]
fn a_mutex_round_trip_locks_and_unlocks() {
    let _guard = serialized();
    let f = fixture();
    let mutex = f.guest.data + 0x200;
    f.guest.write_bytes(mutex, &[0u8; 40]);

    let init = f.thunk("pthread_mutex_init");
    let lock = f.thunk("pthread_mutex_lock");
    let unlock = f.thunk("pthread_mutex_unlock");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, mutex as u64);
    asm.mov(1, 0); // default attributes
    asm.bl(init);
    asm.push(str_imm(0, 22, 0));
    asm.mov(0, mutex as u64);
    asm.bl(lock);
    asm.push(str_imm(0, 22, 8));
    asm.mov(0, mutex as u64);
    asm.bl(unlock);
    asm.push(str_imm(0, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_mutex_init");
    assert_eq!(f.guest.read_u64(f.guest.data + 8), 0, "pthread_mutex_lock");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), 0, "pthread_mutex_unlock");
    // The lock was uncontended, so nothing should have blocked. **This is a watch, not a
    // detector**: it rises whenever the sync layer blocks and stays at zero under every defect
    // this adapter could have, so it says "no thread slept" and nothing more.
    let (waits, _) = f.bionic.futex().activity();
    assert_eq!(waits, 0, "an uncontended lock must not enter the futex");
}

/// `pthread_key_create` writes a 32-bit key, and `setspecific`/`getspecific` round-trip a value
/// that is wider than 32 bits — which is the part a `void *` handler can silently truncate.
#[test]
fn a_tls_key_round_trips_a_full_64_bit_value() {
    let _guard = serialized();
    let f = fixture();
    let key_cell = f.guest.data + 0x200;
    let value = 0xDEAD_BEEF_CAFE_0001u64;

    let create = f.thunk("pthread_key_create");
    let set = f.thunk("pthread_setspecific");
    let get = f.thunk("pthread_getspecific");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, key_cell as u64);
    asm.mov(1, 0); // no destructor
    asm.bl(create);
    asm.push(str_imm(0, 22, 0));
    asm.push(ldr_w(0, 22, 0x200)); // the key the handler wrote
    asm.mov(1, value);
    asm.bl(set);
    asm.push(str_imm(0, 22, 8));
    asm.push(ldr_w(0, 22, 0x200));
    asm.bl(get);
    asm.push(str_imm(0, 22, 16));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_key_create");
    assert_eq!(f.guest.read_u64(f.guest.data + 8), 0, "pthread_setspecific");
    assert_eq!(f.guest.read_u64(f.guest.data + 16), value, "the full 64-bit void *");
}

/// **`pthread_once` runs guest code**, which an inline handler structurally cannot do, so it is on
/// the exit path. The initialiser increments a counter in guest memory; calling `pthread_once`
/// three times must leave it at one.
#[test]
fn pthread_once_runs_the_guest_initialiser_exactly_once() {
    let _guard = serialized();
    let f = fixture();
    let once_word = f.guest.data + 0x200;
    let counter = f.guest.data + 0x208;
    f.guest.write_u32(once_word, 0);
    f.guest.write_u64(counter, 0);

    // `void init(void) { ++*counter; }`
    let init_at = f.guest.next_entry();
    let mut init = Asm::at(init_at);
    init.mov(9, counter as u64);
    init.push(ldr_imm(10, 9, 0));
    init.mov(11, 1);
    init.push(add_reg(10, 10, 11));
    init.push(str_imm(10, 9, 0));
    init.push(ret(30));
    f.guest.load(init.words());

    let once = f.thunk("pthread_once");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for _ in 0..3 {
        asm.mov(0, once_word as u64);
        asm.mov(1, init_at as u64);
        asm.bl(once);
    }
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");
    assert_eq!(f.guest.read_u64(counter), 1, "the initialiser ran once, not three times");
    // It really went out through the exit path and really called back into the guest.
    let crossings = f.boundary.crossings();
    assert_eq!(crossings.exits, 3, "three pthread_once calls, all on the exit path");
    assert_eq!(crossings.guest_calls, 1, "one call into the guest initialiser");
}

/// **`qsort` with a guest comparator**, the other direction of the boundary. Four `int`s, sorted
/// ascending by a comparator the guest supplies.
#[test]
fn qsort_sorts_through_a_guest_comparator() {
    let _guard = serialized();
    let f = fixture();
    let base = f.guest.data + 0x200;
    let input: [i32; 4] = [7, -3, 42, 0];
    for (index, value) in input.iter().enumerate() {
        f.guest.write_u32(base + index * 4, *value as u32);
    }

    // `int cmp(const int *a, const int *b) { return *a - *b; }`
    let cmp_at = f.guest.next_entry();
    let mut cmp = Asm::at(cmp_at);
    cmp.push(ldr_w(2, 0, 0));
    cmp.push(ldr_w(3, 1, 0));
    cmp.push(sub_reg(0, 2, 3));
    cmp.push(ret(30));
    f.guest.load(cmp.words());

    let qsort = f.thunk("qsort");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, base as u64);
    asm.mov(1, 4); // nmemb
    asm.mov(2, 4); // size
    asm.mov(3, cmp_at as u64);
    asm.bl(qsort);
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let sorted: Vec<i32> = (0..4)
        .map(|i| (f.guest.read_u64((base + i * 4) & !7) >> (32 * (((base + i * 4) & 7) / 4))) as u32 as i32)
        .collect();
    assert_eq!(sorted, [-3, 0, 7, 42]);
    assert!(f.boundary.crossings().guest_calls >= 3, "the comparator really ran in the guest");
}

// =================================================================== printf

#[test]
fn snprintf_formats_integers_strings_and_doubles_from_a_real_variadic_call() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"[%d] %s %.2f %#x");
    let text = f.cstring(f.guest.data + 0x140, b"omni");
    let double_at = f.guest.data + 0x180;
    f.guest.write_f64(double_at, 1.5);

    let snprintf = f.thunk("snprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, u64::from((-7i32) as u32)); // %d, in X3 as a variadic int
    asm.mov(4, text as u64); // %s
    asm.mov(9, double_at as u64);
    asm.push(ldr_d(0, 9, 0)); // %.2f, in V0 -- AAPCS64 puts variadic FP in V0-V7
    asm.mov(5, 0xABC); // %#x
    asm.bl(snprintf);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let written = f.guest.read_u64(f.guest.data) as i64;
    let produced = String::from_utf8(f.read_cstring(out)).expect("ASCII output");
    assert_eq!(produced, "[-7] omni 1.50 0xabc");
    assert_eq!(written, produced.len() as i64, "snprintf returns the length it wrote");
}

/// `snprintf(NULL, 0, ...)` is the documented way to ask how long a result would be, and it must
/// not touch the destination. The return is the **full** length, not the truncated one: a handler
/// that returned the truncated length makes every caller that grows its buffer loop forever.
#[test]
fn snprintf_truncates_but_reports_the_full_length() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    f.guest.write_bytes(out, &[0xEE; 16]);
    let fmt = f.cstring(f.guest.data + 0x100, b"0123456789");

    let measured = value_of(&f, "snprintf", |asm| {
        asm.mov(0, 0); // NULL destination
        asm.mov(1, 0); // capacity 0
        asm.mov(2, fmt as u64);
    }) as i64;
    assert_eq!(measured, 10);
    // Nothing was written.
    assert_eq!(f.guest.read_u64(out), 0xEEEE_EEEE_EEEE_EEEE);

    let truncated = value_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 5); // room for four characters and a NUL
        asm.mov(2, fmt as u64);
    }) as i64;
    assert_eq!(truncated, 10, "the length that would have been written");
    assert_eq!(f.read_cstring(out), b"0123");
}

/// A null `%s` argument prints `(null)`, which is what bionic does. It is **not** a refusal: the
/// pointer is never dereferenced, so there is nothing to refuse, and a boundary that rejected it
/// would reject a program bionic runs.
#[test]
fn a_null_string_argument_prints_the_bionic_placeholder() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"<%s>");
    let written = value_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 0); // the null const char *
    }) as i64;
    assert_eq!(f.read_cstring(out), b"<(null)>");
    assert_eq!(written, 8);
}

/// `vsnprintf` with a `va_list` the **guest** built, which is the shape AAPCS64 actually uses:
/// the record is 32 bytes, so `X3` holds a pointer to it, not the record.
#[test]
fn vsnprintf_walks_a_va_list_the_guest_built() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"%d/%s/%g");
    let text = f.cstring(f.guest.data + 0x0C0, b"mid");
    let gr_save = f.guest.data + 0x100; // X0-X7 as a variadic prologue spilled them
    let vr_save = f.guest.data + 0x200; // Q0-Q7
    let va_list = f.guest.data + 0x300;
    let overflow = f.guest.data + 0x400;
    let double_at = f.guest.data + 0x040;
    f.guest.write_f64(double_at, 0.125);

    let vsnprintf = f.thunk("vsnprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    // The general save area: the %d, then the %s pointer.
    asm.mov(9, u64::from((-11i32) as u32));
    asm.push(str_imm(9, 22, 0x100));
    asm.mov(9, text as u64);
    asm.push(str_imm(9, 22, 0x108));
    // The SIMD save area: one double in Q0's 16-byte slot.
    asm.push(ldr_d(3, 22, 0x040));
    asm.push(str_d(3, 22, 0x200));
    // The va_list record itself.
    asm.mov(9, overflow as u64);
    asm.push(str_imm(9, 22, 0x300)); // __stack
    asm.mov(9, (gr_save + 64) as u64);
    asm.push(str_imm(9, 22, 0x308)); // __gr_top
    asm.mov(9, (vr_save + 128) as u64);
    asm.push(str_imm(9, 22, 0x310)); // __vr_top
    asm.mov(9, u64::from((-64i32) as u32));
    asm.push(str_w(9, 22, 0x318)); // __gr_offs
    asm.mov(9, u64::from((-128i32) as u32));
    asm.push(str_w(9, 22, 0x31C)); // __vr_offs

    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, va_list as u64);
    asm.bl(vsnprintf);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry).expect("the run must complete");

    let produced = String::from_utf8(f.read_cstring(out)).expect("ASCII output");
    assert_eq!(produced, "-11/mid/0.125", "read from Q0's 16-byte slot, not from an 8-byte step");
    assert_eq!(f.guest.read_u64(f.guest.data) as i64, produced.len() as i64);
}

// =================================================================== refusals

/// **Task 2 review F6, end to end.** `long double` is a 128-bit quad on Android/LP64 with a
/// 16-byte variadic slot, and nothing in this stack can read one. The refusal names the symbol,
/// the guest address and the conversion — and it happens before any argument is read, so no
/// number is produced out of the wrong bank.
#[test]
fn a_long_double_conversion_is_refused_and_names_itself() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"value: %Lf");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
    });
    assert_eq!(error.symbol(), Some("snprintf"));
    assert_eq!(error.guest_address(), Some(f.thunk("snprintf")));
    let text = error.to_string();
    assert!(text.contains("%Lf"), "the refusal must name the conversion: {text}");
    assert!(text.contains("long double"), "{text}");
}

/// Every symbol this phase deliberately does not implement stays `Unbound`, and its call names
/// itself. That is the design, not a gap: a `fopen` bound to a stub returning a plausible `FILE*`
/// would surface three thousand initializers later somewhere unrelated.
#[test]
fn a_symbol_this_phase_does_not_implement_is_unbound_and_says_so() {
    let _guard = serialized();
    let f = fixture();
    let thunk = f.boundary.slot_named("fopen").map(|s| s.address);
    assert!(thunk.is_none(), "fopen must not be bound by this phase");

    // One that *is* declared, because the loader would have asked for it: bind it as the loader
    // would and confirm the call names it.
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind");
    let unbound = builder.declare_function("fopen").expect("a slot");
    let boundary = builder.finish();
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(unbound);
    asm.push(ret(21));
    guest.load(asm.words());
    let mut cpu = guest.thread(&boundary);
    let _active = bionic.activate().expect("a thread block");
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("fopen is not implemented");
    assert!(matches!(error, AbiError::Unbound { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("fopen"));
    assert_eq!(error.guest_address(), Some(unbound));
}

/// The five printf-family symbols that are *bound* but cannot be serviced refuse by name, and the
/// reason says which missing piece. `Unbound` would have said only "not implemented".
#[test]
fn the_unservable_printf_family_refuses_with_the_missing_piece_named() {
    let _guard = serialized();
    let f = fixture();
    for (symbol, needle) in [
        ("fprintf", "file surface"),
        ("vfprintf", "file surface"),
        ("vasprintf", "allocator"),
        ("sscanf", "scanf"),
        ("fscanf", "scanf"),
    ] {
        let error = refusal_of(&f, symbol, |asm| {
            asm.mov(0, 0);
            asm.mov(1, 0);
            asm.mov(2, 0);
        });
        assert_eq!(error.symbol(), Some(symbol));
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)));
        let text = error.to_string();
        assert!(text.contains(needle), "`{symbol}` must say what is missing: {text}");
        assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    }
}

/// A handler on a thread with no instance published refuses by name rather than inventing a
/// default state. A per-call default would give two guest threads their own private copy of the
/// same mutex, which no later test can see.
#[test]
fn a_handler_without_an_activation_refuses_rather_than_defaulting() {
    let _guard = serialized();
    let f = fixture();
    let at = f.cstring(f.guest.data + 0x100, b"abc");
    let entry = call_one(&f, "strlen", |asm| {
        asm.mov(0, at as u64);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    // Deliberately no `activate()`.
    let error = f.boundary.run(&mut cpu, entry, BUDGET).expect_err("no bionic state is installed");
    assert!(matches!(error, AbiError::BionicNotActive { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("strlen"));
    assert_eq!(error.guest_address(), Some(f.thunk("strlen")));
}

// =================================================================== hostile arguments

/// One null-pointer case: the symbol, and the register setup that reaches it.
type NullCase = Box<dyn Fn(&mut Asm)>;

/// A null pointer is the most ordinary thing guest code passes, and every one of these has to be
/// a typed refusal naming the symbol rather than a host access violation.
///
/// **Two independent defences, and the first one wins.** `omni-bionic`'s own `checked_range`
/// rejects a non-empty access at address zero before the pointer ever reaches `omni_mem::admit`,
/// so the refusal is [`AbiError::Refused`] naming the symbol rather than `BadPointer` naming
/// which of `admit`'s rules said no. That is not a weaker answer — it is the crate refusing input
/// it can tell is invalid without asking the address space — and it means a backend that forgot
/// to check would still not turn `memcpy(NULL, x, 16)` into a host store.
#[test]
fn null_pointers_are_refused_by_name_for_every_shape_of_handler() {
    let _guard = serialized();
    let f = fixture();
    let good = f.cstring(f.guest.data + 0x100, b"abc");

    let cases: Vec<(&str, NullCase)> = vec![
        // A string reader.
        (
            "strlen",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
            }),
        ),
        // A two-pointer comparison: the null is the *second* argument.
        (
            "strcmp",
            Box::new(move |asm: &mut Asm| {
                asm.mov(0, good as u64);
                asm.mov(1, 0);
            }),
        ),
        // A writer.
        (
            "memset",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, 0);
                asm.mov(2, 16);
            }),
        ),
        // A copy with a real source and a null destination.
        (
            "memcpy",
            Box::new(move |asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, good as u64);
                asm.mov(2, 16);
            }),
        ),
        // A number parser, which goes through the context rather than plain memory.
        (
            "strtol",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
                asm.mov(1, 0);
                asm.mov(2, 10);
            }),
        ),
        // A synchronization primitive, whose first access is a compare-and-swap.
        (
            "pthread_mutex_lock",
            Box::new(|asm: &mut Asm| {
                asm.mov(0, 0);
            }),
        ),
    ];
    for (symbol, setup) in cases {
        let error = refusal_of(&f, symbol, |asm| setup(asm));
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)), "{error:?}");
        assert!(
            matches!(error, AbiError::Refused { .. } | AbiError::BadPointer { .. }),
            "`{symbol}` with a null pointer must be a typed refusal: {error:?}"
        );
    }

    // A null format string. bionic's own printf crashes here; a refusal is the only other honest
    // answer, and inventing "(null)" would be an invention.
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x300);
        asm.mov(1, 64);
        asm.mov(2, 0);
    });
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    assert!(error.to_string().contains("null format"), "{error}");

    // **`memcpy(NULL, NULL, 0)` is legal C and must NOT be refused.** The over-correction: a
    // boundary that rejected every null pointer would reject a correct program.
    let returned = value_of(&f, "memcpy", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, 0);
    });
    assert_eq!(returned, 0, "a zero-length memcpy at null is defined behaviour and returns dst");
}

/// A pointer into address space nothing has mapped, and a length that runs off the end of a
/// mapping that *is* there. Both are refused before any host byte is touched.
#[test]
fn wild_pointers_and_lying_lengths_are_refused() {
    let _guard = serialized();
    let f = fixture();
    let unmapped = f.guest.unmapped;

    let error = refusal_of(&f, "strlen", |asm| {
        asm.mov(0, unmapped as u64);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");

    // A length that starts inside a mapping and ends outside it.
    let error = refusal_of(&f, "memcpy", |asm| {
        asm.mov(0, f.guest.data as u64);
        asm.mov(1, f.guest.data as u64 + 0x100);
        asm.mov(2, 1 << 40);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");

    // A length that would wrap the 64-bit address space.
    let error = refusal_of(&f, "memcmp", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x100);
        asm.mov(1, f.guest.data as u64 + 0x200);
        asm.mov(2, u64::MAX);
    });
    assert!(
        matches!(error, AbiError::BadPointer { .. } | AbiError::Refused { .. }),
        "{error:?}"
    );
}

/// A guest's own read-only pages handed over as an output buffer. This is refused on the
/// *protection* rule, which is the distinction that matters: the address is mapped, and a handler
/// that only checked "is it mapped" would take a host access violation on the store.
#[test]
fn a_read_only_destination_is_refused_for_writing() {
    let _guard = serialized();
    let f = fixture();
    let src = f.cstring(f.guest.data + 0x100, b"source");
    let error = refusal_of(&f, "strcpy", |asm| {
        asm.mov(0, f.guest.readonly as u64);
        asm.mov(1, src as u64);
    });
    match error {
        AbiError::BadPointer { access, .. } => assert_eq!(access, "writing"),
        other => panic!("{other:?}"),
    }
}

/// A string with no NUL in it, in a mapping with nothing after it.
///
/// **What bounds this walk is the mapping, not a cap**, and the test says so because the
/// difference matters. `omni-bionic`'s `strlen` reads one byte at a time until it finds a NUL or
/// faults, exactly as the real one does, so it will happily walk from one mapping into an
/// adjacent one — the first attempt at this test did, into the read-only page the harness maps
/// next door, and returned 4096. The refusal arrives only where the address space runs out.
///
/// The consequence, stated rather than fixed: a guest that passes an unterminated pointer into a
/// large mapped region makes one handler scan that whole region, one `admit` per byte. It
/// terminates and it cannot abort, but it is unbounded work chosen by the guest. The FORTIFY form
/// below is the bounded one, and `GuestMem::cstr` — which the `printf` family uses — has the
/// boundary's own 64 KiB `STRING_LIMIT`.
#[test]
fn an_unterminated_string_faults_at_the_end_of_its_mapping() {
    use omni_mem::{CommitPolicy, Placement, Protection};
    let _guard = serialized();
    let f = fixture();
    // A mapping placed in free address space, with the space after it left free, so that the
    // walk has somewhere to run out rather than somewhere to continue.
    let page = f.guest.space.page_size();
    let island = f
        .guest
        .space
        .map_anonymous(
            Placement::Fixed(f.guest.unmapped & !(page - 1)),
            page,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("an island mapping");
    assert!(
        f.guest.space.region_at(island + page).is_none_or(|r| r.is_free()),
        "the test needs free space after the island, or the walk would continue into it"
    );
    f.guest.write_bytes(island, &vec![0x41u8; page]);

    let error = refusal_of(&f, "strlen", |asm| {
        asm.mov(0, island as u64);
    });
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("strlen"));

    // The FORTIFY form is the one with a bound, and it reports the overflow it detected rather
    // than a pointer problem: the string is longer than the object it is supposed to be in.
    let error = refusal_of(&f, "__strlen_chk", |asm| {
        asm.mov(0, island as u64);
        asm.mov(1, 16);
    });
    let text = error.to_string();
    assert!(text.contains("__strlen_chk"), "{text}");
}

/// **A compare-and-swap on an unaligned guest address is a refusal, not a slow path.** A 32-bit
/// atomic on an unaligned address is undefined behaviour in Rust, and on the guest's own hardware
/// `LDXR`/`STXR` take an alignment fault there too — so refusing is what the guest would see on a
/// real device, and it is the only answer here that is not undefined behaviour.
#[test]
fn an_unaligned_mutex_word_is_refused_rather_than_atomically_accessed() {
    let _guard = serialized();
    let f = fixture();
    let misaligned = f.guest.data + 0x201; // deliberately odd
    f.guest.write_bytes(f.guest.data + 0x200, &[0u8; 48]);
    let error = refusal_of(&f, "pthread_mutex_lock", |asm| {
        asm.mov(0, misaligned as u64);
    });
    let text = error.to_string();
    assert!(text.contains("align"), "the refusal must say what is wrong: {text}");
    assert_eq!(error.symbol(), Some("pthread_mutex_lock"));

    // **The over-correction check, and the address is chosen for it.** `0x204` is 4-byte aligned
    // and *not* 8-byte aligned, which is legal for a `pthread_mutex_t` — bionic's holds an `int`.
    // A check tightened to 8 bytes would look correct and would refuse a mutex a real program
    // has, and `data + 0x200` would not have caught it.
    let aligned = f.guest.data + 0x204;
    f.guest.write_bytes(aligned, &[0u8; 40]);
    let code = value_of(&f, "pthread_mutex_lock", |asm| {
        asm.mov(0, aligned as u64);
    });
    assert_eq!(code, 0, "a 4-byte-aligned mutex must still lock");
}

/// A `va_list` whose offsets are outside what an AArch64 register save area can have. Both
/// fields are guest-written, so both are range-checked, and the refusal names which one.
#[test]
fn a_hostile_va_list_is_refused_with_the_field_named() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"%d");
    let va_list = f.guest.data + 0x300;

    let vsnprintf = f.thunk("vsnprintf");
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(22, f.guest.data as u64);
    asm.mov(9, f.guest.data as u64 + 0x400);
    asm.push(str_imm(9, 22, 0x300)); // __stack
    asm.mov(9, f.guest.data as u64 + 0x140);
    asm.push(str_imm(9, 22, 0x308)); // __gr_top
    asm.push(str_imm(9, 22, 0x310)); // __vr_top
    asm.mov(9, u64::from((-2_000_000i32) as u32));
    asm.push(str_w(9, 22, 0x318)); // __gr_offs: far outside -64..=0
    asm.mov(9, 0);
    asm.push(str_w(9, 22, 0x31C));
    asm.mov(0, out as u64);
    asm.mov(1, 64);
    asm.mov(2, fmt as u64);
    asm.mov(3, va_list as u64);
    asm.bl(vsnprintf);
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    let error = f.boundary_run_err(&mut cpu, entry);
    match error {
        AbiError::BadVaList { field, .. } => assert_eq!(field, "__gr_offs"),
        other => panic!("{other:?}"),
    }
}

impl Fixture {
    fn boundary_run_err(&self, cpu: &mut dyn GuestCpu, entry: omni_cpu::GuestAddr) -> AbiError {
        let _active = self.bionic.activate().expect("a thread block");
        self.boundary.run(cpu, entry, BUDGET).expect_err("this program must be refused")
    }
}

/// **A guest-chosen field width is an allocation the guest picked**, and the one that aborts is
/// the one that is not caught. Refused, with nothing written to the destination.
#[test]
fn a_hostile_printf_width_is_refused_and_writes_nothing() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    f.guest.write_bytes(out, &[0xEE; 16]);
    let fmt = f.cstring(f.guest.data + 0x100, b"%999999999999999999999d");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 1);
    });
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    assert!(error.to_string().contains("wide"), "{error}");
    assert_eq!(f.guest.read_u64(out), 0xEEEE_EEEE_EEEE_EEEE, "nothing was written");
}

/// `%n` writes through a pointer the format string names, and is the classic format-string
/// exploit primitive. bionic does not support it; neither does this, and it is refused by name.
#[test]
fn percent_n_is_refused() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let fmt = f.cstring(f.guest.data + 0x100, b"abc%n");
    let error = refusal_of(&f, "snprintf", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64);
        asm.mov(2, fmt as u64);
        asm.mov(3, f.guest.data as u64 + 0x200);
    });
    assert!(error.to_string().contains("%n"), "{error}");
}

/// The FORTIFY refusal: `__vsnprintf_chk` is told both what the caller passed and what the
/// compiler could prove, and a caller passing more than the destination holds is a **detected
/// buffer overflow in guest code**. bionic answers it with `__fortify_fatal`.
#[test]
fn a_fortify_check_that_fires_is_reported_as_the_overflow_it_is() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x600;
    let fmt = f.cstring(f.guest.data + 0x080, b"x");
    let va_list = f.guest.data + 0x300;

    let error = refusal_of(&f, "__vsnprintf_chk", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, 64); // supplied size
        asm.mov(2, 0); // flags
        asm.mov(3, 8); // what the compiler proved: eight bytes
        asm.mov(4, fmt as u64);
        asm.mov(5, va_list as u64);
    });
    let text = error.to_string();
    assert!(text.contains("FORTIFY"), "{text}");
    assert!(text.contains("64") && text.contains('8'), "both sizes must be named: {text}");
}

// =================================================================== the arena's bound

/// The 65th guest thread is a refusal, not a second thread sharing the 1st one's `errno` slot.
///
/// Sharing would be silent: two threads would see each other's `errno` and each other's
/// `strerror` buffer, and the only symptom would be an occasional wrong error number.
#[test]
fn the_thread_arena_refuses_rather_than_sharing_a_block() {
    use omni_android::bionic::MAX_GUEST_THREADS;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");

    // Two barriers, because the order matters and a race here would make the test flaky in the
    // direction that hides the bug: `ready` is passed only once all 64 have a block, and `done`
    // keeps every one of them holding it until this thread has made its attempt. A single
    // barrier would let this thread take a slot first and a spawned thread take the refusal.
    let ready = Arc::new(std::sync::Barrier::new(MAX_GUEST_THREADS + 1));
    let done = Arc::new(std::sync::Barrier::new(MAX_GUEST_THREADS + 1));
    let mut handles = Vec::new();
    for _ in 0..MAX_GUEST_THREADS {
        let bionic = Arc::clone(&bionic);
        let ready = Arc::clone(&ready);
        let done = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            let active = bionic.activate().expect("a block for each of the first 64");
            ready.wait();
            done.wait();
            drop(active);
        }));
    }
    ready.wait();
    // The 65th, from this thread, with all 64 blocks held.
    let overflowed = bionic.activate();
    done.wait();
    for handle in handles {
        handle.join().expect("each thread finishes");
    }

    match overflowed {
        Err(AbiError::Refused { why, .. }) => {
            assert!(why.contains("errno"), "the refusal must say what would be shared: {why}");
        }
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("the 65th thread was given a block, so two threads share an errno slot"),
    }
    assert_eq!(bionic.attached(), MAX_GUEST_THREADS);
}

/// Every attached thread gets its **own** block, and the blocks do not overlap. One `errno` slot
/// shared between two threads is the failure this arena exists to prevent.
#[test]
fn each_thread_gets_a_distinct_block() {
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let bionic = Arc::clone(&bionic);
        let seen = Arc::clone(&seen);
        handles.push(std::thread::spawn(move || {
            let _active = bionic.activate().expect("a block");
            let id = bionic.current_thread().expect("an identity");
            seen.lock().unwrap().push(id);
        }));
    }
    for handle in handles {
        handle.join().expect("each thread finishes");
    }
    let ids = seen.lock().unwrap().clone();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 8, "eight threads, eight distinct pthread_t values: {ids:?}");
    assert!(ids.iter().all(|id| id.0 != 0), "no thread may get the reserved zero");
}

/// The clock is wired even though nothing this phase binds reads it: `pthread_cond_timedwait` and
/// the timed lock forms are its callers and they are Tier C. Exercised here so the next phase
/// finds a capability that works rather than one that was never run.
#[test]
fn the_clock_is_monotonic_and_has_a_wall_clock_beside_it() {
    use omni_bionic::threads::Clock;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let first = bionic.clock().now_monotonic();
    let second = bionic.clock().now_monotonic();
    assert!(second >= first, "CLOCK_MONOTONIC must never go backwards");
    // 2020-01-01 in seconds since the epoch. A wall clock reading below it is a host whose clock
    // is not set, which is worth knowing about rather than asserting a range around "now".
    assert!(bionic.clock().now_realtime().as_secs() > 1_577_836_800);
}

/// `rand` is `omni-bionic`'s LCG and is **not** bit-exact with bionic's. Pinned here so the
/// sequence is a stated fact rather than an accident, and so that anything which later starts
/// depending on bionic's exact stream fails visibly instead of drifting.
#[test]
fn rand_produces_the_documented_lcg_sequence_which_is_not_bionics() {
    let _guard = serialized();
    let f = fixture();
    let first = value_of(&f, "rand", |_| {}) as i64;
    let second = value_of(&f, "rand", |_| {}) as i64;
    assert_eq!(
        (first, second),
        (1_103_527_590, 377_401_575),
        "the LCG from state 1; NOT bionic's sequence, and nothing in the engine may check it \
         against bionic's"
    );
    assert!(first >= 0 && second >= 0, "rand returns [0, RAND_MAX], never a negative int");
}
