//! **The raw syscalls the engine issues, driven by real translated ARM64 code.**
//!
//! ```text
//! cargo test -p omni-android --test syscall --release
//! ```
//!
//! `libroblox.so` calls `syscall(2)` for three things this layer has had to answer, and every one
//! of them was found by a run rather than predicted: `rt_sigprocmask` as a pointer-readability
//! probe (M3), `gettid` from a thread holding a recursive mutex (M3), and **`futex`** from the
//! engine's own worker threads (M5, D29). The first two have their tests in `tests/bionic.rs`
//! with the rest of `procenv`; this file is the third, because a futex test needs two threads and
//! a shared word, which is a different shape from everything there.
//!
//! # What these tests are careful about
//!
//! A futex is the lost-wake class, and this project has measured one at **1.0104 s** already
//! (`VERIFICATION.md` entry 11). So the tests here assert on **values and on the word**, never on
//! how long something took, and the one test that needs two threads to interleave waits for the
//! witness rather than sleeping a duration chosen to be "long enough".

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::{Bionic, ThreadHost};
use omni_android::{AbiError, Boundary};
use omni_cpu::ExitReason;

/// `futex`, in the asm-generic numbering arm64 Linux uses.
const SYS_FUTEX: u64 = 98;

const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_BITSET_MATCH_ANY: u64 = 0xffff_ffff;

/// `EAGAIN`, `ETIMEDOUT`, `EINVAL` and `EFAULT` as the guest's own `errno` must carry them.
const EAGAIN: u64 = 11;
const EINVAL: u64 = 22;
const ETIMEDOUT: u64 = 110;
const EFAULT: u64 = 14;

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
    bionic.set_log_to_stderr(false);
    let backend: Arc<dyn omni_cpu::GuestCpuBackend> = Arc::clone(&guest.backend) as _;
    bionic.set_thread_host(ThreadHost::new(backend)).expect("a thread host");
    let boundary = builder.finish();
    Fixture { guest, bionic, boundary }
}

impl Fixture {
    fn thunk(&self, symbol: &str) -> omni_cpu::GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    fn run(&self, entry: omni_cpu::GuestAddr) -> Result<ExitReason, AbiError> {
        let _active = self.bionic.activate().expect("publish the instance");
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    /// `syscall(98, uaddr, op, val, timeout, 0, val3)`, storing the return at `data` and the
    /// guest's `errno` at `data + 8`.
    ///
    /// **Both**, because the answers this call gives are a pair: `-1` alone does not say whether
    /// the word had changed (`EAGAIN`) or the wait expired (`ETIMEDOUT`), and those are the two
    /// outcomes a futex caller branches on.
    fn futex(&self, uaddr: u64, op: u64, val: u64, timeout: u64, val3: u64) -> (i64, u64) {
        let out = self.guest.data;
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, SYS_FUTEX);
        asm.mov(1, uaddr);
        asm.mov(2, op);
        asm.mov(3, val);
        asm.mov(4, timeout);
        asm.mov(5, 0);
        asm.mov(6, val3);
        asm.bl(self.thunk("syscall"));
        asm.mov(22, out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.bl(self.thunk("__errno"));
        asm.push(ldr_w(1, 0, 0));
        asm.push(str_imm(1, 22, 8));
        asm.push(ret(21));
        self.guest.load(asm.words());
        let exit = self.run(entry).expect("the run must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        (self.guest.read_u64(out) as i64, self.guest.read_u64(out + 8))
    }

    /// The refusal a `syscall(98, ..)` produced.
    fn futex_refusal(&self, uaddr: u64, op: u64, val: u64, val3: u64) -> AbiError {
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, SYS_FUTEX);
        asm.mov(1, uaddr);
        asm.mov(2, op);
        asm.mov(3, val);
        asm.mov(4, 0);
        asm.mov(5, 0);
        asm.mov(6, val3);
        asm.bl(self.thunk("syscall"));
        asm.push(ret(21));
        self.guest.load(asm.words());
        match self.run(entry) {
            Err(error) => error,
            Ok(exit) => panic!("the call completed with {exit:?} where a refusal was required"),
        }
    }
}

/// Write a `struct timespec` into guest memory and return its address.
fn timespec(f: &Fixture, at: omni_cpu::GuestAddr, seconds: i64, nanos: i64) -> u64 {
    f.guest.write_u64(at, seconds as u64);
    f.guest.write_u64(at + 8, nanos as u64);
    at as u64
}

// =================================================================== the comparison

/// **`FUTEX_WAIT` returns `EAGAIN` without sleeping when the word has already changed.**
///
/// This is the whole of what makes a futex a futex, and it is the half `Futex::wait` deliberately
/// does not perform for `omni-bionic`'s own callers (see `runtime`'s module docs for why those two
/// are different questions). A waiter that skipped the comparison would park here, and nothing
/// would ever wake it — the word it is waiting for has already reached the value it wanted.
///
/// **Asserted on the pair, and with a timeout, and both of those are deliberate.**
///
/// `-1` alone cannot tell `EAGAIN` from `ETIMEDOUT`, so the errno is what carries the claim: a
/// futex that skipped the comparison would park and then report `ETIMEDOUT`, and this asserts it
/// reported `EAGAIN` instead.
///
/// The **timeout** is there so that a broken comparison *fails* rather than *hangs*. With a null
/// timeout this call parks for ever, and a mutation row that removes the comparison would turn
/// this test into a run that never finishes — which is not a red test, it is a stuck suite. This
/// project has paid for that distinction twice (M3 task 2 had two rows that hung instead of
/// failing), and the remedy is the one used for the sleep cap: make the failing shape reachable.
#[test]
fn a_wait_whose_word_has_already_changed_is_eagain_rather_than_a_timeout() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 7);
    let short = timespec(&f, f.guest.data + 0x200, 0, 50_000_000);

    // Expecting 7 with the word at 7 would park, so this test asks for a value it does not hold.
    let (returned, errno) = f.futex(word as u64, FUTEX_WAIT, 99, short, 0);
    assert_eq!(returned, -1);
    assert_eq!(
        errno, EAGAIN,
        "the word holds 7 and the caller expected 99, so Linux answers EAGAIN without sleeping;          ETIMEDOUT here would mean it parked, which is a futex that did not compare"
    );

    // And the private flag changes nothing: in one process there is no other process to share
    // with, so private and shared name the same set of waiters.
    let (returned, errno) = f.futex(word as u64, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 99, short, 0);
    assert_eq!((returned, errno), (-1, EAGAIN), "FUTEX_PRIVATE_FLAG is accepted and ignored");
}

/// **A wait with a timeout and a matching word really waits, and reports `ETIMEDOUT`.**
///
/// The pairing with the test above is the point: the same call shape, the same `-1`, and a
/// *different* errno — which is what distinguishes "the word had changed" from "nothing woke me".
/// A futex that ignored the comparison would answer `ETIMEDOUT` to both.
#[test]
fn a_wait_whose_word_matches_sleeps_and_reports_etimedout() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 7);
    let deadline = timespec(&f, f.guest.data + 0x200, 0, 20_000_000);

    let (returned, errno) = f.futex(word as u64, FUTEX_WAIT, 7, deadline, 0);
    assert_eq!(returned, -1);
    assert_eq!(errno, ETIMEDOUT, "the word matched, so it parked, and nothing woke it");
}

/// **A wake with no waiters wakes nobody, and says so.**
///
/// `FUTEX_WAKE` returns *how many* it woke, and zero is a real answer rather than a failure. A
/// layer that returned `1` unconditionally would be telling every caller its wake was delivered.
#[test]
fn a_wake_with_no_waiters_reports_zero() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 0);
    let (returned, _errno) = f.futex(word as u64, FUTEX_WAKE, 1, 0, 0);
    assert_eq!(returned, 0, "nothing was parked on that address");
}

// =================================================================== two threads

/// **A guest thread parked in `FUTEX_WAIT` is woken by another guest thread's `FUTEX_WAKE`.**
///
/// The end-to-end shape, and the one the engine's own workers need. The assertion is on the
/// **value the waiter got back** and on the wake's own count, not on timing: if the wake were
/// lost, the waiter would sit there and the test would fail by timing out rather than by
/// reporting a wrong number, which is why the main thread waits for the witness before waking.
#[test]
fn a_guest_thread_parked_on_a_futex_is_woken_by_another_guest_thread() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    let waiter_out = f.guest.data + 0x180;
    let handle = f.guest.data + 0x1c0;
    f.guest.write_u64(word, 7);
    f.guest.write_u64(waiter_out, 0x5A5A_5A5A_5A5A_5A5A);

    // The waiter: `syscall(98, word, FUTEX_WAIT, 7, NULL, 0, 0)`, then store what it got.
    let waiter = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, SYS_FUTEX);
        asm.mov(1, word as u64);
        asm.mov(2, FUTEX_WAIT);
        asm.mov(3, 7);
        asm.mov(4, 0); // no timeout: this must be woken, not time out
        asm.mov(5, 0);
        asm.mov(6, 0);
        asm.bl(f.thunk("syscall"));
        asm.mov(22, waiter_out as u64);
        asm.push(str_imm(0, 22, 0));
        asm.mov(0, 0);
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };
    // Every program is assembled before any guest thread runs: `Guest::load` reprotects the whole
    // code region, and doing that under a running guest thread faults it.
    let create = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, handle as u64);
        asm.mov(1, 0);
        asm.mov(2, waiter as u64);
        asm.mov(3, 0);
        asm.bl(f.thunk("pthread_create"));
        asm.mov(22, f.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };
    let wake = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(0, SYS_FUTEX);
        asm.mov(1, word as u64);
        asm.mov(2, FUTEX_WAKE);
        asm.mov(3, 1);
        asm.mov(4, 0);
        asm.mov(5, 0);
        asm.mov(6, 0);
        asm.bl(f.thunk("syscall"));
        asm.mov(22, f.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };
    let join = {
        let entry = f.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.mov(22, handle as u64);
        asm.push(ldr_imm(0, 22, 0));
        asm.mov(1, 0);
        asm.bl(f.thunk("pthread_join"));
        asm.mov(22, f.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        f.guest.load(asm.words());
        entry
    };

    {
        let _active = f.bionic.activate().expect("a thread block");
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(
            f.boundary.run(&mut cpu, create, BUDGET).expect("the create completes"),
            ExitReason::Returned { .. }
        ));
    }
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_create must succeed");

    // **Wait for the witness, not for a duration.** `AddressFutex::parked_on` is the address of
    // the most recent park; a sleep chosen to be "long enough" is `VERIFICATION.md` entry 6.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while f.bionic.futex().parked_on() != word as u64 {
        assert!(
            std::time::Instant::now() < deadline,
            "no guest thread parked on {word:#x} in 30 s; futex activity is {:?}, thread \
             failures {:?}",
            f.bionic.futex().activity(),
            f.bionic.guest_thread_failures()
        );
        std::thread::yield_now();
    }

    {
        let _active = f.bionic.activate().expect("a thread block");
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(
            f.boundary.run(&mut cpu, wake, BUDGET).expect("the wake completes"),
            ExitReason::Returned { .. }
        ));
    }
    assert_eq!(
        f.guest.read_u64(f.guest.data) as i64,
        1,
        "FUTEX_WAKE reports how many it woke, and there was exactly one"
    );

    {
        let _active = f.bionic.activate().expect("a thread block");
        let mut cpu = f.guest.thread(&f.boundary);
        assert!(matches!(
            f.boundary.run(&mut cpu, join, BUDGET).expect("the join completes"),
            ExitReason::Returned { .. }
        ));
    }
    assert_eq!(f.guest.read_u64(f.guest.data), 0, "pthread_join must succeed");
    assert_eq!(
        f.guest.read_u64(waiter_out) as i64,
        0,
        "a woken FUTEX_WAIT returns 0; -1 would mean it timed out or compared unequal"
    );
    assert!(f.bionic.guest_thread_failures().is_empty(), "the waiter must not have died");
}

// =================================================================== what it refuses

/// **The operations this futex cannot perform refuse by name**, and the refusal says what the
/// parking lot does not have.
#[test]
fn the_operations_this_futex_cannot_perform_refuse_by_name() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 0);

    // Membership, not a count: each named, with the reason it cannot be answered.
    for (op, name) in [(3u64, "FUTEX_REQUEUE"), (4, "FUTEX_CMP_REQUEUE"), (5, "FUTEX_WAKE_OP"),
                       (6, "FUTEX_LOCK_PI"), (7, "FUTEX_UNLOCK_PI"), (8, "FUTEX_TRYLOCK_PI")] {
        let error = f.futex_refusal(word as u64, op, 0, 0);
        assert_eq!(error.symbol(), Some("syscall"));
        assert!(
            error.to_string().contains(name),
            "the refusal must name the operation `{name}`: {error}"
        );
    }
}

/// **A partial bitset refuses rather than being treated as `MATCH_ANY`.**
///
/// The believable wrong answer *works* for the common case and silently wakes waiters the caller
/// deliberately excluded. bionic's own `__futex_wait_ex` passes `FUTEX_BITSET_MATCH_ANY`, so the
/// path the engine takes is not refused — which the second half of this test asserts, because a
/// refusal that also refused the expected case would be a regression dressed as strictness.
#[test]
fn a_partial_bitset_refuses_and_match_any_does_not() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 7);

    let error = f.futex_refusal(word as u64, FUTEX_WAIT_BITSET, 7, 0x0000_0001);
    assert!(error.to_string().contains("MATCH_ANY"), "{error}");
    let error = f.futex_refusal(word as u64, FUTEX_WAKE_BITSET, 1, 0x0000_0001);
    assert!(error.to_string().contains("MATCH_ANY"), "{error}");

    // `MATCH_ANY` is the path bionic takes, and it must work. The word does not match, so this
    // answers EAGAIN rather than parking — which is also the cheapest way to prove it got past
    // the bitset check.
    // A timeout for the same reason as above: a comparison that was removed must fail this
    // test rather than hang it.
    let short = timespec(&f, f.guest.data + 0x200, 0, 50_000_000);
    let (returned, errno) = f.futex(
        word as u64,
        FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG,
        99,
        short,
        FUTEX_BITSET_MATCH_ANY,
    );
    assert_eq!((returned, errno), (-1, EAGAIN));
    let (returned, _errno) =
        f.futex(word as u64, FUTEX_WAKE_BITSET | FUTEX_PRIVATE_FLAG, 1, 0, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(returned, 0, "nothing is parked, so nothing is woken");
}

/// **The kernel's own argument validation**, each answer a branch guest code has.
#[test]
fn the_arguments_are_validated_the_way_the_kernel_validates_them() {
    let _guard = serialized();
    let f = fixture();
    let word = f.guest.data + 0x100;
    f.guest.write_u64(word, 7);

    // An unaligned `uaddr`: EINVAL. A 32-bit word that is not 4-byte aligned has no atomic load.
    let (returned, errno) = f.futex(word as u64 + 1, FUTEX_WAIT, 7, 0, 0);
    assert_eq!((returned, errno), (-1, EINVAL), "an unaligned futex word is EINVAL");

    // A word that is not mapped: EFAULT, **not** a thread parked on an address nothing can wake.
    let (returned, errno) = f.futex(f.guest.unmapped as u64 & !7, FUTEX_WAIT, 7, 0, 0);
    assert_eq!((returned, errno), (-1, EFAULT), "an unreadable futex word is EFAULT");

    // A `tv_nsec` outside [0, 1e9): EINVAL, which is the kernel's own check.
    let bad = timespec(&f, f.guest.data + 0x200, 0, 1_000_000_000);
    let (returned, errno) = f.futex(word as u64, FUTEX_WAIT, 7, bad, 0);
    assert_eq!((returned, errno), (-1, EINVAL), "tv_nsec must be under a billion");

    // A negative `tv_sec` on a *relative* wait: EINVAL.
    let negative = timespec(&f, f.guest.data + 0x200, -1, 0);
    let (returned, errno) = f.futex(word as u64, FUTEX_WAIT, 7, negative, 0);
    assert_eq!((returned, errno), (-1, EINVAL), "a negative relative timeout is EINVAL");

    // **An absolute deadline already in the past is an immediate timeout, not an error and not
    // an indefinite wait.** `FUTEX_WAIT_BITSET`'s timeout is absolute, and treating a past
    // deadline as "no timeout" would park for ever on a call that asked not to.
    let past = timespec(&f, f.guest.data + 0x200, 1, 0);
    let (returned, errno) =
        f.futex(word as u64, FUTEX_WAIT_BITSET, 7, past, FUTEX_BITSET_MATCH_ANY);
    assert_eq!(
        (returned, errno),
        (-1, ETIMEDOUT),
        "one second after the monotonic epoch is long past, so this times out at once"
    );
}
