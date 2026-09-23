//! **Guest exclusive loads and stores under both [`ExclusiveMonitor`] arms: no increment is lost,
//! and the one behaviour that differs is pinned.**
//!
//! D31 moves the runtime from dynarmic's global exclusive monitor to value-compare. A faster
//! runtime that loses an atomic increment would be a new corruption class, silent and rare, so this
//! suite is the evidence the decision rests on, in three parts:
//!
//! 1. **Many threads, exact totals.** Sixteen host threads, each running its own guest context of
//!    one backend, increment one shared 64-bit word with an `LDAXR`/`ADD`/`STLXR`/`CBNZ` loop and a
//!    shared 128-bit pair with `LDAXP`/`STLXP` -- the shapes LLVM emits for a C++ `fetch_add`
//!    and for a 16-byte atomic -- and the totals must be exact under **both** arms. The loops also
//!    count their own failed store-exclusives, and the test asserts that some failed: a run whose
//!    threads never overlapped would pass while proving nothing (`docs/VERIFICATION.md` entry 11).
//! 2. **The count can see a loss.** The same harness with a plain `LDR`/`ADD`/`STR` loses
//!    increments; if it ever stopped doing so, part 1 would no longer be a detector.
//! 3. **ABA across an exclusive store, the only difference.** One context holds a reservation;
//!    another changes the word with exclusive stores and changes it back. The global monitor fails
//!    the first context's store-exclusive (the other processor's exclusive store cleared its
//!    reservation); value-compare lets it succeed, because the word holds the reserved value. Both
//!    fail when the value really changed, and both succeed when nothing happened.

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicCpu, DynarmicOptions, ExclusiveMonitor};
use omni_cpu::{ExitReason, GuestAddr, GuestCpu, RunLimit};

/// Guest threads in the stress test.
const THREADS: usize = 16;
/// Loop iterations per thread: each is one exclusive increment of the word and one of the pair.
const ITERATIONS: u64 = 20_000;
/// The runtime's own thread capacity, so the global arm is measured at the size the gate runs it.
const MAX_THREADS: u32 = 256;

/// `x0` = the word, `x1` = the 16-byte pair, `x9` = iterations. Leaves in `x10` how many
/// store-exclusives failed and were retried.
///
/// ```text
/// word:  ldaxr x2, [x0] ; add x2, x2, #1 ; stlxr w3, x2, [x0] ; cbnz w3, retry_word
/// pair:  ldaxp x4, x5, [x1] ; add x4, x4, #1 ; add x5, x5, #2 ; stlxp w3, x4, x5, [x1] ; cbnz w3, retry_pair
///        subs x9, x9, #1 ; b.ne word ; ret
/// retry_word: add x10, x10, #1 ; b word
/// retry_pair: add x10, x10, #1 ; b pair
/// ```
fn exclusive_increments() -> Vec<u32> {
    let mut p = vec![movz(10, 0, 0)];
    let word = p.len();
    p.push(ldaxr(2, 0));
    p.push(add_imm(2, 2, 1));
    p.push(stlxr(3, 2, 0));
    let word_cbnz = p.len();
    p.push(0); // patched below
    let pair = p.len();
    p.push(ldaxp(4, 5, 1));
    p.push(add_imm(4, 4, 1));
    p.push(add_imm(5, 5, 2));
    p.push(stlxp(3, 4, 5, 1));
    let pair_cbnz = p.len();
    p.push(0); // patched below
    p.push(subs_imm(9, 9, 1));
    let here = p.len();
    p.push(b_cond(1, word as i32 - here as i32));
    p.push(ret(30));
    let retry_word = p.len();
    p.push(add_imm(10, 10, 1));
    let here = p.len();
    p.push(b(word as i32 - here as i32));
    let retry_pair = p.len();
    p.push(add_imm(10, 10, 1));
    let here = p.len();
    p.push(b(pair as i32 - here as i32));
    p[word_cbnz] = cbnz_w(3, retry_word as i32 - word_cbnz as i32);
    p[pair_cbnz] = cbnz_w(3, retry_pair as i32 - pair_cbnz as i32);
    p
}

/// The negative control: `x0` = the word, `x9` = iterations, and no atomicity at all.
fn plain_increments() -> Vec<u32> {
    let mut p = Vec::new();
    let top = p.len();
    p.push(ldr_imm(2, 0, 0));
    p.push(add_imm(2, 2, 1));
    p.push(str_imm(2, 0, 0));
    p.push(subs_imm(9, 9, 1));
    let here = p.len();
    p.push(b_cond(1, top as i32 - here as i32));
    p.push(ret(30));
    p
}

fn guest_for(monitor: ExclusiveMonitor) -> Guest {
    Guest::with_options(DynarmicOptions {
        max_threads: MAX_THREADS,
        exclusive_monitor: monitor,
        ..Default::default()
    })
}

/// Run `entry` on `THREADS` contexts at once, released together, and return each one's `x10`.
fn run_concurrently(guest: &Guest, entry: GuestAddr, setup: impl Fn(&mut DynarmicCpu)) -> Vec<u64> {
    let sentinel = guest.code + harness::CODE_BYTES - 4;
    let mut cpus: Vec<DynarmicCpu> = (0..THREADS)
        .map(|_| {
            let mut cpu = guest.backend.create_thread_with_tls().expect("a guest thread");
            cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
            cpu.set_x(x(30), sentinel as u64);
            setup(&mut cpu);
            cpu
        })
        .collect();
    let start = std::sync::Barrier::new(THREADS);
    std::thread::scope(|scope| {
        let handles: Vec<_> = cpus
            .iter_mut()
            .map(|cpu| {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    let exit = cpu.run(entry, RunLimit::Unlimited).expect("the loop runs");
                    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
                    cpu.x(x(10))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("a guest thread panicked")).collect()
    })
}

fn stress(monitor: ExclusiveMonitor) {
    let guest = guest_for(monitor);
    let entry = guest.load(&exclusive_increments());
    let word = guest.data;
    let pair = guest.data + 64;
    guest.write_u64(word, 0);
    guest.write_u64(pair, 0);
    guest.write_u64(pair + 8, 0);
    let retries = run_concurrently(&guest, entry, |cpu| {
        cpu.set_x(x(0), word as u64);
        cpu.set_x(x(1), pair as u64);
        cpu.set_x(x(9), ITERATIONS);
    });
    let expected = THREADS as u64 * ITERATIONS;
    assert_eq!(guest.read_u64(word), expected, "{monitor:?}: the 64-bit word lost increments");
    assert_eq!(guest.read_u64(pair), expected, "{monitor:?}: the pair's low half lost increments");
    assert_eq!(
        guest.read_u64(pair + 8),
        2 * expected,
        "{monitor:?}: the pair's high half lost increments"
    );
    let failed: u64 = retries.iter().sum();
    assert!(
        failed > 0,
        "{monitor:?}: not one store-exclusive failed across {THREADS} threads, so the threads never \
         contended and the exact totals above prove nothing about atomicity"
    );
    println!(
        "{monitor:?}: {} increments of each, exact; {failed} store-exclusives failed and were retried",
        expected
    );
}

#[test]
fn no_increment_is_lost_under_the_global_monitor() {
    stress(ExclusiveMonitor::Global);
}

#[test]
fn no_increment_is_lost_under_value_compare() {
    stress(ExclusiveMonitor::ValueCompare);
}

/// Part 2: the count can see a loss. Up to five attempts, because a lost update is a race; the
/// test fails only if **none** of them lost one.
#[test]
fn plain_increments_lose_updates_so_the_exact_count_is_a_detector() {
    let guest = guest_for(ExclusiveMonitor::ValueCompare);
    let entry = guest.load(&plain_increments());
    let word = guest.data;
    let expected = THREADS as u64 * ITERATIONS * 10;
    for attempt in 0..5 {
        guest.write_u64(word, 0);
        run_concurrently(&guest, entry, |cpu| {
            cpu.set_x(x(0), word as u64);
            cpu.set_x(x(9), ITERATIONS * 10);
        });
        let got = guest.read_u64(word);
        assert!(got <= expected);
        if got < expected {
            println!("attempt {attempt}: {got} of {expected} -- a plain increment loop loses updates");
            return;
        }
    }
    panic!(
        "five runs of {THREADS} threads doing unsynchronised increments lost nothing, so this host \
         did not interleave them and the stress tests cannot be detecting a lost update either"
    );
}

/// The program for part 3: `ldaxr x2, [x0]` at `+0`, `stlxr w3, x2, [x0]` at `+4` (where the
/// breakpoint goes), then `mov x0, x3 ; ret` -- so `x0` comes back as the store-exclusive's
/// status: 0 stored, 1 failed.
fn held_reservation() -> Vec<u32> {
    vec![ldaxr(2, 0), stlxr(3, 2, 0), mov_reg(0, 3), ret(30)]
}

/// `x0` = the word, `x1` = what to add (two's complement), one exclusive read-modify-write.
fn exclusive_add() -> Vec<u32> {
    let mut p = Vec::new();
    let top = p.len();
    p.push(ldaxr(2, 0));
    p.push(add_reg(2, 2, 1));
    p.push(stlxr(3, 2, 0));
    let here = p.len();
    p.push(cbnz_w(3, top as i32 - here as i32));
    p.push(ret(30));
    p
}

/// What `held_reservation`'s store-exclusive answers after `interfere` ran while it held its
/// reservation.
fn held_store_status(monitor: ExclusiveMonitor, interfere: &[u64]) -> (u64, u64) {
    let guest = guest_for(monitor);
    let holder_entry = guest.load(&held_reservation());
    let adder_entry = guest.load_at(0x100, &exclusive_add());
    let word = guest.data;
    guest.write_u64(word, 40);
    let sentinel = guest.code + harness::CODE_BYTES - 4;

    let (mut holder, _) = guest.thread();
    holder.add_breakpoint(holder_entry + 4).expect("a breakpoint at the store-exclusive");
    holder.set_x(x(0), word as u64);
    let exit = holder.run(holder_entry, RunLimit::Unlimited).expect("the holder runs");
    assert_eq!(exit, ExitReason::Breakpoint { pc: holder_entry + 4 }, "{exit}");

    let (mut adder, _) = guest.thread();
    for &delta in interfere {
        adder.set_x(x(0), word as u64);
        adder.set_x(x(1), delta);
        adder.set_x(x(30), sentinel as u64);
        let exit = adder.run(adder_entry, RunLimit::Unlimited).expect("the other context runs");
        assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    }

    let exit = holder.run(holder_entry + 4, RunLimit::Unlimited).expect("the holder resumes");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    (holder.x(x(0)), guest.read_u64(word))
}

/// Part 3. Four cases per arm; the table is the whole of the behavioural difference D31 accepts.
#[test]
fn aba_across_another_threads_exclusive_store_is_the_one_difference() {
    const UP: u64 = 1;
    const DOWN: u64 = u64::MAX; // -1
    for monitor in [ExclusiveMonitor::Global, ExclusiveMonitor::ValueCompare] {
        // Nothing happened in between: both arms store (status 0), and the word is what the
        // holder stored -- the value it read, 40.
        assert_eq!(held_store_status(monitor, &[]), (0, 40), "{monitor:?}, no interference");
        // The value really changed: both arms refuse, and the other thread's increment stands.
        assert_eq!(held_store_status(monitor, &[UP]), (1, 41), "{monitor:?}, a real change");
    }
    // ABA -- changed and changed back by exclusive stores: the global monitor's scan cleared the
    // holder's reservation, so it refuses; value-compare sees the reserved value and stores. The
    // word ends at 40 either way, which is why a compiler-generated read-modify-write loop cannot
    // tell the difference: what it stores depends only on what it read, and that is unchanged.
    assert_eq!(held_store_status(ExclusiveMonitor::Global, &[UP, DOWN]), (1, 40), "global, ABA");
    assert_eq!(
        held_store_status(ExclusiveMonitor::ValueCompare, &[UP, DOWN]),
        (0, 40),
        "value-compare, ABA"
    );
}
