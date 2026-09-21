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
use omni_android::bionic::{Bionic, ThreadHost};
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
    // The log sink writes to the host's stderr by default, which is what a real run wants and what
    // a suite that logs 266 lines on purpose does not. The instance's ring still records every
    // line, and the ring is what these tests assert on.
    bionic.set_log_to_stderr(false);
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
    assert_eq!(symbols.len(), 156, "bound symbols: {symbols:?}");
    // Phase 1 bound 86 — 84 inline and two re-entrant. Phase 2 added ten: the four `dl*` refusals
    // inline, and `dl_iterate_phdr` plus the five guest-memory calls on the exit path, for 96.
    // Phase 3a adds 23, all inline: five clocks, fourteen process-and-environment, four logging.
    // Phase 3b adds 29, all inline: eighteen descriptor symbols and eleven `FILE *` ones.
    // Phase 3c adds 4 inline (the signal family) and 4 re-entrant (thread lifecycle).
    assert_eq!(Bionic::inline_symbols().count(), 145);
    assert_eq!(Bionic::reentrant_symbols().count(), 11);
    // Plus the eighteen `STT_OBJECT` data objects, which are not functions and are not bound to a
    // handler at all. 156 + 18 = 174 of the 188 the initializers reach.
    assert_eq!(omni_android::bionic::DATA_OBJECTS.len(), 18);

    // **Membership, not just a total** — a count cannot see a substitution, and this project has
    // had a list whose count stayed right while two members were wrong and two were missing. The
    // 23 phase 3a binds are named one by one.
    let bound: std::collections::BTreeSet<&str> = symbols.iter().copied().collect();
    let phase_3a = [
        // clocks
        "clock_gettime",
        "gettimeofday",
        "gmtime_r",
        "nanosleep",
        "usleep",
        // process and environment
        "getpid",
        "sched_getcpu",
        "arc4random_buf",
        "getauxval",
        "getenv",
        "__system_property_get",
        "abort",
        "__stack_chk_fail",
        "_exit",
        "android_set_abort_message",
        "sysconf",
        "sysinfo",
        "prctl",
        "syscall",
        // logging
        "__android_log_print",
        "syslog",
        "openlog",
        "closelog",
    ];
    assert_eq!(phase_3a.len(), 23);
    for symbol in phase_3a {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3a's scope and is not bound");
    }

    // **Phase 3b's 29, named one by one**, derived from the plan's `3b` row and cross-checked
    // against `tools/os_surface.py`'s `file-io` bucket intersected with the reachable remainder.
    let phase_3b = [
        // the eighteen descriptor symbols
        "__open_2",
        "__write_chk",
        "access",
        "close",
        "closedir",
        "fstat",
        "lstat",
        "mkdir",
        "open",
        "opendir",
        "pread",
        "read",
        "readdir",
        "rename",
        "rmdir",
        "stat",
        "statvfs",
        "unlink",
        // bionic's `FILE *` layer on top of them
        "fclose",
        "fdopen",
        "feof",
        "fflush",
        "fgets",
        "fileno",
        "fopen",
        "fputc",
        "fputs",
        "fread",
        "fwrite",
    ];
    assert_eq!(phase_3b.len(), 29);
    for symbol in phase_3b {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3b's scope and is not bound");
    }

    // **Phase 3c's eight, named one by one**, derived the same way: the 188 minus everything
    // named in `handlers.rs` and `data.rs`, intersected with the plan's `3c` row. Four of them
    // are answered, three refuse by name and one -- `sigfillset` -- is pure computation over the
    // guest's own `sigset_t` and is implemented in `omni-bionic`.
    let phase_3c = [
        "pthread_create",
        "pthread_detach",
        "pthread_getschedparam",
        "pthread_join",
        "pthread_sigmask",
        "raise",
        "sigaction",
        "sigfillset",
    ];
    assert_eq!(phase_3c.len(), 8);
    for symbol in phase_3c {
        assert!(bound.contains(symbol), "`{symbol}` is in phase 3c's scope and is not bound");
    }
    // The three that create, join and detach a thread are on the **exit path**, and that is F9
    // rather than a preference: a start routine is guest code and `pthread_create` maps the new
    // thread's stack. `pthread_getschedparam` does neither and is inline with the four signal
    // symbols, which also hold no CPU.
    let reentrant: std::collections::BTreeSet<&str> = Bionic::reentrant_symbols().collect();
    for symbol in ["pthread_create", "pthread_join", "pthread_detach"] {
        assert!(reentrant.contains(symbol), "`{symbol}` must be re-entrant (F9)");
    }
    for symbol in ["sigfillset", "sigaction", "raise", "pthread_sigmask", "pthread_getschedparam"] {
        assert!(!reentrant.contains(symbol), "`{symbol}` holds no CPU and belongs inline");
    }

    // And the complement: the groups phase 3b deliberately does not touch stay `Unbound`, so that
    // "not done yet" and "done" cannot be confused by anyone reading the count. These are the
    // remaining 22 — eight sockets, eight threads and signals, and the six nothing else claims.
    let still_unbound = [
        // network (3c in the plan's table)
        "eventfd",
        "freeaddrinfo",
        "gai_strerror",
        "getaddrinfo",
        "inet_ntop",
        "poll",
        "select",
        "socket",
        // the remainder nothing else claims
        "clock",
        "time",
        "mallinfo",
        "longjmp",
        "__gcov_dump",
        "__gcov_flush",
    ];
    assert_eq!(still_unbound.len(), 14, "188 - 156 bound - 18 data objects");
    for symbol in still_unbound {
        assert!(
            !bound.contains(symbol),
            "`{symbol}` belongs to a later phase and must still name itself when called"
        );
    }

    // The two lists together are the whole remainder, asserted as a **set difference against the
    // reachable file** rather than as a total: a count cannot see a substitution, and this project
    // has had a list whose count stayed right while two members were wrong and two were missing.
    let reachable = reachable_imports();
    let data: std::collections::BTreeSet<&str> =
        omni_android::bionic::DATA_OBJECTS.iter().map(|o| o.symbol).collect();
    let remainder: std::collections::BTreeSet<String> = reachable
        .iter()
        .filter(|symbol| !bound.contains(symbol.as_str()) && !data.contains(symbol.as_str()))
        .cloned()
        .collect();
    let expected: std::collections::BTreeSet<String> =
        still_unbound.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(remainder, expected, "the unbound remainder is not the list above");
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
    // Phase 3c's three, which are on the exit path for **both** of F9's reasons at once: a start
    // routine is guest code, and `pthread_create` maps the new thread's stack. The fourth,
    // `pthread_getschedparam`, reaches neither and stays inline — putting it here for tidiness
    // would cost it 3x per call for nothing, which is the over-correction half of this test.
    for symbol in ["pthread_create", "pthread_join", "pthread_detach"] {
        assert!(reentrant.contains(symbol), "`{symbol}` belongs with thread lifecycle (F9)");
    }
    assert!(
        !reentrant.contains("pthread_getschedparam"),
        "`pthread_getschedparam` runs no guest code and touches no mapping (D17)"
    );
    assert_eq!(reentrant.len(), 11, "nothing else belongs on the slow path: {reentrant:?}");
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
/// itself. That is the design, not a gap: a `socket` bound to a stub returning a plausible
/// descriptor would surface three thousand initializers later somewhere unrelated.
///
/// The symbol here was `fopen` until phase 3b bound it; it is `socket` now, which phase 3c will
/// bind and which will move it on again. That churn is the test working.
#[test]
fn a_symbol_this_phase_does_not_implement_is_unbound_and_says_so() {
    let _guard = serialized();
    let f = fixture();
    let thunk = f.boundary.slot_named("socket").map(|s| s.address);
    assert!(thunk.is_none(), "socket must not be bound by this phase");

    // One that *is* declared, because the loader would have asked for it: bind it as the loader
    // would and confirm the call names it.
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let builder = guest.boundary(256);
    bionic.bind_into(&builder).expect("bind");
    let unbound = builder.declare_function("socket").expect("a slot");
    let boundary = builder.finish();
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(unbound);
    asm.push(ret(21));
    guest.load(asm.words());
    let mut cpu = guest.thread(&boundary);
    let _active = bionic.activate().expect("a thread block");
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("socket is not implemented");
    assert!(matches!(error, AbiError::Unbound { .. }), "{error:?}");
    assert_eq!(error.symbol(), Some("socket"));
    assert_eq!(error.guest_address(), Some(unbound));
}

/// The five printf-family symbols that are *bound* but cannot be serviced refuse by name, and the
/// reason says which missing piece. `Unbound` would have said only "not implemented".
#[test]
fn the_unservable_printf_family_refuses_with_the_missing_piece_named() {
    let _guard = serialized();
    let f = fixture();
    for (symbol, needle) in [
        // Phase 3b built the stream layer these two named as missing, so their refusals now
        // name what is *actually* missing -- the binding -- rather than a surface that exists.
        ("fprintf", "only the binding"),
        ("vfprintf", "only the binding"),
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

// =================================================================== signals (phase 3c)

/// **`sigfillset` really fills the guest's `sigset_t`, and stops at its end.**
///
/// Asserted on the *bytes*, not on the return value: a handler that returned 0 and wrote nothing
/// would pass any assertion about the result, and the guest would carry an uninitialised set
/// forward. The guard word after the set is what catches a write sized from the wrong constant.
#[test]
fn sigfillset_fills_every_bit_of_the_guests_sigset() {
    let _guard = serialized();
    let f = fixture();
    let set = f.guest.data + 0x200;
    f.guest.write_u64(set, 0);
    f.guest.write_u64(set + 8, 0x1234_5678_9ABC_DEF0);
    let result = value_of(&f, "sigfillset", |asm| {
        asm.mov(0, set as u64);
    });
    assert_eq!(result, 0, "sigfillset returns 0 on success");
    assert_eq!(f.guest.read_u64(set), u64::MAX, "every one of the 64 signal bits");
    assert_eq!(
        f.guest.read_u64(set + 8),
        0x1234_5678_9ABC_DEF0,
        "and nothing past sizeof(sigset_t), which is 8 on LP64"
    );
}

/// A null `set` is bionic's own `-1` with `EINVAL`, read back through `__errno` the way the guest
/// would read it. It is not a refusal: guest code has a branch for this one.
#[test]
fn sigfillset_with_a_null_set_is_minus_one_with_einval() {
    let _guard = serialized();
    let f = fixture();
    let out = f.guest.data + 0x300;
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 0);
    asm.bl(f.thunk("sigfillset"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 0));
    asm.bl(f.thunk("__errno"));
    asm.push(ldr_w(1, 0, 0));
    asm.push(str_imm(1, 22, 8));
    asm.push(ret(21));
    f.guest.load(asm.words());
    let mut cpu = f.guest.thread(&f.boundary);
    assert!(matches!(f.run(&mut cpu, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out) as i64, -1, "sigfillset(NULL) is -1, sign-extended");
    assert_eq!(f.guest.read_u64(out + 8), 22, "EINVAL is Linux's 22");
}

/// A `sigset_t` the guest cannot write to is a typed refusal naming the symbol, not a host
/// access violation and not a silent success.
#[test]
fn sigfillset_on_read_only_guest_memory_is_a_typed_refusal() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "sigfillset", |asm| {
        asm.mov(0, f.guest.readonly as u64);
    });
    assert_eq!(error.symbol(), Some("sigfillset"));
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
}

/// **The three that cannot be delivered refuse by name, and say what is missing.**
///
/// `Unbound` would have said only "nothing implements this". A refusal says *why*, names the
/// signal number the guest asked about, and — for `raise` — says explicitly that mapping
/// `SIGABRT` onto `abort`'s reported termination was considered and declined. A `0` from any of
/// these is the failure Global Constraint 1 is about: it is not observable until the thing the
/// guest registered for actually happens.
#[test]
fn the_undeliverable_signal_family_refuses_by_name() {
    let _guard = serialized();
    let f = fixture();
    // SIGSEGV(11) for sigaction, SIGABRT(6) for raise, SIG_BLOCK(0) for the mask.
    for (symbol, needles, setup) in [
        (
            "sigaction",
            vec!["SIGSEGV", "no guest signal delivery", "installing a handler"],
            vec![11u64, 0x4000, 0x5000],
        ),
        ("raise", vec!["SIGABRT", "`abort`"], vec![6, 0, 0]),
        ("pthread_sigmask", vec!["SIG_BLOCK", "D19"], vec![0, 0x4000, 0]),
    ] {
        let error = refusal_of(&f, symbol, |asm| {
            for (index, value) in setup.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        assert_eq!(error.symbol(), Some(symbol));
        assert_eq!(error.guest_address(), Some(f.thunk(symbol)));
        assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
        let text = error.to_string();
        for needle in needles {
            assert!(text.contains(needle), "`{symbol}` must say `{needle}`: {text}");
        }
    }
}

/// `sigaction`'s **query** form is refused too, and says which form it was.
///
/// It is the one that looks answerable — nothing can have installed a handler, so `SIG_DFL` is
/// arithmetically true — and answering it would mean writing a `struct sigaction` whose layout
/// has never been checked against a header on this machine, to describe a table that does not
/// exist.
#[test]
fn the_query_form_of_sigaction_is_refused_and_names_itself_as_a_query() {
    let _guard = serialized();
    let f = fixture();
    let error = refusal_of(&f, "sigaction", |asm| {
        asm.mov(0, 13); // SIGPIPE
        asm.mov(1, 0); // act == NULL: a query
        asm.mov(2, f.guest.data as u64 + 0x400);
    });
    let text = error.to_string();
    assert!(text.contains("querying the current disposition"), "{text}");
    assert!(text.contains("SIGPIPE"), "{text}");
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
    bionic.set_log_to_stderr(false);
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

    // A length that would wrap when rounded up to a page must not become a small one. **The
    // errno is asserted, not only the return**, because both a wrap and an ordinary too-large
    // request answer MAP_FAILED: saturating the round-up rather than checking it would turn this
    // into an ENOMEM for a mapping of `usize::MAX & !4095` bytes, and the return value alone
    // cannot tell the two apart.
    let entry = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, u64::MAX);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 16));
        asm.bl(f.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), u64::MAX, "MAP_FAILED, not a mapping");
    assert_eq!(
        f.guest.read_u64(out + 24),
        12,
        "ENOMEM is Linux's 12, and it is what PAGE_ALIGN(len) == 0 answers there"
    );

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

/// **The demand pager is the heap seam (D10), and this is what says so.** A guest `mmap` is
/// `CommitPolicy::Lazy`, so 16 MiB of address space costs no commit charge until the guest touches
/// it — and then one granule, not sixteen megabytes.
///
/// `libroblox.so` carries its own allocator and reaches the host only through this call, so an
/// eager `mmap` would charge the whole of every arena the engine reserves.
#[test]
fn a_guest_mmap_costs_no_commit_charge_until_the_guest_touches_it() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let granule = f.guest.space.commit_granule();
    let length = 16 * 1024 * 1024;
    // **A warm-up call first, and it is not tidiness.** Every `value_of` creates a guest thread,
    // and the first one takes a TLS block out of `omni-cpu`'s lazily-committed arena — which is a
    // granule of commit charge that has nothing to do with `mmap`. Measuring from before it would
    // have attributed 64 KiB of somebody else's charge to this call, in the direction that makes
    // a lazy mapping look eager.
    let warmup = guest_mmap(&f, 0, 4096, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(warmup, u64::MAX);
    let before = f.guest.space.stats();

    let at = guest_mmap(&f, 0, length as u64, PROT_RW, MAP_ANON_PRIVATE, -1, 0);
    assert_ne!(at, u64::MAX);
    let mapped = f.guest.space.stats();
    assert_eq!(mapped.mapped - before.mapped, length, "the address space really was claimed");
    assert_eq!(mapped.free, before.free - length, "and it came out of the free space");
    assert_eq!(
        mapped.committed, before.committed,
        "and none of it was committed: address space is free, commit charge is not (D10)"
    );

    let touch = program(&f, |asm| {
        asm.mov(9, at);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
    });
    run_program(&f, touch).expect("the guest can write to it");
    let touched = f.guest.space.stats();
    assert_eq!(
        touched.committed - mapped.committed,
        granule,
        "one granule, committed on demand — not the whole mapping"
    );
}

/// `MAP_SHARED` on anonymous memory is accepted, and is the same thing as `MAP_PRIVATE` here.
///
/// The two differ only across a `fork`, and there is no `fork`: it is not in the reachable set and
/// there is no process surface to build one on. Refusing `MAP_SHARED` would be an over-correction
/// that fails a correct program, which is the direction mutation row `guestmem-B1` exists for.
#[test]
fn an_anonymous_map_shared_is_accepted_because_there_is_no_fork_to_share_with() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let shared = guest_mmap(&f, 0, length, PROT_RW, 0x21, -1, 0);
    assert_ne!(shared, u64::MAX, "MAP_SHARED | MAP_ANONYMOUS is a legitimate request");
    let entry = program(&f, |asm| {
        asm.mov(9, shared);
        asm.mov(10, 0x5A);
        asm.push(str_imm(10, 9, 0));
    });
    assert!(matches!(run_program(&f, entry).expect("usable"), ExitReason::Returned { .. }));

    // A sharing mode that is neither is EINVAL, which is what Linux answers — `MAP_TYPE` is a
    // three-bit field and `3` is `MAP_SHARED_VALIDATE`, which this layer does not implement.
    let neither = guest_mmap(&f, 0, length, PROT_RW, 0x20, -1, 0);
    assert_eq!(neither, u64::MAX, "no sharing mode at all is EINVAL");
}

/// **A guest that unmaps code and maps different code at the same address must run the new code.**
///
/// The backend caches translations by guest address, so `munmap` and `mprotect` have to discard
/// them or the second call runs the first program. This is the reason the guest-memory group is on
/// the exit path in the first place: `ImportCall` holds no CPU, so an inline handler could not
/// invalidate anything even if changing the address space were safe from one.
///
/// **One CPU context for the whole test, and that is what makes it a detector.** The translating
/// backend's code cache is per context (D5: unshared per-thread code caches), so a version that
/// took a fresh context for each step translated everything afresh every time and could not tell
/// an invalidated cache from an empty one. That version passed with the invalidation removed —
/// mutation row `guestmem-A1` came back NOT CAUGHT, which is what found it.
///
/// Both programs are two instructions and differ only in the constant they return, so a failure
/// here is unambiguous: `0xAA` where `0xBB` belongs is the old translation.
#[test]
fn code_at_a_reused_address_is_retranslated_rather_than_run_from_the_cache() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let length = 64 * 1024;
    let out = f.guest.data + 0x400;
    let mut cpu = f.guest.thread(&f.boundary);

    let at = {
        let entry = call_one(&f, "mmap", |asm| {
            asm.mov(0, 0);
            asm.mov(1, length);
            asm.mov(2, PROT_RW);
            asm.mov(3, MAP_ANON_PRIVATE);
            asm.mov(4, u64::MAX);
            asm.mov(5, 0);
        });
        f.guest.rearm(&mut cpu, &f.boundary);
        f.run(&mut cpu, entry).expect("mmap");
        f.guest.read_u64(f.guest.data)
    };
    assert_ne!(at, u64::MAX);

    // Write a two-instruction program at `at`, make it executable through the guest's own
    // `mprotect`, call it, and store what it answered.
    let install_and_call =
        |f: &Fixture, cpu: &mut omni_cpu::dynarmic::DynarmicCpu, answer: u16, slot: u32| {
        f.guest.space.ensure_committed(at as usize, 16).expect("the first page of the mapping");
        f.guest.write_bytes(
            at as usize,
            &[movz(0, answer, 0).to_le_bytes(), ret(30).to_le_bytes()].concat(),
        );
        let protect = call_one(f, "mprotect", |asm| {
            asm.mov(0, at);
            asm.mov(1, length);
            asm.mov(2, 5); // PROT_READ | PROT_EXEC
        });
        f.guest.rearm(cpu, &f.boundary);
        f.run(cpu, protect).expect("mprotect");
        assert_eq!(f.guest.read_u64(f.guest.data) as i64 as i32, 0);

        // A fresh caller each time, so what is under test is the cached translation of the callee
        // at `at` rather than of the caller.
        let caller = program(f, |asm| {
            asm.mov(9, at);
            asm.mov(10, out as u64);
            asm.push(blr(9));
            asm.push(str_imm(0, 10, slot));
        });
        f.guest.rearm(cpu, &f.boundary);
        let exit = f.run(cpu, caller).expect("the guest runs what it mapped");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    };

    install_and_call(&f, &mut cpu, 0xAA, 0);
    assert_eq!(f.guest.read_u64(out), 0xAA);

    // Give the range back and take it again at the same address, then put a different program
    // there. Without the invalidation in `munmap`/`mprotect` the cached translation of the first
    // one still answers.
    let unmap = call_one(&f, "munmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
    });
    f.guest.rearm(&mut cpu, &f.boundary);
    f.run(&mut cpu, unmap).expect("munmap");
    assert_eq!(f.guest.read_u64(f.guest.data) as i64 as i32, 0);

    let remap = call_one(&f, "mmap", |asm| {
        asm.mov(0, at);
        asm.mov(1, length);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE | 0x10_0000);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
    });
    f.guest.rearm(&mut cpu, &f.boundary);
    f.run(&mut cpu, remap).expect("mmap");
    assert_eq!(
        f.guest.read_u64(f.guest.data),
        at,
        "MAP_FIXED_NOREPLACE must give the address back"
    );

    install_and_call(&f, &mut cpu, 0xBB, 8);
    assert_eq!(
        f.guest.read_u64(out + 8),
        0xBB,
        "0xAA here is the first program's translation, served out of the code cache after the          memory it was translated from was unmapped"
    );
}


// =================================================================== M3 task 3 phase 3a
//
// Clocks, process and environment, and the log sink — the first symbols in this adapter whose
// answers come from `omni-platform`.

use omni_android::bionic::{HwcapPolicy, LogPriority, HWCAP_ATOMICS, LOG_CAPTURE_MAX, PROP_VALUE_MAX};

/// Linux `clockid_t` values, as guest code passes them.
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_BOOTTIME: u64 = 7;

/// Read a guest `int` at any 4-byte-aligned address.
///
/// The harness reads 8 bytes at a time from an 8-aligned address, and a `struct tm` is nine `int`s
/// in a row — so half of them start at offset 4 of their word.
fn read_i32(f: &Fixture, at: omni_cpu::GuestAddr) -> i32 {
    assert_eq!(at % 4, 0, "an int is 4-byte aligned");
    let word = f.guest.read_u64(at & !7);
    let shift = 32 * ((at & 7) / 4);
    (word >> shift) as u32 as i32
}

// ------------------------------------------------------------------ clocks

/// **Both clocks are answered from the platform seam, and the ids this layer does not model are
/// refused by number.**
///
/// The refusal half is the part that matters. `clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts)`
/// answered with wall time is a plausible, monotonic number of seconds that is not what was asked
/// for, and a guest profiler built on it would report wall time as CPU time forever.
#[test]
fn clock_gettime_answers_the_two_clocks_and_refuses_the_ones_it_does_not_model() {
    let _guard = serialized();
    let f = fixture();
    let ts = f.guest.data + 0x200;

    for id in [CLOCK_MONOTONIC, 4 /* _RAW */, 6 /* _COARSE */] {
        f.guest.write_u64(ts, u64::MAX);
        f.guest.write_u64(ts + 8, u64::MAX);
        let code = value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(code as i64 as i32, 0, "clock {id}");
        let nanos = f.guest.read_u64(ts + 8);
        assert!(nanos < 1_000_000_000, "clock {id}: tv_nsec is {nanos}, which is not a fraction");
        // The monotonic clock is measured from a process epoch, so a *plausible* reading is a
        // small number of seconds rather than a Unix time. Asserting the upper bound is what
        // catches "monotonic was served from the wall clock", which is otherwise invisible.
        let seconds = f.guest.read_u64(ts);
        assert!(seconds < 1_000_000, "clock {id}: {seconds} s looks like a wall clock");
    }

    // Monotonic never goes backwards across two real calls through the boundary.
    let first = {
        value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, CLOCK_MONOTONIC);
            asm.mov(1, ts as u64);
        });
        (f.guest.read_u64(ts), f.guest.read_u64(ts + 8))
    };
    let second = {
        value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, CLOCK_MONOTONIC);
            asm.mov(1, ts as u64);
        });
        (f.guest.read_u64(ts), f.guest.read_u64(ts + 8))
    };
    assert!(second >= first, "CLOCK_MONOTONIC went backwards: {first:?} -> {second:?}");

    for id in [CLOCK_REALTIME, 5 /* _COARSE */] {
        let code = value_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(code as i64 as i32, 0, "clock {id}");
        // 2020-01-01. A wall clock below it is a host whose clock is not set, which is worth
        // knowing about; the point of the bound is that it separates a wall clock from a
        // monotonic one, and nothing narrower would.
        assert!(f.guest.read_u64(ts) > 1_577_836_800, "clock {id} is not a wall clock");
    }

    for (id, name) in [
        (CLOCK_PROCESS_CPUTIME_ID, "CLOCK_PROCESS_CPUTIME_ID"),
        (CLOCK_THREAD_CPUTIME_ID, "CLOCK_THREAD_CPUTIME_ID"),
        (CLOCK_BOOTTIME, "CLOCK_BOOTTIME"),
    ] {
        let error = refusal_of(&f, "clock_gettime", |asm| {
            asm.mov(0, id);
            asm.mov(1, ts as u64);
        });
        assert_eq!(error.symbol(), Some("clock_gettime"));
        let text = error.to_string();
        assert!(text.contains(name), "the refusal must name the clock asked for: {text}");
    }
    // And one nobody has a name for is still refused, with its number in the message.
    let unknown = refusal_of(&f, "clock_gettime", |asm| {
        asm.mov(0, 4242);
        asm.mov(1, ts as u64);
    });
    assert!(unknown.to_string().contains("4242"), "{unknown}");
}

/// **`gettimeofday` writes MICROseconds**, and zeroes the obsolete `struct timezone`.
///
/// The thousand-fold error is the one this catches: `tv_usec` filled with nanoseconds is still a
/// number under a billion and still increases, so nothing but a range check sees it.
#[test]
fn gettimeofday_writes_microseconds_and_zeroes_the_obsolete_timezone() {
    let _guard = serialized();
    let f = fixture();
    let tv = f.guest.data + 0x200;
    let tz = f.guest.data + 0x240;
    f.guest.write_u64(tv, u64::MAX);
    f.guest.write_u64(tv + 8, u64::MAX);
    f.guest.write_u64(tz, 0xAAAA_AAAA_AAAA_AAAA);

    let code = value_of(&f, "gettimeofday", |asm| {
        asm.mov(0, tv as u64);
        asm.mov(1, tz as u64);
    });
    assert_eq!(code as i64 as i32, 0);
    assert!(f.guest.read_u64(tv) > 1_577_836_800, "tv_sec must be a wall clock");
    let micros = f.guest.read_u64(tv + 8);
    assert!(micros < 1_000_000, "tv_usec is {micros}: a struct timeval is MICROseconds");
    assert_eq!(f.guest.read_u64(tz), 0, "Linux fills the obsolete struct timezone with zeroes");

    // Both null is legal and writes nothing: `gettimeofday(NULL, NULL)` must not fault.
    let code = value_of(&f, "gettimeofday", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
    assert_eq!(code as i64 as i32, 0, "a null tv is legal — the call is then only about tz");
}

/// **`gmtime_r` fills the guest's `struct tm` at the documented offsets.**
///
/// A leap day in a year divisible by 400 is the date chosen, because it exercises the century rule
/// in both directions at once. Asserted field by field against a date anyone can check, not
/// against the conversion's own inverse.
#[test]
fn gmtime_r_breaks_a_real_timestamp_down_into_the_guest_struct_tm() {
    let _guard = serialized();
    let f = fixture();
    let timer = f.guest.data + 0x200;
    let result = f.guest.data + 0x240;
    // 2000-02-29T12:00:00Z, a Tuesday, day 59 of the year.
    f.guest.write_u64(timer, 951_825_600);
    for offset in (0..56).step_by(8) {
        f.guest.write_u64(result + offset, 0xAAAA_AAAA_AAAA_AAAA);
    }

    let returned = value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(returned, result as u64, "gmtime_r returns the buffer it was given");

    assert_eq!(read_i32(&f, result), 0, "tm_sec");
    assert_eq!(read_i32(&f, result + 4), 0, "tm_min");
    assert_eq!(read_i32(&f, result + 8), 12, "tm_hour");
    assert_eq!(read_i32(&f, result + 12), 29, "tm_mday");
    assert_eq!(read_i32(&f, result + 16), 1, "tm_mon is 0-based, so February is 1");
    assert_eq!(read_i32(&f, result + 20), 100, "tm_year is years since 1900");
    assert_eq!(read_i32(&f, result + 24), 2, "tm_wday: 2000-02-29 was a Tuesday");
    assert_eq!(read_i32(&f, result + 28), 59, "tm_yday is 0-based");
    assert_eq!(read_i32(&f, result + 32), 0, "UTC has no daylight saving");
    assert_eq!(f.guest.read_u64(result + 40), 0, "tm_gmtoff: UTC's offset is zero");
    let zone = f.guest.read_u64(result + 48) as omni_cpu::GuestAddr;
    assert_ne!(zone, 0, "tm_zone is a `const char *` guest code prints");
    assert_eq!(f.read_cstring(zone), b"UTC", "and it says UTC, because gmtime is UTC");

    // A pre-1970 timestamp: the half of the calendar arithmetic that a truncating division gets
    // wrong by exactly one day, and only before the epoch.
    f.guest.write_u64(timer, (-1i64) as u64);
    value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(read_i32(&f, result + 20), 69, "1969");
    assert_eq!(read_i32(&f, result + 16), 11, "December");
    assert_eq!(read_i32(&f, result + 12), 31);
    assert_eq!(read_i32(&f, result + 8), 23);

    // A year that will not fit `int tm_year` is NULL with EOVERFLOW, which is C's answer and not
    // a stub. The struct is left as it was rather than half written.
    f.guest.write_u64(result, 0x1234_5678_9ABC_DEF0);
    f.guest.write_u64(timer, i64::MAX as u64);
    let refused = value_of(&f, "gmtime_r", |asm| {
        asm.mov(0, timer as u64);
        asm.mov(1, result as u64);
    });
    assert_eq!(refused, 0, "an out-of-range year is NULL, not a wrapped date");
    assert_eq!(
        f.guest.read_u64(result),
        0x1234_5678_9ABC_DEF0,
        "a failed conversion must not have written anything"
    );
}

/// **A sleep really sleeps, a malformed request is `EINVAL`, and a request past the cap refuses.**
///
/// The cap is the hostile-input half: a sleeping thread executes no guest instructions, so D16's
/// step-budget watchdog cannot end one, and `nanosleep({INT64_MAX, 0})` would be a permanent hang
/// of the host thread that serviced it.
///
/// The duration assertion is **one-sided**, which is the only side `nanosleep` and Windows' ~15.6 ms
/// timer tick between them guarantee. n = 1: a lower bound on a sleep is not a rare event and does
/// not need a sample — every run either slept or did not.
#[test]
fn nanosleep_sleeps_reports_einval_and_refuses_a_request_past_the_cap() {
    let _guard = serialized();
    let f = fixture();
    let req = f.guest.data + 0x200;
    let rem = f.guest.data + 0x240;

    // 10 ms.
    f.guest.write_u64(req, 0);
    f.guest.write_u64(req + 8, 10_000_000);
    f.guest.write_u64(rem, 0xAAAA_AAAA_AAAA_AAAA);
    f.guest.write_u64(rem + 8, 0xAAAA_AAAA_AAAA_AAAA);
    let before = std::time::Instant::now();
    let code = value_of(&f, "nanosleep", |asm| {
        asm.mov(0, req as u64);
        asm.mov(1, rem as u64);
    });
    let elapsed = before.elapsed();
    assert_eq!(code as i64 as i32, 0);
    assert!(
        elapsed >= std::time::Duration::from_millis(10),
        "a 10 ms nanosleep returned after {elapsed:?}, so it did not sleep"
    );
    assert_eq!(f.guest.read_u64(rem), 0, "no signals are delivered, so nothing remains");
    assert_eq!(f.guest.read_u64(rem + 8), 0);

    // POSIX's validity rule, at both edges. `-1` with `errno` is the C library's own answer to a
    // malformed request and is a contract rather than a stub.
    for (seconds, nanos) in [(0u64, 1_000_000_000u64), (0, (-1i64) as u64), ((-1i64) as u64, 0)] {
        f.guest.write_u64(req, seconds);
        f.guest.write_u64(req + 8, nanos);
        let code = value_of(&f, "nanosleep", |asm| {
            asm.mov(0, req as u64);
            asm.mov(1, 0);
        });
        assert_eq!(code as i64 as i32, -1, "{seconds}s + {nanos}ns must be EINVAL");
    }

    // Past the cap: refused by name, with both numbers in the message.
    f.guest.write_u64(req, i64::MAX as u64);
    f.guest.write_u64(req + 8, 0);
    let error = refusal_of(&f, "nanosleep", |asm| {
        asm.mov(0, req as u64);
        asm.mov(1, 0);
    });
    assert_eq!(error.symbol(), Some("nanosleep"));
    let text = error.to_string();
    assert!(text.contains("60"), "the refusal must name the cap: {text}");
    assert!(text.contains(&i64::MAX.to_string()), "and what was asked for: {text}");
}

/// **`usleep` takes only the low 32 bits of `X0`**, because `useconds_t` is `unsigned int`.
///
/// The structural assertion, and the reason it is structural rather than timed: AAPCS64 does not
/// require a caller to clear the high half of a register holding a 32-bit argument, so a handler
/// that read all 64 bits would turn a perfectly ordinary 100 µs sleep into a request for 584,000
/// years — which the cap would then *refuse*. So the test is "a correct call is not refused", and
/// it fails loudly against the wrong read rather than hanging.
#[test]
fn usleep_reads_only_the_low_thirty_two_bits_of_its_argument() {
    let _guard = serialized();
    let f = fixture();
    let code = value_of(&f, "usleep", |asm| {
        asm.mov(0, 0xFFFF_FFFF_0000_0064);
    });
    assert_eq!(code as i64 as i32, 0, "100 us with a dirty high half must still be 100 us");

    // And the cap still applies to a value that really is large: 0xFFFF_FFFF us is 4,294 s.
    let error = refusal_of(&f, "usleep", |asm| {
        asm.mov(0, 0xFFFF_FFFF);
    });
    assert_eq!(error.symbol(), Some("usleep"));
    assert!(error.to_string().contains("60"), "{error}");
}

// ------------------------------------------------------------------ process and environment

/// `getpid` is the host's, and `sched_getcpu` is answered or refused by name — never `-1`.
#[test]
fn getpid_and_sched_getcpu_come_from_the_platform_seam() {
    let _guard = serialized();
    let f = fixture();
    let pid = value_of(&f, "getpid", |_| {}) as i64 as i32;
    assert_eq!(
        pid,
        std::process::id() as i32,
        "several guest instances share one host process, exactly as several threads of an \
         Android process share one pid"
    );

    let entry = call_one(&f, "sched_getcpu", |_| {});
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Ok(_) => {
            let id = f.guest.read_u64(f.guest.data) as i64 as i32;
            assert!(id >= 0, "a processor number is never negative: {id}");
        }
        Err(error) => {
            // The other four targets have no cpu-id backend and must say so by name. `-1` is a
            // documented `sched_getcpu` failure that callers route around, so it is the one
            // answer that would hide the gap.
            assert_eq!(error.symbol(), Some("sched_getcpu"));
            assert!(error.to_string().contains("sched_getcpu"), "{error}");
        }
    }
}

/// **`arc4random_buf` fills exactly what it was given, with bytes that differ between draws.**
///
/// Structural, not statistical: 64 bytes left at zero is what a backend that reports success
/// without writing looks like, two identical 64-byte draws is what a constant source looks like,
/// and an untouched sentinel past the end is what a length bug looks like. None of the three is a
/// randomness-quality claim and none is a flake risk at 2^-512.
#[test]
fn arc4random_buf_fills_exactly_the_buffer_it_was_given() {
    let _guard = serialized();
    let f = fixture();
    let first = f.guest.data + 0x400;
    let second = f.guest.data + 0x500;
    // 64 bytes of buffer followed by 32 bytes of sentinel.
    for offset in (0..96).step_by(8) {
        f.guest.write_u64(first + offset, 0xAAAA_AAAA_AAAA_AAAA);
        f.guest.write_u64(second + offset, 0xAAAA_AAAA_AAAA_AAAA);
    }

    for at in [first, second] {
        value_of(&f, "arc4random_buf", |asm| {
            asm.mov(0, at as u64);
            asm.mov(1, 64);
        });
    }
    let read = |at: omni_cpu::GuestAddr| -> Vec<u64> {
        (0..64).step_by(8).map(|o| f.guest.read_u64(at + o)).collect()
    };
    let a = read(first);
    let b = read(second);
    assert!(a.iter().any(|&w| w != 0), "the buffer was reported filled and is all zero");
    assert!(
        a.iter().any(|&w| w != 0xAAAA_AAAA_AAAA_AAAA),
        "the buffer was reported filled and is untouched"
    );
    assert_ne!(a, b, "two draws from a real entropy source cannot be equal");
    for at in [first, second] {
        for offset in (64..96).step_by(8) {
            assert_eq!(
                f.guest.read_u64(at + offset),
                0xAAAA_AAAA_AAAA_AAAA,
                "arc4random_buf wrote past the {offset}th byte of a 64-byte request"
            );
        }
    }

    // **A request bigger than the host-side chunk, so the multi-chunk path is real code.** The
    // failure this catches is a chunk loop that writes every chunk at the *base* rather than at
    // the offset: with one chunk that is invisible, and with two it silently leaves the second
    // half untouched.
    let wide = f.guest.data + 0x1000;
    let wide_len = 8192u64;
    for offset in (0..wide_len).step_by(8) {
        f.guest.write_u64(wide + offset as omni_cpu::GuestAddr, 0xAAAA_AAAA_AAAA_AAAA);
    }
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, wide as u64);
        asm.mov(1, wide_len);
    });
    for (half, range) in [("first", 0u64..4096), ("second", 4096..8192)] {
        assert!(
            range
                .step_by(8)
                .any(|o| f.guest.read_u64(wide + o as omni_cpu::GuestAddr) != 0xAAAA_AAAA_AAAA_AAAA),
            "the {half} 4 KiB chunk of an 8 KiB request was left untouched"
        );
    }

    // **A destination that is writable for its first chunk and not its second is refused with
    // NOTHING written.** Without validating the whole range up front, the first 4 KiB would
    // receive real entropy and the call would then report failure — and the caller would have no
    // way to know which half it got.
    let straddle = f.guest.data + harness::DATA_BYTES as omni_cpu::GuestAddr - 5000;
    for offset in (0..4992u64).step_by(8) {
        f.guest.write_u64(straddle + offset as omni_cpu::GuestAddr, 0xAAAA_AAAA_AAAA_AAAA);
    }
    let error = refusal_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, straddle as u64);
        asm.mov(1, 8192);
    });
    assert_eq!(error.symbol(), Some("arc4random_buf"));
    for offset in (0..4992u64).step_by(8) {
        assert_eq!(
            f.guest.read_u64(straddle + offset as omni_cpu::GuestAddr),
            0xAAAA_AAAA_AAAA_AAAA,
            "a refused arc4random_buf wrote entropy into the part of the range that WAS writable"
        );
    }

    // A zero length writes nothing and is legal C at any address, null included.
    let sentinel = f.guest.read_u64(first);
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, first as u64);
        asm.mov(1, 0);
    });
    assert_eq!(f.guest.read_u64(first), sentinel, "a zero-length request must touch nothing");
    value_of(&f, "arc4random_buf", |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0);
    });
}

/// **`getenv` answers `NULL` until the host gives the guest a variable**, and the pointer it then
/// returns is stable.
///
/// Empty is a *fact* about a process that was started with no environment — the same fact the
/// `environ` data object states by pointing at a vector of one null — not a stub. The host's own
/// environment is deliberately unreachable: it would be both a wrong answer and a leak.
#[test]
fn getenv_answers_null_until_the_host_gives_the_guest_a_variable() {
    let _guard = serialized();
    let f = fixture();
    let path = f.cstring(f.guest.data + 0x100, b"PATH");
    let name = f.cstring(f.guest.data + 0x140, b"OMNI_TEST");
    let empty = f.cstring(f.guest.data + 0x180, b"");
    let malformed = f.cstring(f.guest.data + 0x1C0, b"A=B");

    // `PATH` is certainly set in the host's environment, which is exactly why it is the one asked
    // for: a `getenv` that reached the host would answer it.
    assert_eq!(
        value_of(&f, "getenv", |asm| { asm.mov(0, path as u64); }),
        0,
        "the guest must not see the host's environment"
    );
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, 0); }), 0, "getenv(NULL)");
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, empty as u64); }), 0, "an empty name");
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, malformed as u64); }), 0, "a name with `=`");

    f.bionic.set_env("OMNI_TEST", "a value").expect("the pool has room");
    let found = value_of(&f, "getenv", |asm| { asm.mov(0, name as u64); });
    assert_ne!(found, 0, "a variable the host set must be found");
    assert_eq!(f.read_cstring(found as omni_cpu::GuestAddr), b"a value");
    // `getenv`'s contract is that the pointer stays valid, so two calls give the same address.
    assert_eq!(value_of(&f, "getenv", |asm| { asm.mov(0, name as u64); }), found);

    // A name that could never be found again is refused at the setting end rather than stored.
    assert!(f.bionic.set_env("", "x").is_err());
    assert!(f.bionic.set_env("A=B", "x").is_err());
}

/// **The property table is empty until the host fills it**, and an oversized value is refused when
/// it is set rather than truncated when it is read.
#[test]
fn the_system_property_table_is_empty_until_the_host_fills_it() {
    let _guard = serialized();
    let f = fixture();
    let name = f.cstring(f.guest.data + 0x100, b"ro.build.version.sdk");
    let value = f.guest.data + 0x200;
    f.guest.write_u64(value, 0xAAAA_AAAA_AAAA_AAAA);

    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, name as u64);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 0, "an unset property is length 0");
    assert_eq!(f.read_cstring(value), b"", "and an empty string, not an untouched buffer");

    f.bionic.set_system_property("ro.build.version.sdk", "33").expect("a short value");
    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, name as u64);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 2, "the length excludes the NUL, as bionic's does");
    assert_eq!(f.read_cstring(value), b"33");

    // A null name is answered as "not set" rather than dereferenced.
    let length = value_of(&f, "__system_property_get", |asm| {
        asm.mov(0, 0);
        asm.mov(1, value as u64);
    }) as i64 as i32;
    assert_eq!(length, 0);

    // The guest declares `char value[PROP_VALUE_MAX]`, so a longer value is refused at the setting
    // end: writing it would overflow a buffer in guest code, and truncating it would be a
    // believable wrong answer.
    let too_long = "x".repeat(PROP_VALUE_MAX);
    let error = f.bionic.set_system_property("ro.too.long", &too_long).expect_err("refused");
    assert!(error.to_string().contains(&PROP_VALUE_MAX.to_string()), "{error}");
    // One byte less, with its NUL, is exactly the limit and is accepted.
    f.bionic
        .set_system_property("ro.exactly.max", &"x".repeat(PROP_VALUE_MAX - 1))
        .expect("PROP_VALUE_MAX includes the NUL");
}

/// **`getauxval(AT_HWCAP)` REFUSES until a host makes the decision, and the refusal carries both
/// measured arms.**
///
/// This is the open `AT_HWCAP` question, and the test exists so that it cannot be closed by
/// accident. Advertising `HWCAP_ATOMICS` gives 53 hard interpreter halts; declining gives 106
/// fallback arms into a global spinlock that anti-scales 21x. Neither is a default, and a policy
/// type in which "no decision" and "decided to decline" were the same value could not refuse.
#[test]
fn getauxval_refuses_at_hwcap_until_a_host_makes_the_decision() {
    let _guard = serialized();
    let f = fixture();

    // The default is the refusal, and it names both arms.
    assert_eq!(f.bionic.hwcap_policy(), HwcapPolicy::Undecided);
    for kind in [16u64 /* AT_HWCAP */, 26 /* AT_HWCAP2 */] {
        let error = refusal_of(&f, "getauxval", |asm| { asm.mov(0, kind); });
        assert_eq!(error.symbol(), Some("getauxval"));
        let text = error.to_string();
        assert!(text.contains("OPEN DECISION"), "{text}");
        assert!(text.contains("53"), "the refusal must carry the advertise arm: {text}");
        assert!(text.contains("106"), "and the decline arm: {text}");
        assert!(text.contains("21x"), "and what declining costs: {text}");
    }

    // A host that has decided says so, and then gets what it asked for — including zero, which is
    // a decision and is not the same value as having made none.
    f.bionic.set_hwcap_policy(HwcapPolicy::Decline);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 16); }), 0);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 26); }), 0);

    f.bionic.set_hwcap_policy(HwcapPolicy::Advertise { hwcap: HWCAP_ATOMICS, hwcap2: 7 });
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 16); }), HWCAP_ATOMICS);
    assert_eq!(value_of(&f, "getauxval", |asm| { asm.mov(0, 26); }), 7);
    assert_eq!(HWCAP_ATOMICS, 1 << 8, "HWCAP_ATOMICS is bit 8 of AT_HWCAP on AArch64");

    // `AT_PAGESZ` is a fact and is answered whatever the policy is.
    let page = value_of(&f, "getauxval", |asm| { asm.mov(0, 6); });
    assert_eq!(page, f.guest.space.page_size() as u64);

    // Everything else refuses by number, rather than returning `getauxval`'s documented 0/ENOENT —
    // which a guest cannot tell apart from a key whose value really is zero.
    for kind in [23u64 /* AT_SECURE */, 17 /* AT_CLKTCK */, 25 /* AT_RANDOM */, 9999] {
        let error = refusal_of(&f, "getauxval", |asm| { asm.mov(0, kind); });
        assert!(error.to_string().contains(&kind.to_string()), "{error}");
    }
}

/// **`abort`, `__stack_chk_fail` and `_exit` become typed, catchable outcomes.**
///
/// The failure this replaces is `std::process::abort()`, which no caller can contain and which
/// would take every other guest instance in the process — and this test runner — with it. A test
/// cannot assert "the host did not abort", because there would be nothing left to assert it; what
/// it can assert is the shape that makes aborting impossible, which is a value carrying the reason.
#[test]
fn abort_and_exit_become_typed_outcomes_rather_than_ending_the_host() {
    let _guard = serialized();
    let f = fixture();

    let silent = refusal_of(&f, "abort", |_| {});
    assert_eq!(silent.symbol(), Some("abort"));
    assert!(matches!(silent, AbiError::GuestAborted { .. }), "{silent:?}");
    assert!(silent.to_string().contains("set no abort message"), "{silent}");

    // The message bionic's crash reporter would have printed travels with the abort.
    let message = f.cstring(f.guest.data + 0x100, b"terminating with uncaught exception");
    value_of(&f, "android_set_abort_message", |asm| { asm.mov(0, message as u64); });
    assert_eq!(
        f.bionic.abort_message().as_deref(),
        Some("terminating with uncaught exception")
    );
    let spoken = refusal_of(&f, "abort", |_| {});
    assert!(spoken.to_string().contains("uncaught exception"), "{spoken}");

    // The stack protector is an abort with its own reason, so a reader is not left thinking the
    // guest chose to exit.
    let smashed = refusal_of(&f, "__stack_chk_fail", |_| {});
    assert_eq!(smashed.symbol(), Some("__stack_chk_fail"));
    assert!(matches!(smashed, AbiError::GuestAborted { .. }), "{smashed:?}");
    assert!(smashed.to_string().contains("canary"), "{smashed}");

    // A null message clears it.
    value_of(&f, "android_set_abort_message", |asm| { asm.mov(0, 0); });
    assert_eq!(f.bionic.abort_message(), None);

    // `_exit` carries its status, and a zero status is still an exit rather than a success.
    for status in [42i64, 0, -1] {
        let exited = refusal_of(&f, "_exit", |asm| { asm.mov(0, status as u64); });
        assert_eq!(exited.symbol(), Some("_exit"));
        match exited {
            AbiError::GuestExited { status: reported, .. } => {
                assert_eq!(i64::from(reported), status);
            }
            other => panic!("{other:?}"),
        }
    }
}

/// **The four that cannot be modelled refuse by name, with the guest's own argument in the
/// message.**
///
/// Each has a believable wrong answer sitting next to it — `sysconf` a page size, `sysinfo` a
/// zeroed struct, `prctl` a 0, `syscall` a `-1`/`ENOSYS` — and each of those would be routed
/// around by ordinary guest code without anything being reported.
#[test]
fn the_four_process_symbols_that_cannot_be_modelled_refuse_by_name() {
    let _guard = serialized();
    let f = fixture();

    // `sysconf` refuses even the two names this layer could answer, because bionic's `_SC_*`
    // numbering could not be verified here and a wrong constant answers the *wrong* query with a
    // right-looking number.
    let page = refusal_of(&f, "sysconf", |asm| { asm.mov(0, 0x27); });
    assert_eq!(page.symbol(), Some("sysconf"));
    let text = page.to_string();
    assert!(text.contains("_SC_PAGESIZE"), "{text}");
    assert!(text.contains("UNVERIFIED"), "the hint must be flagged as unverified: {text}");
    assert!(text.contains("NDK"), "and must say what would settle it: {text}");
    let unknown = refusal_of(&f, "sysconf", |asm| { asm.mov(0, 4242); });
    assert!(unknown.to_string().contains("4242"), "{unknown}");

    let info = refusal_of(&f, "sysinfo", |asm| {
        asm.mov(0, (f.guest.data + 0x200) as u64);
    });
    assert_eq!(info.symbol(), Some("sysinfo"));
    assert!(info.to_string().contains("totalram"), "{info}");

    let named = refusal_of(&f, "prctl", |asm| {
        asm.mov(0, 15); // PR_SET_NAME
        asm.mov(1, (f.guest.data + 0x100) as u64);
    });
    assert_eq!(named.symbol(), Some("prctl"));
    assert!(named.to_string().contains("PR_SET_NAME"), "{named}");
    let vma = refusal_of(&f, "prctl", |asm| { asm.mov(0, 0x5356_4d41); });
    assert!(vma.to_string().contains("PR_SET_VMA"), "{vma}");

    let tid = refusal_of(&f, "syscall", |asm| { asm.mov(0, 178); });
    assert_eq!(tid.symbol(), Some("syscall"));
    let text = tid.to_string();
    assert!(text.contains("gettid"), "{text}");
    assert!(text.contains("ENOSYS"), "the refusal must say why -1/ENOSYS was rejected: {text}");
    let nameless = refusal_of(&f, "syscall", |asm| { asm.mov(0, 100_000); });
    assert!(nameless.to_string().contains("100000"), "{nameless}");
}

// ------------------------------------------------------------------ the log sink

/// **`__android_log_print` runs the real `printf` engine**, and the record keeps the priority and
/// the tag the guest gave.
///
/// A line reading `%s at %p` with the arguments dropped would be worse than no line, so the
/// formatting is the same engine `snprintf` uses rather than a passthrough of the format string.
#[test]
fn android_log_print_formats_through_the_real_printf_engine() {
    let _guard = serialized();
    let f = fixture();
    let tag = f.cstring(f.guest.data + 0x100, b"Roblox");
    let fmt = f.cstring(f.guest.data + 0x140, b"n=%d s=%s");
    let text = f.cstring(f.guest.data + 0x180, b"hi");

    let written = value_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 4); // ANDROID_LOG_INFO
        asm.mov(1, tag as u64);
        asm.mov(2, fmt as u64);
        asm.mov(3, 7);
        asm.mov(4, text as u64);
    }) as i64 as i32;

    let records = f.bionic.log_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].priority, LogPriority::Info);
    assert_eq!(records[0].tag, "Roblox");
    assert_eq!(records[0].message, "n=7 s=hi");
    assert_eq!(written, records[0].message.len() as i32, "the return is the message's length");

    // A null tag is an empty tag, not a fault.
    value_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 6); // ANDROID_LOG_ERROR
        asm.mov(1, 0);
        asm.mov(2, text as u64);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].tag, "");
    assert_eq!(records[1].priority, LogPriority::Error);

    // A priority outside the scale is a refusal, not a mapping to its nearest neighbour.
    let error = refusal_of(&f, "__android_log_print", |asm| {
        asm.mov(0, 42);
        asm.mov(1, tag as u64);
        asm.mov(2, text as u64);
    });
    assert_eq!(error.symbol(), Some("__android_log_print"));
    assert!(error.to_string().contains("42"), "{error}");
    assert_eq!(f.bionic.log_records().len(), 2, "a refused line must not be recorded");
}

/// `syslog` takes its tag from `openlog`, keeps the facility, and `closelog` clears it.
#[test]
fn syslog_takes_its_tag_from_openlog_and_closelog_clears_it() {
    let _guard = serialized();
    let f = fixture();
    let ident = f.cstring(f.guest.data + 0x100, b"omnidroid");
    let fmt = f.cstring(f.guest.data + 0x140, b"x=%d");

    // LOG_USER (1 << 3) | LOG_WARNING (4).
    value_of(&f, "openlog", |asm| {
        asm.mov(0, ident as u64);
        asm.mov(1, 0x01); // LOG_PID, read and not acted on
        asm.mov(2, 8); // LOG_USER
    });
    assert_eq!(f.bionic.syslog_ident().as_deref(), Some("omnidroid"));
    value_of(&f, "syslog", |asm| {
        asm.mov(0, 8 | 4);
        asm.mov(1, fmt as u64);
        asm.mov(2, 5);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].priority, LogPriority::Warn, "LOG_WARNING maps to WARN");
    assert_eq!(records[0].tag, "omnidroid[facility 8]", "the facility is carried, not dropped");
    assert_eq!(records[0].message, "x=5");

    value_of(&f, "closelog", |_| {});
    assert_eq!(f.bionic.syslog_ident(), None);
    value_of(&f, "syslog", |asm| {
        asm.mov(0, 3); // LOG_ERR, facility 0
        asm.mov(1, fmt as u64);
        asm.mov(2, 9);
    });
    let records = f.bionic.log_records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[1].priority, LogPriority::Error);
    assert_eq!(records[1].tag, "syslog", "with no ident and no facility, the tag names the call");
    assert_eq!(records[1].message, "x=9");
}

/// **The capture ring is bounded, and it says how much it dropped.**
///
/// How much the engine logs during initialisation has not been measured, so an unbounded ring is a
/// host allocation a guest can drive in a loop. The assertion is on *membership* rather than only
/// on the count: each line carries its own number, so the surviving window is checked at both ends
/// — a ring that dropped the newest records instead of the oldest would keep exactly the same
/// number of them.
///
/// Driven from a guest loop rather than 266 separate runs, so it is one translation and 266 real
/// thunk crossings.
#[test]
fn the_log_capture_ring_is_bounded_and_reports_what_it_dropped() {
    let _guard = serialized();
    let f = fixture();
    let rounds: u64 = LOG_CAPTURE_MAX as u64 + 10;
    let tag = f.cstring(f.guest.data + 0x100, b"loop");
    let fmt = f.cstring(f.guest.data + 0x140, b"n=%d");
    let thunk = f.thunk("__android_log_print");

    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    // X22 is callee-saved, so the handler cannot disturb it.
    asm.mov(22, rounds);
    let loop_start = asm.pc();
    asm.mov(0, 4); // ANDROID_LOG_INFO
    asm.mov(1, tag as u64);
    asm.mov(2, fmt as u64);
    asm.push(mov_reg(3, 22)); // the variadic `%d`
    asm.bl(thunk);
    asm.push(subs_imm(22, 22, 1));
    let here = asm.pc();
    let back = ((loop_start as i64 - here as i64) / 4) as i32;
    asm.push(b_cond(1 /* NE */, back));
    asm.push(ret(21));
    f.guest.load(asm.words());

    let mut cpu = f.guest.thread(&f.boundary);
    let exit = f.run(&mut cpu, entry).expect("the loop must complete");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");

    let records = f.bionic.log_records();
    // Pinned as a number as well as as a symbol: a test written only against the constant would
    // pass for any cap at all, including one too small to hold a run's worth of lines.
    assert_eq!(LOG_CAPTURE_MAX, 256);
    assert_eq!(records.len(), LOG_CAPTURE_MAX, "the ring is bounded");
    assert_eq!(f.bionic.log_dropped(), rounds - LOG_CAPTURE_MAX as u64, "and says what it dropped");
    // The loop counts down, so the *last* record is n=1 and the oldest survivor is n=256.
    assert_eq!(records[0].message, format!("n={LOG_CAPTURE_MAX}"), "the oldest survivor");
    assert_eq!(records[LOG_CAPTURE_MAX - 1].message, "n=1", "the newest");
}

// ------------------------------------------------------------------ hostile arguments

/// **Every pointer and length in the phase 3a group, hostile**, and none of them panics.
///
/// A panic or an abort reachable from guest-supplied arguments is Critical. Each case here either
/// completes with a defined `-1`/`0`/`NULL` or refuses by name; nothing else is acceptable, and in
/// particular nothing may report success having written somewhere it should not.
#[test]
fn hostile_arguments_to_the_clock_and_process_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture();
    let wild = f.guest.unmapped as u64;
    // A `struct timespec` asking for zero time, so `nanosleep`'s hostile cases test the *pointer*
    // rather than spending a minute asleep.
    let zero_req = f.guest.data + 0x300;
    f.guest.write_u64(zero_req, 0);
    f.guest.write_u64(zero_req + 8, 0);

    let cases: &[(&str, &[u64])] = &[
        ("clock_gettime", &[1, 0]),
        ("clock_gettime", &[1, wild]),
        ("clock_gettime", &[1, u64::MAX]),
        ("clock_gettime", &[1, u64::MAX - 4]),
        ("gettimeofday", &[wild, 0]),
        ("gettimeofday", &[0, wild]),
        ("gettimeofday", &[u64::MAX, u64::MAX]),
        ("gmtime_r", &[0, 0]),
        ("gmtime_r", &[wild, wild]),
        ("gmtime_r", &[u64::MAX, u64::MAX]),
        ("nanosleep", &[0, 0]),
        ("nanosleep", &[wild, 0]),
        ("nanosleep", &[u64::MAX, u64::MAX]),
        ("nanosleep", &[zero_req as u64, wild]),
        ("arc4random_buf", &[0, 64]),
        ("arc4random_buf", &[wild, 64]),
        ("arc4random_buf", &[wild, u64::MAX]),
        ("arc4random_buf", &[u64::MAX, u64::MAX]),
        ("getenv", &[wild]),
        ("getenv", &[u64::MAX]),
        ("__system_property_get", &[wild, wild]),
        ("__system_property_get", &[0, 0]),
        ("__system_property_get", &[0, u64::MAX]),
        ("android_set_abort_message", &[wild]),
        ("android_set_abort_message", &[u64::MAX]),
        ("getauxval", &[u64::MAX]),
        ("sysconf", &[u64::MAX]),
        ("sysinfo", &[wild]),
        ("prctl", &[u64::MAX, u64::MAX, u64::MAX]),
        ("syscall", &[u64::MAX, u64::MAX]),
        ("__android_log_print", &[4, wild, wild]),
        ("__android_log_print", &[4, 0, 0]),
        ("__android_log_print", &[u64::MAX, 0, 0]),
        ("syslog", &[0, 0]),
        ("syslog", &[0, wild]),
        ("openlog", &[wild, 0, 0]),
        ("openlog", &[u64::MAX, 0, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert!(
                    code == 0 || code == -1,
                    "`{symbol}` {args:x?} completed with {code}, which is neither a success nor \
                     a defined failure"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
}

/// **Hostile arguments to every symbol phase 3c binds**, including the ones that create threads.
///
/// A guest that creates threads is a new hostile surface, and the shapes are its own: a null or
/// wild `pthread_t *`, an attribute object that is not readable, a `pthread_t` nobody handed out,
/// and a `sigset_t` that is not writable. Every one of them has to be a typed error naming the
/// symbol or a defined return, never a host access violation and never a panic.
///
/// **No thread host here**, deliberately: this fixture cannot create a thread at all, so every
/// `pthread_create` case exercises the argument handling and the refusal rather than spawning 30
/// host threads with wild arguments. The cases that need a live thread are the tests above.
#[test]
fn hostile_arguments_to_the_thread_and_signal_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let f = fixture();
    let wild = f.guest.unmapped as u64;
    let readonly = f.guest.readonly as u64;

    let cases: &[(&str, &[u64])] = &[
        // `pthread_create(thread, attr, start, arg)`
        ("pthread_create", &[0, 0, 1, 0]),
        ("pthread_create", &[wild, 0, 1, 0]),
        ("pthread_create", &[u64::MAX, 0, 1, 0]),
        ("pthread_create", &[readonly, 0, 1, 0]),
        ("pthread_create", &[0, 0, 0, 0]),
        ("pthread_create", &[wild, wild, wild, wild]),
        ("pthread_create", &[u64::MAX, u64::MAX, u64::MAX, u64::MAX]),
        // `pthread_join(thread, retval)`
        ("pthread_join", &[0, 0]),
        ("pthread_join", &[u64::MAX, 0]),
        ("pthread_join", &[0, wild]),
        ("pthread_join", &[u64::MAX, u64::MAX]),
        ("pthread_join", &[0xDEAD_BEEF, readonly]),
        // `pthread_detach(thread)`
        ("pthread_detach", &[0]),
        ("pthread_detach", &[u64::MAX]),
        // `pthread_getschedparam(thread, policy, param)`
        ("pthread_getschedparam", &[0, 0, 0]),
        ("pthread_getschedparam", &[u64::MAX, wild, wild]),
        ("pthread_getschedparam", &[0xDEAD_BEEF, u64::MAX, u64::MAX]),
        // the signal family
        ("sigfillset", &[0]),
        ("sigfillset", &[wild]),
        ("sigfillset", &[u64::MAX]),
        ("sigfillset", &[u64::MAX - 4]),
        ("sigfillset", &[readonly]),
        ("sigaction", &[0, 0, 0]),
        ("sigaction", &[u64::MAX, wild, wild]),
        ("raise", &[0]),
        ("raise", &[u64::MAX]),
        ("pthread_sigmask", &[u64::MAX, wild, wild]),
        ("pthread_sigmask", &[0, 0, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let code = f.guest.read_u64(f.guest.data) as i64 as i32;
                // The pthread functions return their error as the value, so the defined set here
                // is wider than the errno group's: 0, -1, or one of the four POSIX numbers this
                // group answers with. Anything else is a number nobody decided on.
                assert!(
                    code == 0 || code == -1 || [3, 11, 22, 35].contains(&code),
                    "`{symbol}` {args:x?} completed with {code}, which is neither a success nor \
                     a defined failure"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
    // Nothing above may have created a thread: the fixture has no thread host at all.
    assert_eq!(f.bionic.guest_thread_records(), 0);
    assert!(f.bionic.guest_thread_failures().is_empty());
}

// =================================================================== phase 3b: files

/// A host directory that removes itself, for the guest's filesystem root.
///
/// Built without a dependency, and with the process id and thread id in its name so that two
/// tests -- and two `cargo test` processes -- cannot collide on it.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!(
            "omni-bionic-fs-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fixture whose instance has a filesystem rooted at a fresh scratch directory.
///
/// Built on `fixture_with`, which also places the eighteen data objects — because `__sF` has to
/// exist before `stdin`, `stdout` and `stderr` can be streams over descriptors 0, 1 and 2.
fn rooted(tag: &str) -> (Fixture, Scratch) {
    let scratch = Scratch::new(tag);
    let f = fixture_with(&[]);
    f.bionic.set_filesystem_root(&scratch.0).expect("a filesystem root");
    (f, scratch)
}

/// Read `len` bytes out of guest memory.
fn read_guest(f: &Fixture, at: omni_cpu::GuestAddr, len: usize) -> Vec<u8> {
    (0..len)
        .map(|offset| {
            let address = at + offset;
            (f.guest.read_u64(address & !7) >> (8 * (address & 7))) as u8
        })
        .collect()
}

/// A little-endian `u32` read out of guest memory.
fn read_u32_guest(f: &Fixture, at: omni_cpu::GuestAddr) -> u32 {
    u32::from_le_bytes(read_guest(f, at, 4).try_into().expect("four bytes"))
}

/// A little-endian `u64` read out of guest memory at an arbitrary alignment.
fn read_u64_guest(f: &Fixture, at: omni_cpu::GuestAddr) -> u64 {
    u64::from_le_bytes(read_guest(f, at, 8).try_into().expect("eight bytes"))
}

// The guest's own `O_*`, spelled again in the test so that the constants in `files.rs` are
// compared against a second copy rather than against themselves.
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_DIRECTORY: u64 = 0o200000;

/// **An instance with no filesystem root refuses every path call, by name.**
///
/// The default is the confinement property: there is no root until the embedding names one, so
/// there is no way for a guest to reach a host file by accident. A refusal rather than `ENOENT`,
/// because "this runtime was not configured" and "that file is not there" are different problems
/// and only one of them is the guest's.
#[test]
fn a_guest_with_no_filesystem_root_refuses_every_path_call_by_name() {
    let _guard = serialized();
    let f = fixture();
    let path = f.cstring(f.guest.data + 0x100, b"/data/anything");
    for symbol in ["open", "stat", "lstat", "access", "unlink", "rmdir", "opendir", "statvfs"] {
        let error = refusal_of(&f, symbol, |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, 0);
        });
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        let text = error.to_string();
        assert!(
            text.contains("set_filesystem_root"),
            "`{symbol}` must name the method that would supply a root: {text}"
        );
    }
}

/// A file created, written and read back **through real translated guest code**.
///
/// Five calls, each its own guest program: `open`, `__write_chk`, `close`, `open`, `read`. The
/// assertion is on the *bytes*, read out of guest memory afterwards, so a handler that returned a
/// plausible count without moving anything fails it.
#[test]
fn a_file_is_created_written_and_read_back_through_real_guest_code() {
    let _guard = serialized();
    let (f, scratch) = rooted("roundtrip");
    let path = f.cstring(f.guest.data + 0x100, b"/hello.txt");
    let payload = b"the quick brown fox jumps over the lazy dog";
    let source = f.guest.data + 0x200;
    f.guest.write_bytes(source, payload);

    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_WRONLY | O_CREAT | O_TRUNC);
        asm.mov(2, 0o644);
    }) as i64;
    assert!(fd >= 3, "open returned {fd}");

    let written = value_of(&f, "__write_chk", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, source as u64);
        asm.mov(2, payload.len() as u64);
        asm.mov(3, payload.len() as u64);
    }) as i64;
    assert_eq!(written, payload.len() as i64);

    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
    // The host really has the file, with the bytes the guest wrote.
    assert_eq!(std::fs::read(scratch.path("hello.txt")).expect("the host file"), payload);

    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    assert_eq!(fd, 3, "the lowest free descriptor comes back after the close");
    let destination = f.guest.data + 0x400;
    let read = value_of(&f, "read", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, destination as u64);
        asm.mov(2, 256);
    }) as i64;
    assert_eq!(read, payload.len() as i64);
    assert_eq!(read_guest(&f, destination, payload.len()), payload);
    // A second read is end of file, which is zero and not an error.
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, destination as u64);
            asm.mov(2, 256);
        }) as i64,
        0
    );
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
}

/// **The confinement property, through the boundary, against a bait file.**
///
/// A file is created *outside* the root and the guest is given every shape of path that would
/// reach it. The assertion afterwards is that the bait is still there and was never read: a
/// traversal that worked would show up as content rather than as a missing refusal.
#[test]
fn no_guest_path_can_reach_a_file_outside_the_instance_root() {
    let _guard = serialized();
    let outer = Scratch::new("outer");
    std::fs::write(outer.path("secret.txt"), b"HOST SECRET").expect("the bait");
    let root = outer.path("root");
    std::fs::create_dir_all(&root).expect("the guest root");
    let f = fixture();
    f.bionic.set_filesystem_root(&root).expect("a filesystem root");

    let attempts: &[&[u8]] = &[
        b"/../secret.txt",
        b"/../../secret.txt",
        b"../secret.txt",
        b"/a/../../secret.txt",
        b"/./../secret.txt",
        b"//../secret.txt",
        br"/..\secret.txt",
        b"/a/b/c/../../../../secret.txt",
        b"/NUL",
        b"/C:secret.txt",
        b"/secret.txt.",
    ];
    for (index, attempt) in attempts.iter().enumerate() {
        let path = f.cstring(f.guest.data + 0x100 + index * 0x40, attempt);
        let entry = call_one(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY);
            asm.mov(2, 0);
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            // Refused outright: a traversal, a device name, an aliasing name.
            Err(error) => assert_eq!(error.symbol(), Some("open"), "{error:?}"),
            // Or absorbed: `/.. == /`, so the path named something inside the root that is not
            // there. `-1` is the answer, never a descriptor.
            Ok(_) => {
                let fd = f.guest.read_u64(f.guest.data) as i64 as i32;
                assert_eq!(
                    fd,
                    -1,
                    "`{}` opened descriptor {fd}",
                    String::from_utf8_lossy(attempt)
                );
            }
        }
    }
    assert_eq!(
        std::fs::read(outer.path("secret.txt")).expect("the bait survives"),
        b"HOST SECRET",
        "the bait was modified"
    );
}

/// **`struct stat`'s fields land where the guest reads them.**
///
/// The layout is ASSUMED — derived from Linux UAPI, with no NDK here to check it against — so the
/// test asserts against a file whose length this test chose. A layout that moved `st_size` would
/// report a number that is not 1,234.
#[test]
fn stat_lands_its_fields_where_the_guest_reads_them() {
    let _guard = serialized();
    let (f, scratch) = rooted("stat");
    std::fs::write(scratch.path("f"), vec![7u8; 1234]).expect("a file of known length");
    std::fs::create_dir(scratch.path("d")).expect("a directory");
    let path = f.cstring(f.guest.data + 0x100, b"/f");
    let buf = f.guest.data + 0x400;

    assert_eq!(
        value_of(&f, "stat", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    let mode = read_u32_guest(&f, buf + 16);
    assert_eq!(mode & omni_android::bionic::S_IFMT, omni_android::bionic::S_IFREG, "st_mode");
    assert_ne!(mode & 0o200, 0, "a writable file must report S_IWUSR");
    assert_eq!(read_u32_guest(&f, buf + 20), 1, "st_nlink");
    assert_eq!(read_u64_guest(&f, buf + 48), 1234, "st_size");
    assert_eq!(read_u32_guest(&f, buf + 56), 4096, "st_blksize is the size this layer transfers in");
    assert_eq!(read_u64_guest(&f, buf + 64), 1234u64.div_ceil(512), "st_blocks");
    assert_ne!(read_u64_guest(&f, buf + 8), 0, "st_ino must never be zero");
    assert_ne!(read_u64_guest(&f, buf), 0, "st_dev must never be zero");
    let file_ino = read_u64_guest(&f, buf + 8);

    // A directory, through `lstat`, which is the same encoder on a different call.
    let dir = f.cstring(f.guest.data + 0x140, b"/d");
    assert_eq!(
        value_of(&f, "lstat", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    let mode = read_u32_guest(&f, buf + 16);
    assert_eq!(mode & omni_android::bionic::S_IFMT, omni_android::bionic::S_IFDIR);
    assert_ne!(read_u64_guest(&f, buf + 8), file_ino, "two paths must not share an inode");

    // And `fstat` on an open descriptor agrees with `stat` on its path.
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    assert_eq!(
        value_of(&f, "fstat", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        0
    );
    assert_eq!(read_u64_guest(&f, buf + 48), 1234, "fstat's st_size");
    assert_eq!(read_u64_guest(&f, buf + 8), file_ino, "one file, one inode");
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);

    // A path that is not there is `-1`, not a zeroed structure reported as a success.
    let missing = f.cstring(f.guest.data + 0x180, b"/nope");
    assert_eq!(
        value_of(&f, "stat", |asm| {
            asm.mov(0, missing as u64);
            asm.mov(1, buf as u64);
        }) as i64,
        -1
    );
}

/// `statvfs` fills the guest's structure with the host volume's own numbers.
///
/// **Structural rather than numeric**: the relations that must hold for any volume, because a
/// test that pinned a free-space figure would be asserting about this machine's disk. A fabricated
/// filesystem has no reason to satisfy them.
#[test]
fn statvfs_fills_the_guests_structure_with_the_hosts_numbers() {
    let _guard = serialized();
    let (f, _scratch) = rooted("statvfs");
    let path = f.cstring(f.guest.data + 0x100, b"/");
    let buf = f.guest.data + 0x400;
    let entry = call_one(&f, "statvfs", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, buf as u64);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => {
            // The only acceptable failure is the structural refusal on a target with no backend.
            assert!(error.to_string().contains("statvfs(3)"), "{error}");
            return;
        }
        Ok(_) => assert_eq!(f.guest.read_u64(f.guest.data) as i64, 0),
    }
    let field = |index: usize| read_u64_guest(&f, buf + index * 8);
    assert!(field(0) > 0 && field(0).is_power_of_two(), "f_bsize is {}", field(0));
    assert_eq!(field(1), field(0), "f_frsize is the allocation unit too on this host");
    assert!(field(2) > 0, "f_blocks");
    assert!(field(3) <= field(2), "f_bfree > f_blocks");
    assert!(field(4) <= field(3), "f_bavail > f_bfree");
    assert_eq!((field(5), field(6), field(7)), (0, 0, 0), "the three inode counts");
    assert!(field(10) > 0, "f_namemax");
}

/// A directory walk: `opendir`, `readdir` to the end, `closedir`.
///
/// `.` and `..` come first and every entry appears exactly once. The `DIR *` and the returned
/// `struct dirent *` are the same address, which is what makes the "valid until the next call"
/// contract true by construction.
#[test]
fn a_directory_walk_returns_dot_dotdot_and_every_entry_once() {
    let _guard = serialized();
    let (f, scratch) = rooted("dir");
    std::fs::create_dir(scratch.path("d")).expect("a directory");
    for name in ["alpha", "beta"] {
        std::fs::write(scratch.path(&format!("d/{name}")), b"x").expect("a file");
    }
    std::fs::create_dir(scratch.path("d/sub")).expect("a subdirectory");
    let path = f.cstring(f.guest.data + 0x100, b"/d");

    let dirp = value_of(&f, "opendir", |asm| { asm.mov(0, path as u64); });
    assert_ne!(dirp, 0, "opendir returned NULL");
    assert_eq!(f.bionic.open_dirs(), 1);

    let mut names = Vec::new();
    for _ in 0..8 {
        let entry = value_of(&f, "readdir", |asm| { asm.mov(0, dirp); });
        if entry == 0 {
            break;
        }
        assert_eq!(entry, dirp, "readdir must return the DIR's own dirent slot");
        let ino = read_u64_guest(&f, entry as omni_cpu::GuestAddr);
        assert_ne!(ino, 0, "d_ino must never be zero");
        let reclen = u16::from_le_bytes(
            read_guest(&f, entry as omni_cpu::GuestAddr + 16, 2).try_into().unwrap(),
        );
        assert_eq!(reclen, 280, "d_reclen");
        let kind = read_guest(&f, entry as omni_cpu::GuestAddr + 18, 1)[0];
        let name = f.read_cstring(entry as omni_cpu::GuestAddr + 19);
        names.push((String::from_utf8(name).expect("a UTF-8 name"), kind));
    }
    assert_eq!(names[0], (".".to_string(), 4), "DT_DIR");
    assert_eq!(names[1], ("..".to_string(), 4));
    let mut rest: Vec<_> = names[2..].to_vec();
    rest.sort();
    assert_eq!(
        rest,
        vec![
            ("alpha".to_string(), 8u8),
            ("beta".to_string(), 8),
            ("sub".to_string(), 4),
        ],
        "DT_REG is 8 and DT_DIR is 4"
    );
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, dirp); }) as i64, 0);
    assert_eq!(f.bionic.open_dirs(), 0);
    // A `DIR *` that has been closed is `EBADF`, not a second walk of the same directory.
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, dirp); }) as i64, -1);
}

/// **The three standard streams are streams**, with the descriptors POSIX reserves.
///
/// `stdin`, `stdout` and `stderr` are data objects pointing into `__sF`, and phase 3b registers
/// each of the three as a stream over descriptor 0, 1 and 2. `fileno` answering 1 for `stdout` is
/// what proves the registration, the `FILE_BYTES` spacing and the declaration order agree.
#[test]
fn the_three_standard_streams_are_streams_over_the_descriptors_posix_reserves() {
    let _guard = serialized();
    let (f, _scratch) = rooted("std");
    let sf = f
        .boundary
        .slot_named("__sF")
        .expect("__sF is declared")
        .address;
    for (index, expected) in [(0usize, 0i64), (1, 1), (2, 2)] {
        let stream = sf + index * omni_android::bionic::FILE_BYTES;
        assert_eq!(
            value_of(&f, "fileno", |asm| { asm.mov(0, stream as u64); }) as i64,
            expected,
            "`__sF[{index}]` must be descriptor {expected}"
        );
        assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream as u64); }) as i64, 0);
        // `fflush` on a standard stream reaches the host's own buffered writer.
        assert_eq!(value_of(&f, "fflush", |asm| { asm.mov(0, stream as u64); }) as i64, 0);
    }
    // `fflush(NULL)` walks every stream and succeeds.
    assert_eq!(value_of(&f, "fflush", |asm| { asm.mov(0, 0); }) as i64, 0);
    // And the `stdout` data object really points at `__sF[1]`.
    let stdout_cell = f.boundary.slot_named("stdout").expect("stdout is declared").address;
    assert_eq!(
        f.guest.read_u64(stdout_cell),
        (sf + omni_android::bionic::FILE_BYTES) as u64,
        "stdout must point at __sF[1]"
    );
}

/// A `FILE *` round trip: `fopen`, `fputs`, `fclose`, `fopen`, `fgets`, `feof`, `fclose`.
#[test]
fn a_file_stream_round_trips_through_fopen_fputs_fgets_and_fclose() {
    let _guard = serialized();
    let (f, scratch) = rooted("stream");
    let path = f.cstring(f.guest.data + 0x100, b"/lines.txt");
    let write_mode = f.cstring(f.guest.data + 0x140, b"w");
    let read_mode = f.cstring(f.guest.data + 0x160, b"r");
    let line = f.cstring(f.guest.data + 0x200, b"first line\nsecond line\n");

    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, write_mode as u64);
    });
    assert_ne!(stream, 0, "fopen returned NULL");
    assert_eq!(f.bionic.open_streams(), 4, "the three standard streams plus this one");
    assert_eq!(
        value_of(&f, "fputs", |asm| {
            asm.mov(0, line as u64);
            asm.mov(1, stream);
        }) as i64,
        23,
        "fputs returns the byte count bionic returns, and the two lines are 11 + 12 bytes"
    );
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
    assert_eq!(f.bionic.open_streams(), 3, "the slot is released");
    assert_eq!(
        std::fs::read(scratch.path("lines.txt")).expect("the host file"),
        b"first line\nsecond line\n"
    );

    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, read_mode as u64);
    });
    let buf = f.guest.data + 0x400;
    let got = value_of(&f, "fgets", |asm| {
        asm.mov(0, buf as u64);
        asm.mov(1, 64);
        asm.mov(2, stream);
    });
    assert_eq!(got, buf as u64, "fgets returns its buffer");
    assert_eq!(f.read_cstring(buf), b"first line\n", "the newline is kept");
    let got = value_of(&f, "fgets", |asm| {
        asm.mov(0, buf as u64);
        asm.mov(1, 64);
        asm.mov(2, stream);
    });
    assert_eq!(got, buf as u64);
    assert_eq!(f.read_cstring(buf), b"second line\n", "and the second line is intact");
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 0, "not at the end yet");
    assert_eq!(
        value_of(&f, "fgets", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 64);
            asm.mov(2, stream);
        }),
        0,
        "the third call is end of file: NULL"
    );
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 1, "and feof sticks");
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
}

/// `fread` and `fwrite` report whole items, and `fdopen` puts a stream over a descriptor.
#[test]
fn fread_and_fwrite_report_whole_items_over_a_descriptor_fdopen_adopted() {
    let _guard = serialized();
    let (f, scratch) = rooted("items");
    std::fs::write(scratch.path("b"), b"0123456789").expect("ten bytes");
    let path = f.cstring(f.guest.data + 0x100, b"/b");
    let mode = f.cstring(f.guest.data + 0x140, b"rb");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    let stream = value_of(&f, "fdopen", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, mode as u64);
    });
    assert_ne!(stream, 0, "fdopen returned NULL");
    assert_eq!(value_of(&f, "fileno", |asm| { asm.mov(0, stream); }) as i64, fd);

    let buf = f.guest.data + 0x400;
    // Three items of three bytes: nine bytes, three items.
    assert_eq!(
        value_of(&f, "fread", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 3);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        3
    );
    assert_eq!(read_guest(&f, buf, 9), b"012345678");
    // One byte left: no complete item, and end of file.
    assert_eq!(
        value_of(&f, "fread", |asm| {
            asm.mov(0, buf as u64);
            asm.mov(1, 3);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        0
    );
    assert_eq!(value_of(&f, "feof", |asm| { asm.mov(0, stream); }) as i64, 1);
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);

    // And `fwrite` on the way out, with `fputc` after it.
    let out = f.cstring(f.guest.data + 0x180, b"/out");
    let wmode = f.cstring(f.guest.data + 0x1c0, b"wb");
    let source = f.guest.data + 0x600;
    f.guest.write_bytes(source, &[1u8, 2, 3, 4, 5, 6]);
    let stream = value_of(&f, "fopen", |asm| {
        asm.mov(0, out as u64);
        asm.mov(1, wmode as u64);
    });
    assert_eq!(
        value_of(&f, "fwrite", |asm| {
            asm.mov(0, source as u64);
            asm.mov(1, 2);
            asm.mov(2, 3);
            asm.mov(3, stream);
        }),
        3,
        "three items of two bytes"
    );
    // `fputc` returns the byte as an unsigned char: 0xff is 255, never EOF.
    assert_eq!(
        value_of(&f, "fputc", |asm| {
            asm.mov(0, 0xffff_ffff_ffff_ffff);
            asm.mov(1, stream);
        }) as i64,
        255
    );
    assert_eq!(value_of(&f, "fclose", |asm| { asm.mov(0, stream); }) as i64, 0);
    assert_eq!(
        std::fs::read(scratch.path("out")).expect("the host file"),
        &[1u8, 2, 3, 4, 5, 6, 0xff]
    );
}

/// `pread` reads at an offset and leaves the descriptor's own offset alone.
///
/// **The regression this pins was found by the seam's own test and is worth having from the guest
/// side too**: `FileExt::seek_read` on Windows moves the file pointer, so a `pread` built on it
/// alone makes the next sequential `read` return end of file.
#[test]
fn pread_leaves_the_descriptors_own_offset_alone_for_the_guest_too() {
    let _guard = serialized();
    let (f, scratch) = rooted("pread");
    std::fs::write(scratch.path("p"), b"0123456789").expect("ten bytes");
    let path = f.cstring(f.guest.data + 0x100, b"/p");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    }) as i64;
    let buf = f.guest.data + 0x400;
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
            asm.mov(2, 4);
        }) as i64,
        4
    );
    assert_eq!(read_guest(&f, buf, 4), b"0123");
    let entry = call_one(&f, "pread", |asm| {
        asm.mov(0, fd as u64);
        asm.mov(1, (buf + 0x100) as u64);
        asm.mov(2, 3);
        asm.mov(3, 7);
    });
    let mut cpu = f.guest.thread(&f.boundary);
    match f.run(&mut cpu, entry) {
        Err(error) => {
            assert!(error.to_string().contains("pread(2)"), "{error}");
            return;
        }
        Ok(_) => assert_eq!(f.guest.read_u64(f.guest.data) as i64, 3),
    }
    assert_eq!(read_guest(&f, buf + 0x100, 3), b"789");
    // The sequential offset is untouched: the next read continues from 4.
    assert_eq!(
        value_of(&f, "read", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, (buf + 0x200) as u64);
            asm.mov(2, 3);
        }) as i64,
        3
    );
    assert_eq!(read_guest(&f, buf + 0x200, 3), b"456", "pread moved the descriptor's offset");
    // A negative offset is EINVAL rather than a wrap into a huge one.
    assert_eq!(
        value_of(&f, "pread", |asm| {
            asm.mov(0, fd as u64);
            asm.mov(1, buf as u64);
            asm.mov(2, 3);
            asm.mov(3, 0xffff_ffff_ffff_ffff);
        }) as i64,
        -1
    );
    assert_eq!(value_of(&f, "close", |asm| { asm.mov(0, fd as u64); }) as i64, 0);
}

/// The namespace calls keep POSIX's own distinctions, from real guest code.
#[test]
fn the_namespace_calls_keep_posixs_distinctions_from_guest_code() {
    let _guard = serialized();
    let (f, scratch) = rooted("namespace");
    let dir = f.cstring(f.guest.data + 0x100, b"/d");
    let file = f.cstring(f.guest.data + 0x140, b"/d/f");
    let moved = f.cstring(f.guest.data + 0x180, b"/moved");

    assert_eq!(
        value_of(&f, "mkdir", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, 0o755);
        }) as i64,
        0
    );
    assert!(scratch.path("d").is_dir(), "the host directory exists");
    assert_eq!(
        value_of(&f, "mkdir", |asm| {
            asm.mov(0, dir as u64);
            asm.mov(1, 0o755);
        }) as i64,
        -1,
        "a second mkdir is EEXIST"
    );
    std::fs::write(scratch.path("d/f"), b"body").expect("a file in it");
    // `unlink` refuses a directory and `rmdir` refuses a file, which `std::fs` does not do for us.
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, dir as u64); }) as i64, -1);
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, file as u64); }) as i64, -1);
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, dir as u64); }) as i64, -1, "not empty");
    assert_eq!(
        value_of(&f, "rename", |asm| {
            asm.mov(0, file as u64);
            asm.mov(1, moved as u64);
        }) as i64,
        0
    );
    assert!(scratch.path("moved").is_file());
    assert_eq!(value_of(&f, "rmdir", |asm| { asm.mov(0, dir as u64); }) as i64, 0, "now empty");
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, moved as u64); }) as i64, 0);
    assert_eq!(value_of(&f, "unlink", |asm| { asm.mov(0, moved as u64); }) as i64, -1);

    // `access`: F_OK, R_OK and W_OK are answered; X_OK is refused by name.
    std::fs::write(scratch.path("a"), b"x").expect("a file");
    let probe = f.cstring(f.guest.data + 0x1c0, b"/a");
    for mode in [0u64, 4, 2, 6] {
        assert_eq!(
            value_of(&f, "access", |asm| {
                asm.mov(0, probe as u64);
                asm.mov(1, mode);
            }) as i64,
            0,
            "access(path, {mode})"
        );
    }
    let error = refusal_of(&f, "access", |asm| {
        asm.mov(0, probe as u64);
        asm.mov(1, 1);
    });
    assert_eq!(error.symbol(), Some("access"), "{error:?}");
    assert!(error.to_string().contains("X_OK"), "{error}");
}

/// **`__open_2` refuses `O_CREAT` by name, and the refused `open` flags refuse by name.**
///
/// Each of these is a promise this layer cannot keep, and each has a believable wrong answer
/// available — accept the flag and do nothing — which is the shape `mlock` was refused for.
#[test]
fn the_open_flags_that_cannot_be_honoured_refuse_by_name() {
    let _guard = serialized();
    let (f, _scratch) = rooted("flags");
    let path = f.cstring(f.guest.data + 0x100, b"/x");
    // `__open_2` with O_CREAT: the FORTIFY check bionic aborts on.
    let error = refusal_of(&f, "__open_2", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_WRONLY | O_CREAT);
    });
    assert_eq!(error.symbol(), Some("__open_2"), "{error:?}");
    assert!(error.to_string().contains("O_CREAT"), "{error}");
    // And the flags whose guarantees this layer cannot meet.
    for (flag, name) in [
        (0o4010000u64, "O_SYNC"),
        (0o10000u64, "O_DSYNC"),
        (0o40000u64, "O_DIRECT"),
        (0o10000000u64, "O_PATH"),
        (0o20200000u64, "O_TMPFILE"),
    ] {
        let error = refusal_of(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDWR | flag);
            asm.mov(2, 0);
        });
        assert!(error.to_string().contains(name), "{name}: {error}");
    }
    // A bit nobody defined is refused with the bits named, never masked away.
    let error = refusal_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY | (1 << 30));
        asm.mov(2, 0);
    });
    assert!(error.to_string().contains("no name for"), "{error}");
    // `O_DIRECTORY` on a file is ENOTDIR, and on a directory it gives a descriptor that stats.
    assert_eq!(
        value_of(&f, "__open_2", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY | O_DIRECTORY);
        }) as i64,
        -1
    );
}

/// **A wild `FILE *` or `DIR *` is refused by name, never answered.**
///
/// `NULL` is `readdir`'s own end-of-directory answer and `EBADF` is `fileno`'s own invalid-stream
/// answer, so either would let guest code route around a wild pointer. Every `FILE *` and `DIR *`
/// in this guest's world came out of this layer, so one that did not is a real defect.
#[test]
fn a_wild_file_or_directory_pointer_is_refused_by_name() {
    let _guard = serialized();
    let (f, _scratch) = rooted("wild");
    for symbol in ["feof", "fileno", "fclose", "fflush"] {
        let error = refusal_of(&f, symbol, |asm| { asm.mov(0, 0xdead_beef_0000_1000); });
        assert_eq!(error.symbol(), Some(symbol), "{error:?}");
        assert!(error.to_string().contains("dead"), "{error}");
    }
    let error = refusal_of(&f, "readdir", |asm| { asm.mov(0, 0xdead_beef_0000_1000); });
    assert_eq!(error.symbol(), Some("readdir"), "{error:?}");
    // `closedir` on an unknown pointer is EBADF rather than a refusal: it is the one call whose
    // whole job is to release a handle, and `EBADF` is what a double `closedir` gets on a device.
    assert_eq!(value_of(&f, "closedir", |asm| { asm.mov(0, 0xdead_beef_0000_1000); }) as i64, -1);
}

/// **Hostile arguments to the file group: every one is a defined answer or a typed refusal.**
///
/// Null pointers, unmapped pointers, read-only destinations, descriptors nobody opened, counts
/// past `SSIZE_MAX`, and a FORTIFY size smaller than the count. A panic or an abort reachable
/// from any of these is Critical (Global Constraint 11), so the test asserts the *shape* of every
/// outcome rather than a value.
#[test]
fn hostile_arguments_to_the_file_group_are_typed_errors_and_not_panics() {
    let _guard = serialized();
    let (f, scratch) = rooted("hostile");
    std::fs::write(scratch.path("h"), b"0123456789").expect("a file");
    let path = f.cstring(f.guest.data + 0x100, b"/h");
    let fd = value_of(&f, "open", |asm| {
        asm.mov(0, path as u64);
        asm.mov(1, O_RDONLY);
        asm.mov(2, 0);
    });
    let unmapped = f.guest.unmapped as u64;
    let readonly = f.guest.readonly as u64;

    let cases: &[(&str, &[u64])] = &[
        // Null and wild paths.
        ("open", &[0, O_RDONLY, 0]),
        ("open", &[unmapped, O_RDONLY, 0]),
        ("stat", &[0, f.guest.data as u64]),
        ("lstat", &[unmapped, f.guest.data as u64]),
        ("statvfs", &[0, f.guest.data as u64]),
        ("access", &[0, 0]),
        ("unlink", &[0]),
        ("mkdir", &[unmapped, 0o755]),
        ("rmdir", &[0]),
        ("opendir", &[0]),
        ("rename", &[0, 0]),
        // Destinations that cannot be written.
        ("stat", &[path as u64, unmapped]),
        ("stat", &[path as u64, readonly]),
        ("fstat", &[fd, readonly]),
        ("statvfs", &[path as u64, unmapped]),
        ("read", &[fd, unmapped, 16]),
        ("read", &[fd, readonly, 16]),
        // Descriptors nobody opened, including the negative ones a failed call returns.
        ("read", &[0xffff_ffff_ffff_ffff, f.guest.data as u64, 16]),
        ("close", &[0xffff_ffff_ffff_ffff]),
        ("fstat", &[999, f.guest.data as u64]),
        ("pread", &[999, f.guest.data as u64, 16, 0]),
        ("__write_chk", &[999, f.guest.data as u64, 4, 4]),
        // Counts and offsets the guest chose.
        ("read", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff]),
        ("pread", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff, 0]),
        ("pread", &[fd, f.guest.data as u64, 16, 0x8000_0000_0000_0000]),
        ("__write_chk", &[fd, f.guest.data as u64, 0xffff_ffff_ffff_ffff, 0xffff_ffff_ffff_ffff]),
        // A FORTIFY size smaller than the count: a detected overrun in guest code.
        ("__write_chk", &[fd, f.guest.data as u64, 64, 8]),
        // Zero-length transfers at a null pointer, which are legal C and must not fault.
        ("read", &[fd, 0, 0]),
        ("__write_chk", &[fd, 0, 0, 0]),
        // Streams.
        ("fopen", &[0, 0]),
        ("fopen", &[path as u64, 0]),
        ("fopen", &[path as u64, unmapped]),
        ("fdopen", &[999, 0]),
        ("fgets", &[0, 64, 0]),
        ("fread", &[unmapped, 1, 16, 0]),
    ];

    for (symbol, args) in cases {
        let entry = call_one(&f, symbol, |asm| {
            for (index, value) in args.iter().enumerate() {
                asm.mov(index as u32, *value);
            }
        });
        let mut cpu = f.guest.thread(&f.boundary);
        match f.run(&mut cpu, entry) {
            Ok(exit) => {
                assert!(
                    matches!(exit, ExitReason::Returned { .. }),
                    "`{symbol}` {args:x?}: {exit:?}"
                );
                let value = f.guest.read_u64(f.guest.data) as i64;
                assert!(
                    value == 0 || value == -1 || value >= 0,
                    "`{symbol}` {args:x?} completed with {value}"
                );
            }
            Err(error) => {
                assert_eq!(error.symbol(), Some(*symbol), "{error:?}");
                assert!(error.guest_address().is_some(), "{error}");
            }
        }
    }
}

/// The descriptor and stream ceilings are `EMFILE`, and they are real bounds.
///
/// A guest that leaks descriptors in a loop would otherwise hold as many host handles as it
/// liked, and several instances share one host process.
#[test]
fn the_descriptor_and_stream_ceilings_report_emfile_rather_than_growing() {
    let _guard = serialized();
    let (f, scratch) = rooted("ceilings");
    std::fs::write(scratch.path("c"), b"x").expect("a file");
    let path = f.cstring(f.guest.data + 0x100, b"/c");
    let mode = f.cstring(f.guest.data + 0x140, b"r");

    let mut opened = 0usize;
    loop {
        let fd = value_of(&f, "open", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, O_RDONLY);
            asm.mov(2, 0);
        }) as i64;
        if fd < 0 {
            break;
        }
        opened += 1;
        assert!(opened <= omni_platform::fs::MAX_OPEN_FILES, "the descriptor ceiling never fired");
    }
    assert_eq!(
        opened,
        omni_platform::fs::MAX_OPEN_FILES - 3,
        "three of the descriptors are the standard streams"
    );
    // And a `fopen` now fails too, because it needs a descriptor first.
    assert_eq!(
        value_of(&f, "fopen", |asm| {
            asm.mov(0, path as u64);
            asm.mov(1, mode as u64);
        }),
        0
    );
}

// =================================================================== thread lifecycle (phase 3c)

/// A fixture whose instance can create guest threads, over the guest's own backend.
fn fixture_with_threads(limit: usize) -> Fixture {
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(limit))
        .expect("a thread host");
    f
}

/// Write a `u64` into guest memory from a host thread that does not hold the `Guest`.
///
/// `Guest` is deliberately not `Sync` — it owns a `Cell` for the next code offset — so a helper
/// thread cannot borrow it. Identity mapping (D4) makes a guest address a host address, and
/// `GuestSpace` is `Send + Sync`, so this is the whole of what such a thread needs.
fn poke(space: &Arc<omni_mem::GuestSpace>, at: omni_cpu::GuestAddr, value: u64) {
    let ptr = space.ptr(at, 8).expect("a host pointer for a guest word");
    // SAFETY: `GuestSpace::ptr` has checked that the eight bytes at `at` are mapped and
    // committed in this space, and D4 makes the guest address a host address. Guest threads may
    // be reading this word concurrently, which is the point of it — it is a gate they poll —
    // and an aligned 8-byte store is what they poll for.
    unsafe { ptr.cast::<u64>().write_unaligned(value) }
}

/// Opens a set of guest gate words when dropped, **including while a panic unwinds**.
///
/// Every test here that starts a guest thread which spins on a gate needs one. A failing
/// assertion otherwise leaves that thread spinning for the rest of the test binary: it does not
/// hang — libtest exits the process when the run finishes — but it burns a core and makes every
/// later test slower and noisier, and under a mutation run that is dozens of rows. Found when
/// mutation row `threads-B1` made `pthread_join` block in a test whose program opened its gate
/// *after* the join, which deadlocked the harness rather than failing it.
struct OpenOnDrop {
    space: Arc<omni_mem::GuestSpace>,
    gates: Vec<omni_cpu::GuestAddr>,
}

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        for gate in &self.gates {
            poke(&self.space, *gate, 1);
        }
    }
}

/// Assemble a start routine: `X0` is the guest's `arg`, and whatever it leaves in `X0` is the
/// thread's `void *`. It finishes with `RET`, which lands on the boundary's sentinel.
fn start_routine(f: &Fixture, build: impl FnOnce(&mut Asm)) -> omni_cpu::GuestAddr {
    let entry = f.guest.next_entry();
    let mut asm = Asm::at(entry);
    build(&mut asm);
    asm.push(ret(30));
    f.guest.load(asm.words());
    entry
}

/// `pthread_create(&tid, attr, start, arg)` with `tid` at `out` and the result at `out + 8`.
fn create_call(
    f: &Fixture,
    asm: &mut Asm,
    out: omni_cpu::GuestAddr,
    attr: u64,
    start: omni_cpu::GuestAddr,
    arg: u64,
) {
    asm.mov(0, out as u64);
    asm.mov(1, attr);
    asm.mov(2, start as u64);
    asm.mov(3, arg);
    asm.bl(f.thunk("pthread_create"));
    asm.mov(22, out as u64);
    asm.push(str_imm(0, 22, 8));
}

/// **An instance with no thread host refuses `pthread_create` by name.**
///
/// No default is possible: only a CPU backend can give a new guest thread the bionic TLS block
/// D13 requires before it runs an instruction. `EAGAIN` would say the runtime ran out of
/// resources when it was never configured, and a guest would retry for ever.
#[test]
fn pthread_create_without_a_thread_host_refuses_by_name() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let error = refusal_of(&f, "pthread_create", |asm| {
        asm.mov(0, f.guest.data as u64 + 0x800);
        asm.mov(1, 0);
        asm.mov(2, f.guest.code as u64);
        asm.mov(3, 0);
    });
    assert_eq!(error.symbol(), Some("pthread_create"));
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("set_thread_host"), "the refusal must name the method: {text}");
    assert!(text.contains("D13"), "and say what it cannot satisfy without one: {text}");
}

/// **The phase, end to end: a real guest thread runs real translated ARM64 code, and the join
/// brings its `void *` back.**
///
/// Asserted three ways, because each one alone would pass against a different broken version:
/// the thread's own *side effect* in guest memory (a handler that returned 0 without starting
/// anything would leave it untouched), the value `pthread_join` writes (a join that succeeded
/// without waiting would read it before the thread wrote it), and the return codes of both calls.
#[test]
fn a_guest_thread_runs_guest_code_and_join_brings_back_its_value() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let marker = f.guest.data + 0x880;
    f.guest.write_u64(marker, 0);

    // The start routine: store `arg` at `marker`, and return `arg + 1`.
    let start = start_routine(&f, |asm| {
        asm.mov(9, marker as u64);
        asm.push(str_imm(0, 9, 0));
        asm.push(add_imm(0, 0, 1));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0x4321);
        // pthread_join(tid, &retval)
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, out as u64 + 16);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create returns 0 on success");
    assert_eq!(f.guest.read_u64(out + 24), 0, "pthread_join returns 0 on success");
    assert_eq!(f.guest.read_u64(marker), 0x4321, "the start routine really ran, with its arg");
    assert_eq!(f.guest.read_u64(out + 16), 0x4322, "and its `void *` came back through the join");
    assert_ne!(f.guest.read_u64(out), 0, "the pthread_t is never the reserved zero");
    assert_eq!(f.bionic.live_guest_threads(), 0, "the thread was reaped by the join");
}

/// **D13, on a thread the guest created: `TPIDR_EL0` is programmed before the thread's first
/// instruction, and its stack guard is the same value every other thread has.**
///
/// This is the constraint that is easiest to satisfy incorrectly and hardest to notice: a guest
/// thread with a zero thread pointer faults on the first stack-protected call, with a symptom
/// that reads as a loader bug, and one with a *different* guard than its parent fails
/// `__stack_chk_fail` only when a canary crosses threads.
///
/// The start routine reads the thread pointer itself, exactly as 1,276 of `libroblox.so`'s own
/// instructions do, and stores both the pointer and `[Xt, #0x28]`.
#[test]
fn a_created_guest_thread_has_its_thread_pointer_and_the_process_stack_guard() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let seen = f.guest.data + 0x900;
    f.guest.write_u64(seen, 0);
    f.guest.write_u64(seen + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.mov(9, seen as u64);
        asm.push(mrs_tpidr_el0(10));
        asm.push(str_imm(10, 9, 0));
        asm.push(ldr_imm(11, 10, 0x28));
        asm.push(str_imm(11, 9, 8));
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");

    let pointer = f.guest.read_u64(seen);
    let guard = f.guest.read_u64(seen + 8);
    assert_ne!(pointer, 0, "a guest thread with a null TPIDR_EL0 is D13's crash");
    assert!(
        f.guest.space.contains(pointer as usize, 0x30),
        "the thread pointer must be guest memory with room for the slots up to +0x28"
    );
    assert_ne!(guard, 0, "a zero canary compares equal to a zeroed stack slot");
    assert_eq!(
        guard,
        f.guest.backend.tls().stack_guard(),
        "bionic copies ONE per-process guard into every thread; a created thread with its own \
         value fails __stack_chk_fail only when a canary crosses threads"
    );
}

/// The `pthread_t` the parent is given is the one the child's `pthread_self()` returns, and the
/// two threads have different `errno` cells.
///
/// Allocating the identity in the child would make these differ until the child got there, which
/// is a race guest code loses rarely and silently.
#[test]
fn a_created_thread_agrees_with_its_parent_about_its_own_identity() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let seen = f.guest.data + 0x900;
    f.guest.write_u64(seen, 0);
    f.guest.write_u64(seen + 8, 0);

    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.mov(20, seen as u64);
        asm.bl(f.thunk("pthread_self"));
        asm.push(str_imm(0, 20, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 20, 8));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
        // And this thread's own errno cell, for the comparison.
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 22, 32));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");
    assert_eq!(
        f.guest.read_u64(seen),
        f.guest.read_u64(out),
        "the child's pthread_self() must be the pthread_t the parent was handed"
    );
    assert_ne!(
        f.guest.read_u64(seen + 8),
        f.guest.read_u64(out + 32),
        "two guest threads sharing one errno cell is the failure the arena exists to prevent"
    );
}

/// **A start routine at an address that is not guest code is a recorded failure, and the join
/// refuses rather than reporting a `void *` the thread never produced.**
///
/// The hostile case the brief names. A `0` from the join with a null `retval` would be
/// indistinguishable from a thread that returned `NULL`.
#[test]
fn a_start_routine_at_a_bad_address_is_a_recorded_failure_and_a_join_refusal() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, f.guest.unmapped, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, out as u64 + 16);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });
    let error = match run_program(&f, entry) {
        Err(error) => error,
        Ok(exit) => panic!("the join must refuse, got {exit:?}"),
    };
    assert_eq!(error.symbol(), Some("pthread_join"));
    assert!(matches!(error, AbiError::Refused { .. }), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("stopped without returning"), "{text}");

    // `pthread_create` itself succeeded: the thread was created, and it is the thread that
    // failed. That distinction is the whole reason the failure is recorded separately.
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create reported success");
    let failures = f.bionic.guest_thread_failures();
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].start_routine, f.guest.unmapped);
    assert_eq!(failures[0].thread, f.guest.read_u64(out));
}

/// A detached thread that fails has nobody to report to, so the instance's failure list is the
/// only place it can surface — and it is, with the thread and its start routine named.
#[test]
fn a_detached_guest_thread_that_fails_is_still_recorded() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let entry = program(&f, |asm| {
        // pthread_attr_init, then write PTHREAD_CREATE_DETACHED into it directly -- the guest's
        // own attr bytes, which is all a detach state is.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(9, attr as u64);
        asm.mov(10, 1);
        asm.push(str_w(10, 9, 0));
        create_call(&f, asm, out, attr as u64, f.guest.unmapped, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "pthread_create reported success");

    // The thread is detached, so nothing joins it: wait for the failure to be recorded.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.bionic.guest_thread_failures().is_empty() {
        assert!(std::time::Instant::now() < deadline, "a detached thread's failure was never recorded");
        std::thread::yield_now();
    }
    let failures = f.bionic.guest_thread_failures();
    assert_eq!(failures[0].start_routine, f.guest.unmapped);
    assert!(
        failures[0].to_string().contains("start routine"),
        "the record must name the thread and its start routine: {}",
        failures[0]
    );
    // And the record is gone: nobody will ever join a detached thread, so keeping one would be a
    // leak a guest could drive in a loop.
    while f.bionic.guest_thread_records() > 0 {
        assert!(std::time::Instant::now() < deadline, "a detached thread's record was never removed");
        std::thread::yield_now();
    }
}

/// The POSIX error cases, each of which is a branch guest code has.
///
/// Asserted **by number**: `EDEADLK` is 35, `ESRCH` is 3 and `EINVAL` is 22 in Linux's numbering,
/// which is not the host's, and a handler that returned the host's would be wrong in a way that
/// only a guest can see.
#[test]
fn the_join_and_detach_error_cases_are_the_posix_ones() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;

    // Joining yourself.
    let entry = program(&f, |asm| {
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        // An id nothing ever handed out.
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 8));
        // Detaching one.
        asm.mov(0, 0xDEAD_BEEF);
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 16));
        // And `pthread_getschedparam` on it.
        asm.mov(0, 0xDEAD_BEEF);
        asm.mov(1, out as u64 + 64);
        asm.mov(2, out as u64 + 72);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 24));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out), 35, "joining yourself is EDEADLK");
    assert_eq!(f.guest.read_u64(out + 8), 3, "joining an unknown thread is ESRCH");
    assert_eq!(f.guest.read_u64(out + 16), 3, "detaching an unknown thread is ESRCH");
    assert_eq!(f.guest.read_u64(out + 24), 3, "getschedparam on an unknown thread is ESRCH");
}

/// A second `pthread_detach` is `EINVAL`, and a `pthread_join` on a detached thread is too.
///
/// **A second detach answering `0` would be indistinguishable from the first**, and a double
/// detach is a guest defect worth hearing about: in a real implementation it is a use-after-free
/// of the thread's own descriptor. The thread here spins on a guest word so that it is still
/// running while both calls are made — a thread that had already finished would exercise the
/// reap path instead, which is a different branch.
#[test]
fn detaching_twice_is_einval_and_joining_a_detached_thread_is_einval() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0x980;
    f.guest.write_u64(gate, 0);

    // Spin until the parent stores a non-zero word, then return.
    let start = start_routine(&f, |asm| {
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    let ready = f.guest.data + 0x988;
    f.guest.write_u64(ready, 0);
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        asm.push(mov_reg(0, 19));
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 16));
        asm.push(mov_reg(0, 19));
        asm.bl(f.thunk("pthread_detach"));
        asm.push(str_imm(0, 22, 24));
        // Both detaches are done: tell the host, which releases the thread. See below.
        asm.mov(9, ready as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.push(mov_reg(0, 19));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 32));
    });

    // **The gate is opened from the host, once both detaches have happened, and not by the guest
    // program after its own `pthread_join`.** The join here is *expected* to return EINVAL
    // immediately, because the thread is detached — but a defect that made it block instead
    // would deadlock against a gate the same program had not reached yet. That is not a
    // hypothetical: mutation row `threads-B1` refuses the *first* detach, which leaves the
    // thread joinable, and the first version of this test hung the whole mutation harness on it
    // rather than failing. A test that deadlocks under the defect it exists to detect reports
    // nothing at all.
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let opener = {
        let space = Arc::clone(&f.guest.space);
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while space.ptr(ready, 8).map_or(0, |p| unsafe { p.cast::<u64>().read_unaligned() })
                == 0
            {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::yield_now();
            }
            poke(&space, gate, 1);
        })
    };
    let outcome = run_program(&f, entry);
    opener.join().expect("the gate opener");
    assert!(matches!(outcome.expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), 0, "the first detach succeeds");
    assert_eq!(f.guest.read_u64(out + 24), 22, "the second is EINVAL, not a second success");
    assert_eq!(f.guest.read_u64(out + 32), 22, "a detached thread is not joinable");

    // And the detached thread really does finish and clean itself up.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.bionic.guest_thread_records() > 0 {
        assert!(std::time::Instant::now() < deadline, "the detached thread never finished");
        std::thread::yield_now();
    }
    assert!(f.bionic.guest_thread_failures().is_empty(), "it returned, so it did not fail");
}

/// **Two guest threads joining each other is `EDEADLK`, not two host threads blocked for ever.**
///
/// A hang from untrusted guest input is a denial of service on the host, and POSIX permits
/// `EDEADLK` for exactly this ("a deadlock was detected"). The check walks the wait chain, so it
/// catches a cycle of any length rather than only the self-join POSIX names.
#[test]
fn two_guest_threads_joining_each_other_is_edeadlk() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let ids = f.guest.data + 0xB00;
    let results = f.guest.data + 0xB40;
    for offset in 0..8u32 {
        f.guest.write_u64(ids + offset as usize * 8, 0);
        f.guest.write_u64(results + offset as usize * 8, 0);
    }

    // The ids double as the gates these two spin on, so a failing assertion releases them.
    let _gates = OpenOnDrop {
        space: Arc::clone(&f.guest.space),
        gates: vec![ids, ids + 8],
    };
    // Each thread joins whichever id it is handed, spinning until it is non-zero first so that
    // both ids are published before either join is attempted.
    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.push(mov_reg(20, 0)); // the address of the id to join
        let loop_at = asm.pc();
        asm.push(ldr_imm(9, 20, 0));
        asm.push(subs_imm(9, 9, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.push(mov_reg(0, 9));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 20, 16));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    let entry = program(&f, |asm| {
        // Thread A is told to join what lands at `ids + 8`; thread B, `ids + 0`.
        create_call(&f, asm, out, 0, start, ids as u64 + 8);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        create_call(&f, asm, out + 32, 0, start, ids as u64);
        asm.mov(22, out as u64 + 32);
        asm.push(ldr_imm(20, 22, 0));
        // Publish both ids at once, so neither thread can join before the other exists.
        asm.mov(9, ids as u64);
        asm.push(str_imm(19, 9, 0));
        asm.push(str_imm(20, 9, 8));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    // **The parent does not join them**, and that is not a convenience: a `pthread_join` from
    // this thread would itself be a waiter on one of the two, and the other's join would then be
    // refused as "already being joined" before it ever reached the cycle check. The first draft
    // did exactly that and measured EINVAL instead of EDEADLK.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while f.bionic.live_guest_threads() > 0 {
        assert!(std::time::Instant::now() < deadline, "the mutual join deadlocked");
        std::thread::yield_now();
    }

    // One of the two saw the cycle. Which one is a race and is not asserted; that exactly one
    // did is not.
    let a = f.guest.read_u64(ids + 16);
    let b = f.guest.read_u64(ids + 24);
    assert!(
        a == 35 || b == 35,
        "one of the two mutual joins must be EDEADLK, got {a} and {b}"
    );
    assert!(f.bionic.guest_thread_failures().is_empty(), "neither thread failed: {:?}", f.bionic.guest_thread_failures());
}

/// A hostile `pthread_attr_t`: a detach state the guest wrote directly, a stack under the floor,
/// and a stack size no mapping of which can exist.
///
/// **`SIZE_MAX` is the one that matters.** Rounding it up to a page with `(n + page - 1) &
/// !(page - 1)` gives **zero**, silently in a release build, so a guest asking for the largest
/// stack in the world would be handed a zero-byte one. That is `gmtime(i64::MIN)` again.
#[test]
fn a_hostile_pthread_attr_is_einval_or_eagain_and_never_a_zero_stack() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    let attr = f.guest.data + 0xA00;
    let start = start_routine(&f, |asm| {
        asm.mov(0, 7);
    });

    let entry = program(&f, |asm| {
        // A detach state that is neither JOINABLE nor DETACHED.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(9, attr as u64);
        asm.mov(10, 99);
        asm.push(str_w(10, 9, 0));
        create_call(&f, asm, out, attr as u64, start, 0);

        // A stack under the floor.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, 64);
        asm.bl(f.thunk("pthread_attr_setstacksize"));
        create_call(&f, asm, out + 32, attr as u64, start, 0);

        // A stack of SIZE_MAX.
        asm.mov(0, attr as u64);
        asm.bl(f.thunk("pthread_attr_init"));
        asm.mov(0, attr as u64);
        asm.mov(1, u64::MAX);
        asm.bl(f.thunk("pthread_attr_setstacksize"));
        create_call(&f, asm, out + 64, attr as u64, start, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));

    assert_eq!(f.guest.read_u64(out + 8), 22, "a detach state of 99 is EINVAL");
    assert_eq!(f.guest.read_u64(out + 40), 22, "a 64-byte stack is EINVAL");
    assert_eq!(f.guest.read_u64(out + 72), 11, "a SIZE_MAX stack is EAGAIN, never a zero stack");
    assert_eq!(f.bionic.guest_thread_records(), 0, "none of the three created a thread");
    assert_eq!(f.bionic.live_guest_threads(), 0);
}

/// **Thread-count exhaustion is `EAGAIN`, which is POSIX's own answer and a branch guest code
/// has** — not a refusal, and not a thread that shares another one's `errno` block.
#[test]
fn creating_more_guest_threads_than_the_limit_is_eagain() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0x980;
    f.guest.write_u64(gate, 0);
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let start = start_routine(&f, |asm| {
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        for slot in 0..3u32 {
            create_call(&f, asm, out + slot as usize * 32, 0, start, 0);
        }
        // Let them all go, then join the two that started.
        asm.mov(9, gate as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        for slot in 0..2u32 {
            asm.mov(22, out as u64 + u64::from(slot) * 32);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
        }
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the first is created");
    assert_eq!(f.guest.read_u64(out + 40), 0, "and the second");
    assert_eq!(f.guest.read_u64(out + 72), 11, "the third is EAGAIN, the limit being 2");
    assert_eq!(f.bionic.live_guest_threads(), 0, "both were reaped");
    assert!(f.bionic.guest_thread_failures().is_empty());
}

/// **A guest thread's arena block goes back when it exits**, so a guest that creates and joins in
/// a loop is not limited to 64 threads for the life of the instance.
///
/// The block index comes from a free list rather than from the table's length, and the length
/// version is what makes two live threads share one block — which is silent. This test creates
/// and joins more threads than the live limit, which only works if blocks are reused.
#[test]
fn an_exited_guest_thread_gives_its_arena_block_back() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let out = f.guest.data + 0x800;
    let counter = f.guest.data + 0x9C0;
    f.guest.write_u64(counter, 0);
    let start = start_routine(&f, |asm| {
        asm.mov(9, counter as u64);
        asm.push(ldr_imm(10, 9, 0));
        asm.push(add_imm(10, 10, 1));
        asm.push(str_imm(10, 9, 0));
        asm.mov(0, 0);
    });

    for _ in 0..6 {
        let entry = program(&f, |asm| {
            create_call(&f, asm, out, 0, start, 0);
            asm.mov(22, out as u64);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
            asm.push(str_imm(0, 22, 24));
        });
        assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
        assert_eq!(f.guest.read_u64(out + 8), 0, "every create succeeds");
        assert_eq!(f.guest.read_u64(out + 24), 0, "every join succeeds");
    }
    assert_eq!(f.guest.read_u64(counter), 6, "six threads really ran");
    // **One**: this test's own host thread, which attached when the first program ran. Six
    // guest threads came and went; if a block were never given back the count would be seven,
    // and the seventh `pthread_create` would eventually be refused rather than reusing one.
    assert_eq!(
        f.bionic.attached(),
        1,
        "every exited guest thread must have given its arena block back"
    );
}

/// `pthread_getschedparam` answers `SCHED_OTHER` with a priority of 0 for a thread this instance
/// created, and it writes both out-parameters.
///
/// The argument that this is an answer rather than a stub is in the handler's documentation and
/// rests on there being no way to *set* a policy anywhere in the reachable 188 — which
/// `the_bound_count_is_exactly_what_this_phase_claims` and the reachable list together pin.
#[test]
fn getschedparam_answers_sched_other_with_priority_zero() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0x800;
    f.guest.write_u64(out + 64, 0x5555_5555_5555_5555);
    f.guest.write_u64(out + 72, 0x6666_6666_6666_6666);
    f.guest.write_u64(out + 80, 0x7777_7777_7777_7777);
    f.guest.write_u64(out + 88, 0x8888_8888_8888_8888);
    // **No gate, and the thread returns immediately.** Whether it is still running when the
    // question is asked does not matter: a joinable thread's record outlives it until the join,
    // so `pthread_getschedparam` answers either way — and a thread that spins would have to be
    // released, which is one more way for a failing assertion to leave one behind.
    let start = start_routine(&f, |asm| {
        asm.mov(0, 0);
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
        asm.mov(22, out as u64);
        asm.push(ldr_imm(19, 22, 0));
        asm.push(mov_reg(0, 19));
        asm.mov(1, out as u64 + 64);
        asm.mov(2, out as u64 + 72);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 24));
        // And about **this** thread, which `pthread_create` did not make. The first version
        // answered ESRCH for it, because the registry only held created threads — the wrong
        // answer, since the main thread is a thread of this process with the same default
        // policy, and a guest asking about itself during initialisation would have taken an
        // error branch for no reason.
        asm.bl(f.thunk("pthread_self"));
        asm.mov(1, out as u64 + 80);
        asm.mov(2, out as u64 + 88);
        asm.bl(f.thunk("pthread_getschedparam"));
        asm.push(str_imm(0, 22, 40));
        asm.push(mov_reg(0, 19));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "it answers for a thread it created");
    assert_eq!(f.guest.read_u64(out + 40), 0, "and for the thread asking, which it did not create");
    assert_eq!(f.guest.read_u64(out + 80) & 0xFFFF_FFFF, 0, "SCHED_OTHER for this thread too");
    assert_eq!(
        f.guest.read_u64(out + 64) & 0xFFFF_FFFF,
        0,
        "SCHED_OTHER is SCHED_NORMAL and is 0"
    );
    assert_eq!(
        f.guest.read_u64(out + 72) & 0xFFFF_FFFF,
        0,
        "sched_priority has exactly one legal value under SCHED_OTHER"
    );
}

/// The stop switch really stops a guest thread that would otherwise run for ever, at a run-window
/// boundary — and a `pthread_join` on it is a refusal rather than a `void *` it never produced.
///
/// **D16's shape**: the bound on a runaway guest is a short budget window plus a decision point,
/// not a cross-thread halt. The window is lowered here so the test is a second rather than a
/// minute; that the mechanism is the window and not the halt is what the low value demonstrates.
#[test]
fn a_runaway_guest_thread_stops_at_a_window_boundary() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(2).with_step_window(1_000))
        .expect("a thread host");
    let out = f.guest.data + 0x800;
    let running = f.guest.data + 0x9E0;
    f.guest.write_u64(running, 0);

    // An unconditional loop that first says it is running.
    let start = start_routine(&f, |asm| {
        asm.mov(9, running as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.push(b(0));
    });

    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
    });
    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the thread was created");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while f.guest.read_u64(running) == 0 {
        assert!(std::time::Instant::now() < deadline, "the runaway thread never started");
        std::thread::yield_now();
    }
    f.bionic.stop_guest_threads();
    while f.bionic.live_guest_threads() > 0 {
        assert!(std::time::Instant::now() < deadline, "the stop switch did not stop it");
        std::thread::yield_now();
    }
    assert_eq!(
        f.bionic.guest_thread_state(f.guest.read_u64(out)),
        Some(omni_android::bionic::GuestThreadState::Stopped)
    );

    // And the join refuses, naming what happened, rather than reporting a value.
    let join = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
    });
    let error = match run_program(&f, join) {
        Err(error) => error,
        Ok(exit) => panic!("the join must refuse, got {exit:?}"),
    };
    assert!(error.to_string().contains("asked to stop"), "{error}");
}

/// A second thread host is refused: a second CPU backend would allocate TLS blocks from a second
/// arena, and bionic copies **one** stack-guard value into every thread of a process.
#[test]
fn a_thread_host_may_be_set_only_once() {
    let _guard = serialized();
    let f = fixture_with_threads(2);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    match f.bionic.set_thread_host(ThreadHost::new(backend)) {
        Err(AbiError::Refused { why, .. }) => {
            assert!(why.contains("stack-guard"), "the refusal must say what breaks: {why}");
        }
        other => panic!("a second thread host must be refused, got {other:?}"),
    }
}

/// **A second guest thread's translations are invalidated when another thread unmaps a range.**
///
/// Phase 2 recorded `ReentrantCall::invalidate_code` as reaching **one** context — "a narrowing
/// of the window, not a closing of it" — and said the registry of live contexts that would close
/// the rest belonged with thread lifecycle. This is that registry, and this is the test that can
/// tell it apart from nothing happening.
///
/// **A detector rather than a watch** (Global Constraint 13): both counters stay at zero under
/// exactly this workload if the broadcast is removed, because there is no other path that raises
/// them. What is asserted is that a range one thread unmapped reached a *different* live context
/// and was applied to its CPU.
#[test]
fn a_range_one_guest_thread_unmaps_reaches_another_threads_context() {
    let _guard = serialized();
    let f = fixture_with(&[]);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&f.guest.backend) as _;
    f.bionic
        .set_thread_host(ThreadHost::new(backend).with_limit(2).with_step_window(1_000))
        .expect("a thread host");
    let out = f.guest.data + 0x800;
    let gate = f.guest.data + 0xC80;
    let running = f.guest.data + 0xC88;
    f.guest.write_u64(gate, 0);
    f.guest.write_u64(running, 0);
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };

    // A guest thread that keeps running -- so it keeps reaching run-window boundaries, which is
    // where a context applies what another one queued for it.
    let _gates = OpenOnDrop { space: Arc::clone(&f.guest.space), gates: vec![gate] };
    let start = start_routine(&f, |asm| {
        asm.mov(9, running as u64);
        asm.mov(10, 1);
        asm.push(str_imm(10, 9, 0));
        asm.mov(9, gate as u64);
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 9, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
    });

    // **Every program this test runs is assembled before the second thread starts.**
    // `Guest::load` flips the whole shared code region to `ReadWrite` and back to `ReadExecute`,
    // and doing that while another guest thread is fetching from it is a fault in that thread
    // rather than anything to do with what is being tested. Found by this test failing about one
    // run in three.
    let entry = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, 0);
    });
    let mapped = program(&f, |asm| {
        asm.mov(0, 0);
        asm.mov(1, 0x1_0000);
        asm.mov(2, PROT_RW);
        asm.mov(3, MAP_ANON_PRIVATE);
        asm.mov(4, u64::MAX);
        asm.mov(5, 0);
        asm.bl(f.thunk("mmap"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 64));
        asm.mov(1, 0x1_0000);
        asm.bl(f.thunk("munmap"));
        asm.push(str_imm(0, 22, 72));
    });
    let join = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 24));
    });

    assert!(matches!(run_program(&f, entry).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0, "the thread was created");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while f.guest.read_u64(running) == 0 {
        assert!(std::time::Instant::now() < deadline, "the second guest thread never started");
        std::thread::yield_now();
    }
    let before = f.boundary.code_invalidations();
    assert_eq!(before.applied, 0, "nothing has been unmapped yet");

    // Now this thread maps and unmaps a range, through the guest's own `mmap`/`munmap`.
    assert!(matches!(run_program(&f, mapped).expect("completes"), ExitReason::Returned { .. }));
    assert_ne!(f.guest.read_u64(out + 64), u64::MAX, "the mapping succeeded");
    assert_eq!(f.guest.read_u64(out + 72), 0, "and the unmapping did");

    // The other context picks it up at its next run-window boundary.
    while f.boundary.code_invalidations().applied == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the other guest thread's context never applied the invalidation: {:?}",
            f.boundary.code_invalidations()
        );
        std::thread::yield_now();
    }
    let after = f.boundary.code_invalidations();
    assert!(after.queued >= 1, "{after:?}");
    assert!(after.applied >= 1, "{after:?}");
    assert_eq!(after.overflows, 0, "two ranges do not fill a queue of 64");

    // Let it go and reap it.
    f.guest.write_u64(gate, 1);
    assert!(matches!(run_program(&f, join).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 24), 0, "the join succeeded");
}

/// **A thread that exits must not hand its arena block to a thread that is still using one.**
///
/// The block index used to come from the table's length, which is exact only for a table nothing
/// is ever removed from — and until this phase nothing was. Remove the entry holding index 1 from
/// a table of three and the length is 2, so the next thread is handed index 2, which is live.
/// Two guest threads would then share one `errno` cell and one `strerror` buffer, and the only
/// symptom would be an occasional wrong error number in a thread that did nothing wrong.
///
/// The sequence is the one that produces the collision, not the one that is easiest to write:
/// start two threads, let the **first** exit, then start a third while the second is still
/// running, and compare the third's `errno` cell with the second's.
#[test]
fn a_thread_that_exits_does_not_give_its_block_to_a_live_thread() {
    let _guard = serialized();
    let f = fixture_with_threads(4);
    let out = f.guest.data + 0xD80;
    // Three records of { gate, errno }, one per thread.
    let rec = |n: usize| f.guest.data + 0xD00 + n * 16;
    for n in 0..3 {
        f.guest.write_u64(rec(n), 0);
        f.guest.write_u64(rec(n) + 8, 0);
    }

    // `X0` is this thread's record: publish `__errno()` into it, then spin until its gate opens.
    let start = start_routine(&f, |asm| {
        asm.push(mov_reg(19, 30));
        asm.push(mov_reg(20, 0));
        asm.bl(f.thunk("__errno"));
        asm.push(str_imm(0, 20, 8));
        let loop_at = asm.pc();
        asm.push(ldr_imm(10, 20, 0));
        asm.push(subs_imm(10, 10, 0));
        let here = asm.pc();
        asm.push(b_cond(0, ((loop_at as i64 - here as i64) / 4) as i32));
        asm.mov(0, 0);
        asm.push(mov_reg(30, 19));
    });

    // Every program up front: `Guest::load` reprotects the shared code region, and doing that
    // while a guest thread is fetching from it faults that thread.
    let create_two = program(&f, |asm| {
        create_call(&f, asm, out, 0, start, rec(0) as u64);
        create_call(&f, asm, out + 32, 0, start, rec(1) as u64);
    });
    let join_first = program(&f, |asm| {
        asm.mov(22, out as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.push(str_imm(0, 22, 16));
    });
    let create_third = program(&f, |asm| {
        create_call(&f, asm, out + 64, 0, start, rec(2) as u64);
    });
    let join_rest = program(&f, |asm| {
        for slot in [1u64, 2] {
            asm.mov(22, out as u64 + slot * 32);
            asm.push(ldr_imm(0, 22, 0));
            asm.mov(1, 0);
            asm.bl(f.thunk("pthread_join"));
            asm.push(str_imm(0, 22, 16));
        }
    });

    let _gates = OpenOnDrop {
        space: Arc::clone(&f.guest.space),
        gates: (0..3).map(rec).collect(),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let wait_for = |at: omni_cpu::GuestAddr| {
        while f.guest.read_u64(at) == 0 {
            assert!(std::time::Instant::now() < deadline, "a guest thread never published {at:#x}");
            std::thread::yield_now();
        }
        f.guest.read_u64(at)
    };

    assert!(matches!(run_program(&f, create_two).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 8), 0);
    assert_eq!(f.guest.read_u64(out + 40), 0);
    let first = wait_for(rec(0) + 8);
    let second = wait_for(rec(1) + 8);
    assert_ne!(first, second, "two live threads already share an errno cell");

    // Let the FIRST one go and reap it, leaving a hole in the middle of the table.
    f.guest.write_u64(rec(0), 1);
    assert!(matches!(run_program(&f, join_first).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.guest.read_u64(out + 16), 0, "the first join succeeded");

    // Now a third, which must be given the hole and not the block the second is using.
    assert!(matches!(
        run_program(&f, create_third).expect("completes"),
        ExitReason::Returned { .. }
    ));
    assert_eq!(f.guest.read_u64(out + 72), 0, "the third was created");
    let third = wait_for(rec(2) + 8);
    assert_ne!(
        third, second,
        "the third guest thread was handed the block the second is still using: two threads \
         sharing one errno cell is the failure the arena exists to prevent"
    );
    assert_eq!(third, first, "and it should be the block the first one gave back");

    f.guest.write_u64(rec(1), 1);
    f.guest.write_u64(rec(2), 1);
    assert!(matches!(run_program(&f, join_rest).expect("completes"), ExitReason::Returned { .. }));
    assert_eq!(f.bionic.live_guest_threads(), 0);
    assert!(f.bionic.guest_thread_failures().is_empty());
}
