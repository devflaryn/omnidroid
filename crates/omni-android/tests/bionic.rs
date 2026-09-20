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
    assert_eq!(symbols.len(), 96, "bound symbols: {symbols:?}");
    // Phase 1 bound 86 — 84 inline and two re-entrant. Phase 2 adds ten: the four `dl*` refusals
    // inline, and `dl_iterate_phdr` plus the five guest-memory calls on the exit path.
    assert_eq!(Bionic::inline_symbols().count(), 88);
    assert_eq!(Bionic::reentrant_symbols().count(), 8);
    // Plus the eighteen `STT_OBJECT` data objects, which are not functions and are not bound to a
    // handler at all. 96 + 18 = 114 of the 188 the initializers reach.
    assert_eq!(omni_android::bionic::DATA_OBJECTS.len(), 18);
}

/// **Task 2 review finding F9, asserted rather than trusted to a comment.**
///
/// `ImportCall::mem()` reaches the whole `GuestSpace`, and an inline handler runs inside one of the
/// translating backend's own callbacks with generated code live — where the pager's "the thread
/// running guest code must not hold this space's lock" invariant is reachable and where unmapping
/// or reprotecting a range invalidates memory live translations reference. Nothing in the types
/// prevents `mmap` being moved into the inline table; this is what notices.
///
/// The converse matters too: a symbol on the exit path costs three times as much per call (D17),
/// so the list is pinned in both directions.
#[test]
fn dispatch_paths_are_what_f9_requires() {
    let reentrant: std::collections::BTreeSet<&str> = Bionic::reentrant_symbols().collect();
    for symbol in ["mmap", "munmap", "mprotect", "madvise", "mlock"] {
        assert!(
            reentrant.contains(symbol),
            "`{symbol}` reaches GuestSpace and must be serviced on the exit path (F9)"
        );
    }
    // These three call guest code, which an inline handler structurally cannot (D18).
    for symbol in ["pthread_once", "qsort", "dl_iterate_phdr"] {
        assert!(reentrant.contains(symbol), "`{symbol}` calls guest code");
    }
    assert_eq!(reentrant.len(), 8, "nothing else belongs on the slow path: {reentrant:?}");
    let inline: std::collections::BTreeSet<&str> = Bionic::inline_symbols().collect();
    // The four `dl*` refusals touch no address space and run no guest code, so they stay on the
    // fast path even though their sibling does not.
    for symbol in ["dlopen", "dlsym", "dlclose", "dlerror"] {
        assert!(inline.contains(symbol), "`{symbol}` has no reason to exit the run loop");
    }
    assert!(inline.is_disjoint(&reentrant));
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

// ================================================= phase 2: data symbols, dl*, guest memory

use omni_android::bionic::{GuestProcess, DATA_OBJECTS, DL_PHDR_INFO_BYTES, FILE_BYTES};
use omni_elf::loader::DlPhdrInfo;

/// One synthetic loaded image, as a host would describe a real one.
struct Image {
    name: &'static str,
    addr: omni_cpu::GuestAddr,
    phdr: omni_cpu::GuestAddr,
    phnum: u16,
}

/// A fixture with the handlers bound **and** the eighteen data objects placed, plus whatever
/// images the test wants `dl_iterate_phdr` to enumerate.
///
/// The stack canary comes from the backend's own TLS arena rather than from a number this file
/// chose, which is the whole point of `__stack_chk_guard`: a function that loads the global must
/// see what `[TPIDR_EL0, #0x28]` holds (D13).
fn fixture_with(images: &[Image]) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind every handler");
    let stack_guard = guest.backend.tls().stack_guard();
    bionic
        .declare_data_into(&builder, &GuestProcess { stack_guard })
        .expect("declare and fill the eighteen data objects");
    for image in images {
        bionic
            .register_image(&DlPhdrInfo {
                name: image.name.to_string(),
                addr: image.addr,
                phdr: image.phdr,
                phnum: image.phnum,
            })
            .expect("register an image");
    }
    let boundary = builder.finish();
    Fixture { guest, bionic, boundary }
}

/// Assemble a program with `X21` holding the caller's return address, and run it.
fn program(f: &Fixture, build: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    build(&mut asm);
    asm.push(ret(21));
    f.guest.load(asm.words());
    entry
}

fn run_program(f: &Fixture, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
    let mut cpu = f.guest.thread(&f.boundary);
    f.run(&mut cpu, entry)
}

// ------------------------------------------------------------------ the data symbols

/// **The eighteen are placed, sized and filled**, read back through the boundary's own memory.
///
/// Asserted on contents rather than on addresses, because an address proves only that
/// `declare_data` was called and every one of these has a value guest code will act on.
#[test]
fn the_data_objects_hold_the_values_the_guest_will_read() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let guard = f.guest.backend.tls().stack_guard();

    // `__stack_chk_guard` must be the canary D13 programmed, or a stack-protected function that
    // loads the global form and one that loads `[TPIDR_EL0, #0x28]` disagree — and the second
    // kind is 1,276 of `libroblox.so`'s 1,282 thread-pointer reads.
    assert_ne!(guard, 0);
    assert_eq!(f.guest.read_u64(f.thunk("__stack_chk_guard")), guard);

    // `stdin`/`stdout`/`stderr` are `FILE *` into `__sF`, one `FILE` apart.
    let sf = f.thunk("__sF");
    for (index, symbol) in ["stdin", "stdout", "stderr"].into_iter().enumerate() {
        assert_eq!(
            f.guest.read_u64(f.thunk(symbol)) as usize,
            sf + index * FILE_BYTES,
            "`{symbol}` must point at __sF[{index}]"
        );
    }
    // And the three do not overlap: the object is three `FILE`s wide.
    let sf_object = DATA_OBJECTS.iter().find(|o| o.symbol == "__sF").expect("__sF");
    assert_eq!(sf_object.len, 3 * FILE_BYTES);

    // `environ` points at a vector whose first entry is the terminating null: an empty
    // environment, which is a fact about this process rather than a placeholder. A null
    // `environ` would be the wrong answer — POSIX-shaped code walks it without checking.
    let vector = f.guest.read_u64(f.thunk("environ")) as usize;
    assert_ne!(vector, 0, "environ itself must not be null");
    assert_eq!(f.guest.read_u64(vector), 0, "the vector is one terminating null");

    // `in6addr_any` is `::` and `in6addr_loopback` is `::1`.
    let any = f.thunk("in6addr_any");
    assert_eq!(f.guest.read_u64(any), 0);
    assert_eq!(f.guest.read_u64(any + 8), 0);
    let loopback = f.thunk("in6addr_loopback");
    assert_eq!(f.guest.read_u64(loopback), 0);
    assert_eq!(
        f.guest.read_u64(loopback + 8).to_be(),
        1,
        "::1 is fifteen zero bytes and then a one, in network order"
    );
    assert_ne!(f.guest.read_u64(loopback + 8), f.guest.read_u64(any + 8));

    // Every `AMEDIAFORMAT_KEY_*` points at a distinct non-empty string.
    let mut keys = std::collections::BTreeSet::new();
    for object in DATA_OBJECTS.iter().filter(|o| o.symbol.starts_with("AMEDIAFORMAT_KEY_")) {
        let string = f.guest.read_u64(f.thunk(object.symbol)) as usize;
        assert_ne!(string, 0, "`{}` must not be null: the engine strcmps it", object.symbol);
        let text = f.read_cstring(string);
        assert!(!text.is_empty(), "`{}` points at an empty string", object.symbol);
        assert!(keys.insert(text.clone()), "two keys share {:?}", String::from_utf8_lossy(&text));
    }
    assert_eq!(keys.len(), 10);
    assert_eq!(f.guest.read_u64(f.thunk("AMEDIAFORMAT_KEY_MIME")) as usize, {
        let at = f.guest.read_u64(f.thunk("AMEDIAFORMAT_KEY_MIME")) as usize;
        assert_eq!(f.read_cstring(at), b"mime");
        at
    });
}

/// A zero canary compares equal to a zeroed stack slot, so a guest stack overflow that wrote
/// zeroes would pass every `__stack_chk_fail` check. `omni-cpu` refuses to *generate* one; this
/// refuses to *store* one, and the two refusals have to agree or the global and the TLS copy
/// diverge in the one case that matters.
#[test]
fn a_zero_stack_canary_is_refused_rather_than_stored() {
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    let error = bionic
        .declare_data_into(&builder, &GuestProcess { stack_guard: 0 })
        .expect_err("a zero canary must be refused");
    assert_eq!(error.symbol(), Some("__stack_chk_guard"));
    assert!(error.to_string().contains("zero"), "{error}");
}

/// **A data symbol that is *called* is still `DataSymbolCalled`.** The eighteen are addresses to
/// load from; executing whatever `__sF` holds is the one response that must not happen, and now
/// that the objects have contents there is something there to execute.
#[test]
fn calling_a_filled_data_symbol_is_still_refused_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    for symbol in ["__sF", "environ", "AMEDIAFORMAT_KEY_MIME"] {
        let target = f.thunk(symbol);
        // `BLR` rather than `BL`: the data area is nowhere near the code region and a `BL`
        // displacement is +/-128 MB.
        let entry = program(&f, |asm| {
            asm.mov(9, target as u64);
            asm.push(blr(9));
        });
        match run_program(&f, entry) {
            Err(AbiError::DataSymbolCalled { symbol: named, address }) => {
                assert_eq!(named, symbol);
                assert_eq!(address, target);
            }
            other => panic!("`{symbol}`: {other:?}"),
        }
    }
}

// ------------------------------------------------------------------ dl_iterate_phdr

/// Bytes of one record the guest callback writes out.
const RECORD_BYTES: usize = 48;

/// A guest `int (*)(struct dl_phdr_info *, size_t, void *)` that copies six fields of every
/// object it is handed into a cursor the third argument points at, and returns `answer`.
///
/// **This is what makes the test about the struct layout rather than about the handler.** The
/// callback reads `dlpi_addr` at `+0`, `dlpi_name` at `+8`, `dlpi_phdr` at `+16`, `dlpi_phnum` at
/// `+24` and `dlpi_adds` at `+32` with real `LDR` instructions, exactly as a guest unwinder does.
/// A handler that wrote the fields in the wrong order would put the name where the bias belongs
/// and this would see it.
fn dl_callback(guest: &Guest, answer: i64) -> omni_cpu::GuestAddr {
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(ldr_imm(3, 2, 0)); // X3 = cursor
    asm.push(str_imm(1, 3, 0)); // the `size` argument
    asm.push(ldr_imm(4, 0, 0));
    asm.push(str_imm(4, 3, 8)); // dlpi_addr
    asm.push(ldr_imm(4, 0, 8));
    asm.push(str_imm(4, 3, 16)); // dlpi_name
    asm.push(ldr_imm(4, 0, 16));
    asm.push(str_imm(4, 3, 24)); // dlpi_phdr
    asm.push(ldr_w(4, 0, 24));
    asm.push(str_imm(4, 3, 32)); // dlpi_phnum, zero-extended
    asm.push(ldr_imm(4, 0, 32));
    asm.push(str_imm(4, 3, 40)); // dlpi_adds
    asm.push(add_imm(3, 3, RECORD_BYTES as u32));
    asm.push(str_imm(3, 2, 0));
    asm.mov(0, answer as u64);
    asm.push(ret(30));
    guest.load(asm.words())
}

/// `dl_iterate_phdr` enumerates every registered image, in registration order, with the fields
/// where AArch64 bionic puts them.
#[test]
fn dl_iterate_phdr_enumerates_the_real_loaded_image() {
    let _guard = serialized();
    let images = [
        Image { name: "libroblox.so", addr: 0x1234_0000, phdr: 0x1234_0040, phnum: 9 },
        Image { name: "libzstd-jni.so", addr: 0x5000_0000, phdr: 0x5000_0040, phnum: 7 },
    ];
    let f = fixture_with(&images);

    let cursor = f.guest.data + 0x400;
    let records = f.guest.data + 0x800;
    f.guest.write_u64(cursor, records as u64);
    let callback = dl_callback(&f.guest, 0);

    let returned = value_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(returned, 0, "every callback returned zero, so the walk completes and returns zero");
    assert_eq!(
        f.guest.read_u64(cursor) as usize,
        records + images.len() * RECORD_BYTES,
        "the callback must have run once per registered image"
    );

    for (index, image) in images.iter().enumerate() {
        let at = records + index * RECORD_BYTES;
        assert_eq!(
            f.guest.read_u64(at) as usize,
            DL_PHDR_INFO_BYTES,
            "the `size` argument is sizeof(struct dl_phdr_info)"
        );
        assert_eq!(f.guest.read_u64(at + 8) as usize, image.addr, "dlpi_addr is the load bias");
        let name = f.guest.read_u64(at + 16) as usize;
        assert_ne!(name, 0, "dlpi_name is a pointer the callback dereferences");
        assert_eq!(f.read_cstring(name), image.name.as_bytes());
        assert_eq!(f.guest.read_u64(at + 24) as usize, image.phdr, "dlpi_phdr");
        assert_eq!(f.guest.read_u64(at + 32), u64::from(image.phnum), "dlpi_phnum");
        assert_eq!(
            f.guest.read_u64(at + 40),
            images.len() as u64,
            "dlpi_adds is how many objects have ever been added; nothing here can dlopen"
        );
    }
    // The walk really left the run loop once per object plus once for the call itself.
    assert!(f.boundary.crossings().guest_calls >= images.len() as u64);
}

/// A callback that answers non-zero stops the walk and its value is returned — which is the
/// contract the unwinder relies on: it answers non-zero the moment it finds the object holding
/// the address it is looking for.
#[test]
fn a_callback_that_answers_non_zero_stops_the_walk() {
    let _guard = serialized();
    let images = [
        Image { name: "first.so", addr: 0x1000_0000, phdr: 0x1000_0040, phnum: 4 },
        Image { name: "second.so", addr: 0x2000_0000, phdr: 0x2000_0040, phnum: 4 },
        Image { name: "third.so", addr: 0x3000_0000, phdr: 0x3000_0040, phnum: 4 },
    ];
    let f = fixture_with(&images);
    let cursor = f.guest.data + 0x400;
    let records = f.guest.data + 0x800;
    f.guest.write_u64(cursor, records as u64);
    let callback = dl_callback(&f.guest, 7);

    let returned = value_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(returned as i64 as i32, 7, "the callback's own answer is returned");
    assert_eq!(
        f.guest.read_u64(cursor) as usize,
        records + RECORD_BYTES,
        "exactly one object was reported before the walk stopped"
    );
    assert_eq!(f.guest.read_u64(records + 8) as usize, images[0].addr, "and it was the first");
}

/// **The refusal that keeps `dl_iterate_phdr` from becoming a stub.**
///
/// Reporting a process with no objects is a *success*: the call returns zero, which is what it
/// returns when every callback declined. The guest's statically-linked unwinder would then find
/// no `.eh_frame` and every `throw` would fail to find a landing pad, thousands of initializers
/// from here. So an adapter with nothing registered refuses and says what to call.
#[test]
fn dl_iterate_phdr_with_no_registered_image_refuses_rather_than_reporting_an_empty_process() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let callback = dl_callback(&f.guest, 0);
    let cursor = f.guest.data + 0x400;
    let error = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, cursor as u64);
    });
    assert_eq!(error.symbol(), Some("dl_iterate_phdr"));
    let text = error.to_string();
    assert!(text.contains("register_image"), "the refusal must say what to call: {text}");
    assert!(text.contains("eh_frame"), "and why it matters: {text}");
}

/// Hostile: a null callback, and a callback pointing at memory nothing has mapped. Neither may
/// panic, and the two are told apart — one is a refusal, the other is a stopped guest callback.
#[test]
fn a_hostile_dl_iterate_phdr_callback_is_a_typed_error_and_not_a_panic() {
    let _guard = serialized();
    let images = [Image { name: "only.so", addr: 0x1000_0000, phdr: 0x1000_0040, phnum: 4 }];
    let f = fixture_with(&images);
    let cursor = f.guest.data + 0x400;
    f.guest.write_u64(cursor, (f.guest.data + 0x800) as u64);

    let null = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, 0);
        asm.mov(1, cursor as u64);
    });
    assert!(matches!(null, AbiError::Refused { .. }), "{null:?}");
    assert!(null.to_string().contains("0x0"), "{null}");

    let wild = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, f.guest.unmapped as u64);
        asm.mov(1, cursor as u64);
    });
    assert!(
        matches!(wild, AbiError::GuestCallbackStopped { .. }),
        "an unmapped callback is the guest's own fault and is reported as one: {wild:?}"
    );

    // And a `data` pointer the callback will fault on is the callback's failure too, not ours.
    // Assembled first: `call_one` fixes its own entry address before the setup closure runs, so a
    // closure that loaded another program would move the code out from under its own branches.
    let callback = dl_callback(&f.guest, 0);
    let bad_data = refusal_of(&f, "dl_iterate_phdr", |asm| {
        asm.mov(0, callback as u64);
        asm.mov(1, f.guest.unmapped as u64);
    });
    assert!(matches!(bad_data, AbiError::GuestCallbackStopped { .. }), "{bad_data:?}");
}

// ------------------------------------------------------------------ dlopen and friends

/// `dlopen`, `dlsym` and `dlclose` refuse **by name, quoting the guest's own argument**, and
/// `dlerror` answers null.
///
/// A plausible handle is the worst outcome available here: the guest would `dlsym` it, store what
/// came back, and call it thousands of initializers later. A refusal naming the library is a lead.
#[test]
fn the_dl_family_refuses_by_name_rather_than_issuing_a_handle_it_cannot_honour() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let path = f.cstring(f.guest.data + 0x100, b"libvulkan.so");
    let wanted = f.cstring(f.guest.data + 0x180, b"vkGetInstanceProcAddr");

    let open = refusal_of(&f, "dlopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, 2); // RTLD_NOW
    });
    assert_eq!(open.symbol(), Some("dlopen"));
    assert!(open.to_string().contains("libvulkan.so"), "{open}");

    let sym = refusal_of(&f, "dlsym", |asm| {
        asm.mov(0, 0x1234);
        asm.mov(1, wanted as u64);
    });
    assert_eq!(sym.symbol(), Some("dlsym"));
    assert!(sym.to_string().contains("vkGetInstanceProcAddr"), "{sym}");
    assert!(sym.to_string().contains("0x1234"), "{sym}");

    let close = refusal_of(&f, "dlclose", |asm| {
        asm.mov(0, 0x1234);
    });
    assert_eq!(close.symbol(), Some("dlclose"));

    // `dlerror` is the one that answers, and null is the true answer: nothing above it can leave
    // an error behind, because none of the three returns at all.
    assert_eq!(value_of(&f, "dlerror", |_| {}), 0);
}

/// Hostile: `dlopen(NULL)`, an unterminated name, and a wild pointer. All three are refused and
/// none of them is a panic — the refusal *describes* the bad pointer rather than replacing the
/// useful message with a bad-pointer error.
#[test]
fn a_hostile_dlopen_argument_is_described_rather_than_crashing() {
    let _guard = serialized();
    let f = fixture_with(&[]);

    let null = refusal_of(&f, "dlopen", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 2);
    });
    assert!(null.to_string().contains("NULL"), "{null}");

    let wild = refusal_of(&f, "dlopen", |asm| {
        asm.mov(0, f.guest.unmapped as u64);
        asm.mov(1, 2);
    });
    assert_eq!(wild.symbol(), Some("dlopen"), "still dlopen's refusal, not a bare bad pointer");
    assert!(wild.to_string().contains("unreadable"), "{wild}");

    // A string with no NUL anywhere in its region.
    let island = f.guest.readonly;
    let unterminated = refusal_of(&f, "dlsym", |asm| {
        asm.mov(0, 0);
        asm.mov(1, island as u64);
    });
    assert_eq!(unterminated.symbol(), Some("dlsym"));
}

// ------------------------------------------------------------------ the guest-memory group

/// `PROT_READ | PROT_WRITE`.
const PROT_RW: u64 = 3;
/// `MAP_PRIVATE | MAP_ANONYMOUS`.
const MAP_ANON_PRIVATE: u64 = 0x22;

/// Call `mmap(addr, length, prot, flags, fd, offset)` from real guest code.
fn guest_mmap(
    f: &Fixture,
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
    offset: u64,
) -> u64 {
    value_of(f, "mmap", |asm| {
        asm.mov(0, addr);
        asm.mov(1, length);
        asm.mov(2, prot);
        asm.mov(3, flags);
        asm.mov(4, fd as u64);
        asm.mov(5, offset);
    })
}

/// The refusal form of [`guest_mmap`].
fn guest_mmap_refusal(
    f: &Fixture,
    addr: u64,
    length: u64,
    prot: u64,
    flags: u64,
    fd: i64,
) -> AbiError {
    refusal_of(f, "mmap", |asm| {
        asm.mov(0, addr);
        asm.mov(1, length);
        asm.mov(2, prot);
        asm.mov(3, flags);
        asm.mov(4, fd as u64);
        asm.mov(5, 0);
    })
}

/// **The heap seam.** The guest asks for anonymous memory, writes to it, and reads it back — all
/// in translated ARM64, so the mapping the handler made is the one the demand pager serves.
#[test]
fn a_guest_mmap_returns_memory_the_guest_can_write_and_read_back() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 128 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX, "MAP_FAILED");
    assert_ne!(at, 0);
    assert_eq!(at % f.guest.space.page_size() as u64, 0, "mmap returns page-aligned memory");

    // Fresh anonymous memory reads as zero, which the allocator this feeds depends on.
    let probe = f.guest.data + 0x400;
    let entry = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, probe as u64);
        asm.push(ldr_imm(11, 9, 0));
        asm.push(str_imm(11, 10, 0)); // what was there before any write
        asm.mov(11, 0x0BAD_F00D_DEAD_BEEF);
        asm.push(str_imm(11, 9, 0));
        asm.push(ldr_imm(12, 9, 0));
        asm.push(str_imm(12, 10, 8));
        // And the last page of the mapping, so the whole length is really there.
        asm.mov(13, at + length - 8);
        asm.push(str_imm(11, 13, 0));
        asm.push(ldr_imm(14, 13, 0));
        asm.push(str_imm(14, 10, 16));
    });
    let exit = run_program(&f, entry).expect("the guest must be able to use what mmap gave it");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(f.guest.read_u64(probe), 0, "anonymous memory arrives zeroed");
    assert_eq!(f.guest.read_u64(probe + 8), 0x0BAD_F00D_DEAD_BEEF);
    assert_eq!(f.guest.read_u64(probe + 16), 0x0BAD_F00D_DEAD_BEEF, "the last page is mapped too");
    // Mapped, not free. **Not** a length assertion: `region_at` reports the *entry*, and a lazily
    // committed mapping is split at every granule it commits, so the entry at `at` is one 64 KiB
    // granule rather than the whole 128 KiB. That is the shape the straddling-access fix records.
    assert!(f.guest.space.region_at(at as usize).is_some_and(|r| !r.is_free()));
}

/// `munmap` really takes the memory away: the same guest instruction that worked before the call
/// faults after it.
#[test]
fn munmap_takes_the_mapping_away_and_a_later_guest_access_faults() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);

    let code = value_of(&f, "munmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
    });
    assert_eq!(code as i64 as i32, 0, "munmap succeeded");
    assert!(f.guest.space.region_at(at as usize).is_none_or(|r| r.is_free()));

    let entry = program(&f, |asm| {
        asm.mov(9, at);
        asm.push(ldr_imm(10, 9, 0));
    });
    let exit = run_program(&f, entry).expect("the run itself must not fail");
    match exit {
        ExitReason::MemoryFault { address, .. } => assert_eq!(address as u64, at),
        other => panic!("reading unmapped memory must fault: {other:?}"),
    }
}

/// `mprotect` really changes what the guest may do: a store that worked is refused after it.
#[test]
fn mprotect_drops_a_mapping_to_read_only_and_the_guest_store_faults() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);

    let write = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x11);
        asm.push(str_imm(10, 9, 0));
    });
    assert!(matches!(
        run_program(&f, write).expect("writable"),
        ExitReason::Returned { .. }
    ));

    let code = value_of(&f, "mprotect", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 1); // PROT_READ
    });
    assert_eq!(code as i64 as i32, 0);

    let again = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x22);
        asm.push(str_imm(10, 9, 0));
    });
    match run_program(&f, again).expect("the run itself must not fail") {
        ExitReason::MemoryFault { address, .. } => assert_eq!(address as u64, at),
        other => panic!("a store to a read-only mapping must fault: {other:?}"),
    }
    // Reading still works, so the protection changed rather than the mapping disappearing.
    let read = program(&f, |asm| {
        asm.mov(9, at);
        asm.push(ldr_imm(10, 9, 0));
    });
    assert!(matches!(run_program(&f, read).expect("readable"), ExitReason::Returned { .. }));
}

/// **Every `mmap` shape this layer will not carry out is a refusal, and none of them is
/// `MAP_FAILED`.**
///
/// `MAP_FAILED` is the believable wrong answer: the guest's allocator handles it by trying
/// something else, and the real failure would surface later as an allocation pattern nobody could
/// explain.
#[test]
fn every_unimplementable_mmap_shape_is_refused_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let page = f.guest.space.page_size() as u64;

    // A real file descriptor.
    let file = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_PRIVATE_ONLY, 7);
    assert_eq!(file.symbol(), Some("mmap"));
    assert!(file.to_string().contains("file-backed"), "{file}");

    // No MAP_ANONYMOUS, even with fd = -1.
    let not_anon = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_PRIVATE_ONLY, -1);
    assert!(not_anon.to_string().contains("file-backed"), "{not_anon}");

    // MAP_FIXED, which Linux implements by destroying whatever is already there.
    let fixed = guest_mmap_refusal(&f, 0x4000_0000, page, PROT_RW, MAP_ANON_PRIVATE | 0x10, -1);
    assert!(fixed.to_string().contains("MAP_FIXED"), "{fixed}");
    assert!(fixed.to_string().contains("NOREPLACE"), "{fixed}");

    // A flag nobody implemented: MAP_GROWSDOWN.
    let unknown = guest_mmap_refusal(&f, 0, page, PROT_RW, MAP_ANON_PRIVATE | 0x0100, -1);
    assert!(unknown.to_string().contains("0x100"), "{unknown}");

    // Write without read, and write with execute.
    for prot in [2u64, 4, 6, 7] {
        let error = guest_mmap_refusal(&f, 0, page, prot, MAP_ANON_PRIVATE, -1);
        assert_eq!(error.symbol(), Some("mmap"), "prot {prot:#x}");
        assert!(error.to_string().contains(&format!("{prot:#x}")), "{error}");
    }
}

/// `MAP_PRIVATE` with no `MAP_ANONYMOUS`.
const MAP_PRIVATE_ONLY: u64 = 0x02;

/// A well-formed call that legitimately fails returns `MAP_FAILED` **and sets `errno`** — which is
/// the contract, not a stub. The guest reads `errno` through `__errno`, exactly as it would.
#[test]
fn a_legitimate_mmap_failure_is_map_failed_with_errno_and_not_a_refusal() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let out = f.guest.data + 0x400;

    // Length zero: EINVAL, which is what Linux answers.
    let entry = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out), u64::MAX, "MAP_FAILED is (void *) -1, not NULL");
    assert_eq!(f.guest.read_u64(out + 8), 22, "EINVAL is Linux's 22");

    // A length that would wrap when rounded up to a page must not become a small one.
    let wrapped = guest_mmap(&f, 0, u64::MAX, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_eq!(wrapped, u64::MAX, "MAP_FAILED, not a mapping");

    // And a length larger than the guest address space is ENOMEM rather than a panic.
    let huge = guest_mmap(&f, 0, 1 << 40, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_eq!(huge, u64::MAX);
}

/// `MAP_FIXED_NOREPLACE` is honoured because `Placement::Fixed` means exactly that, and a second
/// request for the same address fails rather than destroying the first mapping.
#[test]
fn map_fixed_noreplace_is_honoured_and_refuses_an_occupied_address() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let page = f.guest.space.page_size() as u64;
    let length = 64 * 1024;

    let first = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(first, u64::MAX);
    let second =
        guest_mmap(&f, first, length, PROT_RW, MAP_ANON_PRIVATE | 0x10_0000, -1, 0);
    assert_eq!(second, u64::MAX, "the address is taken, and MAP_FIXED_NOREPLACE must not replace");

    // A misaligned fixed address is EINVAL rather than a rounded-down mapping.
    let misaligned =
        guest_mmap(&f, page + 1, length, PROT_RW, MAP_ANON_PRIVATE | 0x10_0000, -1, 0);
    assert_eq!(misaligned, u64::MAX);
}

/// **`MADV_FREE` is implemented and `MADV_DONTNEED` is refused**, and the difference is their
/// contracts: `MADV_FREE` promises "the old contents or zeroes", which `advise_idle` gives, and
/// `MADV_DONTNEED` promises "zero, immediately", which it does not.
#[test]
fn madvise_implements_the_advice_it_can_honour_and_refuses_the_one_it_cannot() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let at = guest_mmap(&f, 0, length, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);
    // Touch it, so there is a committed granule for the advice to be about.
    let touch = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 0x55);
        asm.push(str_imm(10, 9, 0));
    });
    run_program(&f, touch).expect("writable");

    let freed = value_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 8); // MADV_FREE
    });
    assert_eq!(freed as i64 as i32, 0);

    let dontneed = refusal_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 4); // MADV_DONTNEED
    });
    assert_eq!(dontneed.symbol(), Some("madvise"));
    let text = dontneed.to_string();
    assert!(text.contains("MADV_DONTNEED"), "{text}");
    assert!(text.contains("zero"), "the refusal must name the guarantee it cannot meet: {text}");
    assert!(text.contains("MADV_FREE"), "and what is implemented instead: {text}");

    // The purely advisory ones succeed, because ignoring a hint that cannot change what a read
    // returns is the latitude the interface gives.
    for advice in [0u64, 1, 2, 3, 14, 15] {
        let code = value_of(&f, "madvise", |asm| {
            asm.mov(0, at);
            asm.mov(1, length);
            asm.mov(2, advice);
        });
        assert_eq!(code as i64 as i32, 0, "advice {advice}");
    }
    // And an advice nobody defined is EINVAL, which is what Linux answers.
    let unknown = value_of(&f, "madvise", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, 999);
    });
    assert_eq!(unknown as i64 as i32, -1);
}

/// `mlock` is refused, and the refusal says why `-1`/`ENOMEM` was rejected — it is the most
/// tempting wrong answer in the group, because a failing `mlock` is ordinary on a real device.
#[test]
fn mlock_is_refused_rather_than_answered_with_a_believable_failure() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let error = refusal_of(&f, "mlock", |asm| {
        asm.mov(0, f.guest.data as u64);
        asm.mov(1, 4096);
    });
    assert_eq!(error.symbol(), Some("mlock"));
    let text = error.to_string();
    assert!(text.contains("resident"), "{text}");
    assert!(text.contains("ENOMEM"), "the rejected alternative is named: {text}");
}

/// Hostile arguments to every one of the five: null, unaligned, wild, and lengths that would wrap.
/// None may panic, and each must be a typed error or a documented `-1`.
#[test]
fn hostile_arguments_to_the_guest_memory_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let wild = f.guest.unmapped as u64;

    for (symbol, args) in [
        ("munmap", vec![0u64, 0]),
        ("munmap", vec![1, 4096]),
        ("munmap", vec![wild, u64::MAX]),
        ("munmap", vec![u64::MAX, u64::MAX]),
        ("mprotect", vec![1, 4096, 1]),
        ("mprotect", vec![wild, u64::MAX, 1]),
        ("mprotect", vec![0, 0, 0]),
        ("madvise", vec![1, 4096, 8]),
        ("madvise", vec![wild, u64::MAX, 8]),
        ("madvise", vec![0, 0, 3]),
    ] {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            // Either it completed with a `-1`/`0`, or it refused by name. Both are fine; a panic
            // or a silent success that changed something is not.
            Ok(exit) => {
                assert!(matches!(exit, ExitReason::Returned { .. }), "`{symbol}` {args:?}: {exit:?}");
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert!(code == 0 || code == -1, "`{symbol}` {args:?} returned {code}");
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(symbol), "{error:?}");
            }
        }
    }

    // `mmap` with every argument hostile at once.
    let hostile = guest_mmap(&f, u64::MAX, u64::MAX, PROT_RW, MAP_ANON_PRIVATE, -1, u64::MAX);
    assert_eq!(hostile, u64::MAX);
    // `mlock` refuses whatever it is handed, including a wrapping length.
    let locked = refusal_of(&f, "mlock", |asm| {
        asm.mov(0, wild);
        asm.mov(1, u64::MAX);
    });
    assert_eq!(locked.symbol(), Some("mlock"));
}

/// The adapter's static pool refuses rather than wrapping its bump pointer, which would put one
/// image's `dlpi_name` on top of an `AMEDIAFORMAT_KEY_*` string.
#[test]
fn a_full_static_pool_is_a_refusal_and_not_an_overwrite() {
    use omni_android::bionic::POOL_BYTES;
    let _guard = serialized();
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let mut interned = Vec::new();
    let chunk = vec![b'x'; 255];
    loop {
        match bionic.intern("dl_iterate_phdr", &chunk) {
            Ok(at) => interned.push(at),
            Err(error) => {
                assert!(error.to_string().contains("static pool"), "{error}");
                break;
            }
        }
        assert!(interned.len() < POOL_BYTES, "the pool must run out");
    }
    assert!(!interned.is_empty());
    let unique: std::collections::BTreeSet<_> = interned.iter().collect();
    assert_eq!(unique.len(), interned.len(), "no two allocations share an address");
    assert!(bionic.pool_used() <= POOL_BYTES);
}
