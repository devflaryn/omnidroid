//! **The thunk round trip**: what it costs for the guest to call out of its world and come back,
//! measured two ways, because the two ways are M3's actual design decision.
//!
//! The only cost figure on the branch before this was "under 53 ns", and it is an **entry** ceiling
//! (D5 amendment 2) — what one `od_jit_run` call costs, explicitly excluding the exit and the
//! re-entry. M3 needs the round trip: the guest branches into the thunk region, the host services the
//! call, the guest resumes. Every one of `libroblox.so`'s imported symbols crosses this boundary, and
//! it is crossed from all 3,594 static initializers before a frame is ever drawn.
//!
//! Two shapes, and the measurement decides between them:
//!
//! * **Exit to Rust per call.** [`ExitReason::Thunk`] comes back out of [`GuestCpu::run`], the caller
//!   services the symbol and calls `run` again at the link register. Simple, and it pays a full
//!   unwind of the generated frame plus a full re-entry every time.
//! * **Dispatch inside the run loop.** `DynarmicCpu::add_inline_thunk` services the call in the `SVC`
//!   callback and writes the guest `PC`, and the backend's own dispatcher loop picks the guest back
//!   up. Nothing returns to Rust at all.
//!
//! Two further cells measure the shape the *loader* really produces — a call to a PLT stub whose GOT
//! slot was bound to the thunk address — because that adds four guest instructions and, decisively,
//! an indirect terminal.
//!
//! The measurements are `#[ignore]`d; the correctness and characterisation tests are not, because
//! design B's whole claim is that `run` is entered **once**, and that is a property a test can pin
//! rather than a number a benchmark reports.
//!
//! ```text
//! cargo test -p omni-cpu --release --test thunk -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(all(target_arch = "x86_64", feature = "dynarmic"))]

mod harness;

use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::{DynarmicCpu, InlineThunkCall};
use omni_cpu::{ExitReason, GuestAddr, GuestCpu, GuestCpuBackend, RunLimit};

/// Samples per configuration. Odd, so the median is an observation and not an average of two.
const N: usize = 31;

/// Guest calls per timed sample.
///
/// Large enough that one sample is milliseconds rather than microseconds — so `Instant`'s resolution
/// is irrelevant — and small enough that the whole matrix runs in seconds.
const CALLS: u64 = 100_000;

/// Guest calls for the correctness tests, which assert counts rather than time them.
const CHECK_CALLS: u64 = 1_000;

/// Layout inside the 64 KiB guest code region. Distinct offsets, so loading one program never
/// overwrites another whose context is still alive.
const BASELINE_AT: usize = 0x0000;
const DIRECT_AT: usize = 0x0400;
const VIA_STUB_AT: usize = 0x0800;
/// The thunk itself. Inside the code region on purpose: the real loader gives imported symbols a
/// reserved region (`ARCHITECTURE.md` section 5), but what the backend sees is a branch to an address
/// it was told about, and `read_code` plants the stop there whatever guest memory holds.
const THUNK_AT: usize = 0x2000;
const STUB_AT: usize = 0x4000;

/// Serializes the measurements. A timing taken while a sibling runs measures the scheduler.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------- encodings

/// `BL offset` — `100101 imm26`. Offset in instructions.
const fn bl(offset_insns: i32) -> u32 {
    0x9400_0000 | ((offset_insns as u32) & 0x03FF_FFFF)
}

/// `ADRP Xd, #pages` — `1 immlo:2 10000 immhi:19 Rd:5`, `pages` being the signed 4 KiB page delta.
const fn adrp(rd: u32, pages: i32) -> u32 {
    let imm = (pages as u32) & 0x1F_FFFF;
    0x9000_0000 | ((imm & 3) << 29) | ((imm >> 2) << 5) | rd
}

// ------------------------------------------------------------------------------- the programs

/// A loop that calls `target` `calls` times and then returns to the sentinel.
///
/// ```text
///     MOV  X21, X30        ; the sentinel return address, which BL is about to clobber
///     MOV  X19, #calls
/// loop:
///     BL   target          ; or NOP, when there is no target
///     SUBS X19, X19, #1
///     B.NE loop
///     RET  X21
/// ```
///
/// Offsets are byte offsets into the guest code region, so the `BL` displacement is computed from
/// both rather than assuming the program starts at zero. `X19`/`X21` rather than low registers, so a
/// handler marshalling `X0`-`X7` cannot disturb the loop — the difference between measuring a marshal
/// and measuring a corrupted loop.
fn call_loop(calls: u64, program_at: usize, target: Option<usize>) -> Vec<u32> {
    let mut program = vec![mov_reg(21, 30)];
    program.extend(mov64(19, calls));
    let loop_start = program.len();
    program.push(match target {
        Some(target) => {
            let site = (program_at + 4 * program.len()) as i64;
            bl(((target as i64 - site) / 4) as i32)
        }
        None => NOP,
    });
    program.push(subs_imm(19, 19, 1));
    let here = program.len();
    program.push(b_cond(1, loop_start as i32 - here as i32));
    program.push(ret(21));
    program
}

/// The four instructions of an AArch64 PLT stub, as `lld` emits them:
///
/// ```text
///     ADRP X16, got_page
///     LDR  X17, [X16, #got_offset]
///     ADD  X16, X16, #got_offset
///     BR   X17
/// ```
///
/// Built by hand because it is the shape the real loader produces, and it is **not** free: the guest
/// spends four more instructions and, decisively, leaves the block through `BR` — an *indirect*
/// terminal, which under `optimization::INTERRUPTIBLE` is a dispatcher round trip rather than a
/// linked jump. A `BL` straight into the thunk region leaves through `LinkBlock` instead. That is
/// what these cells measure, and it decides whether the runtime keeps the stub in the loop or binds
/// the call sites past it.
fn plt_stub(stub_at: GuestAddr, got_slot: GuestAddr) -> Vec<u32> {
    let pages = ((got_slot & !0xFFF) as i64 - (stub_at & !0xFFF) as i64) / 4096;
    let offset = (got_slot & 0xFFF) as u32;
    vec![adrp(16, pages as i32), ldr_imm(17, 16, offset), add_imm(16, 16, offset), br(17)]
}

// ------------------------------------------------------------------------------- the handlers

/// The cheapest possible host service: nothing at all.
///
/// This is the *boundary* and nothing else, which is what makes it the right thing to compare
/// against. A real imported symbol adds its own body on top.
fn nothing(_call: &mut InlineThunkCall<'_>) {}

/// `STMXCSR` into a `u32`. `_mm_getcsr` is deprecated in favour of exactly this.
fn read_mxcsr() -> u32 {
    let mut out: u32 = 0;
    // SAFETY: SSE2 is baseline on x86-64 and this file is `cfg(target_arch = "x86_64")`.
    // `stmxcsr` writes four bytes to a `u32` this frame owns.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut out, options(nostack)) };
    out
}

/// A representative AAPCS64 marshal: read the eight integer argument registers, combine them so the
/// optimizer cannot delete the reads, write the result register.
///
/// Eight and one because that is AAPCS64's integer argument set plus its return register. The cost is
/// the same nine `JitState` accesses in either design, so reporting it separately keeps the comparison
/// about *dispatch* and hands the marshal cost to task 2 as a number of its own.
fn marshal_eight_arguments(call: &mut InlineThunkCall<'_>) {
    let mut sum = 0u64;
    for i in 0..8 {
        sum = sum.wrapping_add(call.x(i));
    }
    call.set_x(0, sum);
}

/// What [`marshal_eight_arguments`] leaves in `X0` after `calls` calls, from
/// `X0 = 1, X1 = 2, … X7 = 128`.
///
/// The first call writes 255; every later one adds 254, because `X0` is both an input and the output.
/// A closed form rather than a shape, because it is what shows every marshal ran and none ran twice.
const fn marshal_result(calls: u64) -> u64 {
    255 + (calls - 1) * 254
}

fn arm_arguments(cpu: &mut DynarmicCpu) {
    for i in 0..8 {
        cpu.set_x(x(i), 1 << i);
    }
}

// --------------------------------------------------------------------------------- the drivers

/// Put the sentinel back in `X30` before a run.
///
/// **Not optional, and the reason is a bug this file had for one revision.** The loop's first
/// instruction is `MOV X21, X30`, so it copies whatever `X30` holds *at entry* into the register it
/// finally returns through. `BL` clobbers `X30` with the return address, so after one pass `X30`
/// points into the middle of the loop — and a second pass therefore `RET`s into `SUBS X19, X19, #1`
/// with `X19` at zero, which clears `Z`, takes the `B.NE`, and loops 2^64 times. The first sample
/// measured correctly and the second hung forever, which is exactly the shape of failure a warm-up
/// pass followed by a timing loop invites. One `set_x`, on every cell including the baseline, so it
/// subtracts out.
fn rearm(cpu: &mut DynarmicCpu, sentinel: GuestAddr) {
    cpu.set_x(x(30), sentinel as u64);
}

/// Design A: every call leaves the backend and comes back out of `run`.
///
/// Returns how many thunk exits were serviced, so a caller can prove the path ran.
fn run_exiting(cpu: &mut DynarmicCpu, entry: GuestAddr, sentinel: GuestAddr, marshal: bool) -> u64 {
    rearm(cpu, sentinel);
    let mut serviced = 0u64;
    let mut exit = cpu.run(entry, RunLimit::Unlimited).expect("the first slice");
    loop {
        match exit {
            ExitReason::Thunk { .. } => {
                serviced += 1;
                if marshal {
                    let mut sum = 0u64;
                    for i in 0..8 {
                        sum = sum.wrapping_add(cpu.x(x(i)));
                    }
                    cpu.set_x(x(0), sum);
                }
                // The guest resumes where the `BL` said it would.
                let resume = cpu.x(x(30)) as GuestAddr;
                exit = cpu.run(resume, RunLimit::Unlimited).expect("a resumed slice");
            }
            ExitReason::Returned { .. } => return serviced,
            other => panic!("unexpected exit {other}"),
        }
    }
}

/// Design B: one `run`, with the calls serviced inside it.
///
/// Returns how many times `run` returned, which is the claim being made: **one**.
fn run_inline(cpu: &mut DynarmicCpu, entry: GuestAddr, sentinel: GuestAddr) -> u64 {
    rearm(cpu, sentinel);
    let mut entries = 0u64;
    let mut from = entry;
    loop {
        entries += 1;
        match cpu.run(from, RunLimit::Unlimited).expect("the inline run") {
            ExitReason::Returned { .. } => return entries,
            ExitReason::StepLimitReached { pc, .. } => from = pc,
            other => panic!("unexpected exit {other}"),
        }
    }
}

// ------------------------------------------------------------------------------- measurement

struct Summary {
    min: Duration,
    median: Duration,
    max: Duration,
}

impl Summary {
    fn of(mut samples: Vec<Duration>) -> Self {
        samples.sort_unstable();
        Self { min: samples[0], median: samples[samples.len() / 2], max: samples[samples.len() - 1] }
    }

    /// Nanoseconds per guest call, from the median.
    fn ns_per_call(&self) -> f64 {
        self.median.as_secs_f64() * 1e9 / CALLS as f64
    }
}

fn measure(mut body: impl FnMut()) -> Summary {
    body(); // warm the translation, so the figure is steady state and not cold-translation cost
    Summary::of(
        (0..N)
            .map(|_| {
                let t = Instant::now();
                body();
                t.elapsed()
            })
            .collect(),
    )
}

fn report(label: &str, summary: &Summary, baseline: Option<&Summary>) {
    let per_call = summary.ns_per_call();
    let net = baseline
        .map(|b| format!("net {:>8.2}", per_call - b.ns_per_call()))
        .unwrap_or_else(|| " ".repeat(12));
    println!(
        "  {label:<46} {per_call:>9.2} ns/call   {net}   (n = {N}: min {:.2}, max {:.2})",
        summary.min.as_secs_f64() * 1e9 / CALLS as f64,
        summary.max.as_secs_f64() * 1e9 / CALLS as f64,
    );
}

// ------------------------------------------------------------------------------------- tests

/// **The characterisation that makes the measurement mean anything.** Inline dispatch must service
/// every call *without* `run` returning, and both designs must leave the guest in the same state.
///
/// Global Constraint 13: the cheap version of this test — time two loops and print two numbers —
/// would pass identically if `add_inline_thunk` silently fell back to the ordinary thunk exit, or if
/// it never ran at all and the guest executed a `NOP`. Both are exactly the failures that would make
/// design B's figure a lie. So the counts are asserted, not the timings.
#[test]
fn inline_dispatch_services_every_call_without_leaving_the_run_loop() {
    let _serial = serialized();
    let guest = Guest::new();
    guest.assert_high_addresses();
    let thunk = guest.code + THUNK_AT;
    let entry = guest.load_at(DIRECT_AT, &call_loop(CHECK_CALLS, DIRECT_AT, Some(THUNK_AT)));

    // Design A, for the reference answer.
    let (mut cpu, sentinel) = guest.thread();
    cpu.add_thunk(thunk).expect("a thunk");
    arm_arguments(&mut cpu);
    let serviced = run_exiting(&mut cpu, entry, sentinel, true);
    assert_eq!(serviced, CHECK_CALLS, "every call must have come out of run as a thunk exit");
    assert_eq!(
        cpu.x(x(0)),
        marshal_result(CHECK_CALLS),
        "the marshal must have run exactly {CHECK_CALLS} times"
    );
    // **`pc()` is the sentinel plus four, and that is a sharp edge rather than a bug.** A stop at a
    // planted `SVC` is raised from `call_svc`, and dynarmic sets the guest `PC` to the instruction
    // *after* the `SVC` before calling it. `ExitReason::Returned { pc }` subtracts the four and is
    // the address the runtime should read; `pc()` afterwards is not. Pinned so a caller reading
    // `pc()` after a return finds it stated rather than discovering it.
    assert_eq!(cpu.pc(), sentinel + 4, "a sentinel return leaves pc() one instruction past it");
    drop(cpu);

    // Design B: the same program, the same arguments, serviced inline.
    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, marshal_eight_arguments).expect("an inline thunk");
    arm_arguments(&mut cpu);
    let entries = run_inline(&mut cpu, entry, sentinel);
    assert_eq!(
        entries, 1,
        "inline dispatch is the whole point: {CHECK_CALLS} guest calls must have been serviced \
         inside ONE run. {entries} entries means the callback raised a halt after all, and design \
         B's timing is then measuring design A"
    );
    assert_eq!(
        cpu.inline_calls(),
        CHECK_CALLS,
        "the inline path must have run {CHECK_CALLS} times. A zero here with a passing timing is \
         the failure this assertion exists for: the loop would have been executing nothing"
    );
    assert_eq!(cpu.pc(), sentinel + 4, "as design A: pc() lands one instruction past the sentinel");
    assert_eq!(
        cpu.x(x(0)),
        marshal_result(CHECK_CALLS),
        "the two designs must leave the guest in the same state, or they are not two designs for \
         the same boundary"
    );

    // Removing it puts the address back to an ordinary exit, rather than leaving a thunk that
    // silently does nothing.
    assert!(cpu.remove_inline_thunk(thunk).expect("remove"), "it was registered");
    assert!(!cpu.remove_inline_thunk(thunk).expect("remove again"), "and now it is not");
    cpu.add_thunk(thunk).expect("an ordinary thunk at the same address");
    let before = cpu.inline_calls();
    rearm(&mut cpu, sentinel);
    match cpu.run(entry, RunLimit::Unlimited).expect("a run after removal") {
        ExitReason::Thunk { pc } => assert_eq!(pc, thunk),
        other => panic!("after removal the address must exit, got {other}"),
    }
    assert_eq!(cpu.inline_calls(), before, "a removed inline thunk must not still be serviced");
}

/// An inline thunk and an exiting thunk must coexist, because M3 wants both: a resolved import
/// dispatches inline, an unresolved one has to reach the runtime.
#[test]
fn an_inline_thunk_and_an_exiting_thunk_coexist_at_different_addresses() {
    let _serial = serialized();
    let guest = Guest::new();
    let inline_at = THUNK_AT;
    let exiting_at = THUNK_AT + 0x100;

    // MOV X21, X30 ; BL inline ; BL exiting ; RET X21
    let mut program = vec![mov_reg(21, 30)];
    program.push(bl(inline_at as i32 / 4 - program.len() as i32));
    program.push(bl(exiting_at as i32 / 4 - program.len() as i32));
    program.push(ret(21));
    let entry = guest.load_at(0, &program);

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(guest.code + inline_at, marshal_eight_arguments).expect("inline");
    cpu.add_thunk(guest.code + exiting_at).expect("exiting");
    cpu.set_x(x(0), 7);
    for i in 1..8 {
        cpu.set_x(x(i), 0);
    }

    match cpu.run(entry, RunLimit::Unlimited).expect("run") {
        ExitReason::Thunk { pc } => assert_eq!(
            pc,
            guest.code + exiting_at,
            "the first stop must be the EXITING thunk: the inline one before it must not have \
             produced an exit"
        ),
        other => panic!("expected the exiting thunk, got {other}"),
    }
    assert_eq!(cpu.inline_calls(), 1, "the inline thunk must have been serviced on the way past");
    assert_eq!(cpu.x(x(0)), 7, "and its marshal must have written X0");

    let resume = cpu.x(x(30)) as GuestAddr;
    match cpu.run(resume, RunLimit::Unlimited).expect("resume") {
        ExitReason::Returned { pc } => assert_eq!(pc, sentinel),
        other => panic!("expected the sentinel, got {other}"),
    }
}

/// An inline thunk reached through a **real PLT stub** must behave exactly as one reached by a direct
/// `BL`, because that is the path the loader actually builds.
///
/// It is a different terminal in the emitted code — `BR X17` ends the stub's block through
/// `PopRSBHint` rather than `LinkBlock` — so "the callback can write `PC` and the dispatcher picks it
/// up" has to hold for both, and this is the one that passes through `LookupBlock` twice per call
/// rather than once.
#[test]
fn inline_dispatch_works_through_a_plt_stub_bound_to_the_thunk() {
    let _serial = serialized();
    let guest = Guest::new();
    let thunk = guest.code + THUNK_AT;
    let stub = guest.code + STUB_AT;
    let got_slot = guest.data;
    guest.write_u64(got_slot, thunk as u64);
    let entry = guest.load_at(VIA_STUB_AT, &call_loop(CHECK_CALLS, VIA_STUB_AT, Some(STUB_AT)));
    guest.load_at(STUB_AT, &plt_stub(stub, got_slot));

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, marshal_eight_arguments).expect("an inline thunk");
    arm_arguments(&mut cpu);
    assert_eq!(
        run_inline(&mut cpu, entry, sentinel),
        1,
        "a call through the stub must not leave the run loop either"
    );
    assert_eq!(cpu.inline_calls(), CHECK_CALLS);
    assert_eq!(cpu.x(x(0)), marshal_result(CHECK_CALLS));
    // `X16`/`X17` are the stub's scratch registers and the guest wrote them, which is the evidence
    // that the stub really executed rather than the `BL` reaching the thunk some other way.
    assert_eq!(cpu.x(x(17)), thunk as u64, "the stub loaded the bound GOT slot into X17");
    assert_eq!(cpu.x(x(16)), got_slot as u64, "and left the slot address in X16");
}

/// **Inline dispatch must not create a guest shape the runtime cannot stop.** (Global Constraint 11.)
///
/// This is the question D16 exists to answer, asked again about a new terminal. Inline dispatch keeps
/// the guest inside `od_jit_run` across every import call, so if the `SVC` terminal reached the next
/// block without checking anything, a guest looping on an imported symbol would ignore both the step
/// budget and a cross-thread halt — the same trap `PopRSBHint` and `FastDispatchHint` set under
/// dynarmic's default flags, which is why this backend clears them.
///
/// It does not, and the reason is structural rather than lucky: `CheckHalt{PopRSBHint}` with
/// `ReturnStackBuffer` cleared lands on `ReturnFromRunCode`, which D16 identified as **the only path
/// that checks both** the halt flag and the cycle counter. So an inline thunk call is one of the few
/// points in translated code where both escapes are live. Asserted, because "the emitted code happens
/// to check" is exactly the kind of claim that stops being true on a re-pin.
#[test]
fn a_counted_budget_still_stops_a_guest_looping_through_an_inline_thunk() {
    let _serial = serialized();
    let guest = Guest::new();
    let thunk = guest.code + THUNK_AT;
    // Effectively endless: 2^40 calls is more than any budget here will reach.
    let entry = guest.load_at(DIRECT_AT, &call_loop(1 << 40, DIRECT_AT, Some(THUNK_AT)));

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, nothing).expect("an inline thunk");
    rearm(&mut cpu, sentinel);

    const BUDGET: u64 = 50_000;
    match cpu.run(entry, RunLimit::Instructions(BUDGET)) {
        Ok(ExitReason::StepLimitReached { executed, .. }) => {
            assert!(
                executed >= BUDGET,
                "the budget must be spent, not merely declared: {executed} of {BUDGET}"
            );
        }
        other => panic!("a counted budget must stop this loop, got {other:?}"),
    }
    assert!(
        cpu.inline_calls() > 0,
        "the loop must have been going through the inline thunk, or this test bounds nothing"
    );

    // And a cross-thread halt, which reaches it at the next slice boundary (that is D16's ruling:
    // the watchdog is the budget, and a halt is honoured between slices).
    let handle = cpu.halt_handle();
    handle.request();
    match cpu.run(entry, RunLimit::Unlimited) {
        Ok(ExitReason::Halted { .. }) => {}
        other => panic!("an outstanding halt must be honoured, got {other:?}"),
    }
}

/// **An inline handler runs with the GUEST's MXCSR loaded, and design A's does not.**
///
/// Found by reading `A64EmitX64::EmitA64CallSupervisor`, which calls `Devirtualize<CallSVC>::EmitCall`
/// **without** a preceding `code.SwitchMxcsrOnExit()` — the only terminal in the A64 emitter that
/// does switch is `IR::Term::Interpret`. `BlockOfCode::GenRunCode` loads `guest_MXCSR` on entry and
/// restores the host's only on the `FORCE_RETURN` paths. So a handler called from inside generated
/// code inherits the guest's rounding mode and its flush-to-zero / denormals-are-zero bits, while
/// design A services the call after `run` has returned and the host MXCSR is back.
///
/// **This matters for task 3, not for the benchmark.** `exp`, `log`, `powf` and `sincosf` are all in
/// the set of imports the 3,594 initializers reach, and Rust's `f32`/`f64` compile to SSE. A host
/// `powf` serviced inline would run under whatever FPCR the guest last set — silently, with no error
/// and a plausible answer. Either such a handler saves and restores MXCSR itself, or symbols that do
/// host floating point stay on the exiting thunk.
///
/// Asserted rather than left as a code reading, because a code reading is not a measurement.
#[test]
fn an_inline_handler_inherits_the_guest_mxcsr() {
    use core::sync::atomic::{AtomicU32, Ordering};



    /// What `_mm_getcsr()` read inside the handler.
    static SEEN: AtomicU32 = AtomicU32::new(0);

    fn record_mxcsr(_call: &mut InlineThunkCall<'_>) {
        SEEN.store(read_mxcsr(), Ordering::Relaxed);
    }

    let _serial = serialized();
    let guest = Guest::new();
    let thunk = guest.code + THUNK_AT;

    // The host's own MXCSR, for the comparison. 0x1F80 on a default Windows x64 thread.
    let host = read_mxcsr();

    // MOV X21, X30 ; MOVZ X0, #0x0100, LSL #16  (FPCR.FZ, bit 24) ; MSR FPCR, X0 ; BL thunk ; RET X21
    let mut program = vec![mov_reg(21, 30), movz(0, 0x0100, 1), msr_fpcr(0)];
    program.push(bl(THUNK_AT as i32 / 4 - program.len() as i32));
    program.push(ret(21));
    let entry = guest.load_at(0, &program);

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, record_mxcsr).expect("an inline thunk");
    rearm(&mut cpu, sentinel);
    match cpu.run(entry, RunLimit::Unlimited).expect("run") {
        ExitReason::Returned { pc } => assert_eq!(pc, sentinel),
        other => panic!("expected the sentinel, got {other}"),
    }
    assert_eq!(cpu.inline_calls(), 1, "the handler must have run");

    let seen = SEEN.load(Ordering::Relaxed);
    assert_ne!(seen, 0, "the handler did not record anything");
    // FPCR.FZ makes dynarmic set SSE flush-to-zero (bit 15) and denormals-are-zero (bit 6);
    // `A64JitState::SetFpcr` is where that mapping lives.
    const FTZ: u32 = 1 << 15;
    const DAZ: u32 = 1 << 6;
    assert_eq!(
        seen & (FTZ | DAZ),
        FTZ | DAZ,
        "the handler saw MXCSR {seen:#06x} with the host's at {host:#06x}. If the flush-to-zero and          denormals-are-zero bits are clear, the guest's MXCSR is NOT live inside an inline handler          and this finding should be withdrawn from the task 1 report"
    );
    // And the host's own MXCSR does not have them, which is what makes the assertion above a
    // difference rather than a coincidence.
    assert_eq!(host & (FTZ | DAZ), 0, "the host thread already had FTZ/DAZ set; test is void");

    // Design A services the call *after* `run` returns, so the host MXCSR is back by then. The same
    // program, the same guest FPCR, an exiting thunk.
    SEEN.store(0, Ordering::Relaxed);
    let (mut cpu, _sentinel) = guest.thread();
    cpu.add_thunk(thunk).expect("an exiting thunk");
    rearm(&mut cpu, sentinel);
    match cpu.run(entry, RunLimit::Unlimited).expect("run") {
        ExitReason::Thunk { .. } => {}
        other => panic!("expected the thunk exit, got {other}"),
    }
    let after_exit = read_mxcsr();
    assert_eq!(
        after_exit, host,
        "design A must hand control back with the HOST MXCSR restored; got {after_exit:#06x}          against {host:#06x}. If this fails, the difference between the two designs is smaller than          the report claims and both need the save/restore"
    );
}

/// **The measurement.** Both designs, with and without a representative marshal, plus the PLT-stub
/// shape, against a baseline that is the same guest loop with the call removed.
///
/// The baseline is measured rather than assumed, which is the one thing D5 amendment 2's "under
/// 53 ns" got right and the reason it is a ceiling rather than a number: subtracting a loop cost that
/// has not been measured is how a figure ends up containing ten guest instructions of somebody
/// else's work.
#[test]
#[ignore = "measurement, not a test"]
fn the_thunk_round_trip() {
    let _serial = serialized();
    let guest = Guest::new();
    guest.assert_high_addresses();
    let thunk = guest.code + THUNK_AT;

    println!();
    println!("THE THUNK ROUND TRIP — {CALLS} guest calls per sample, n = {N} samples per cell");
    println!("  backend: {}", guest.backend.name());

    // The baseline: the identical loop with `NOP` where the `BL` was. Every net figure below is
    // relative to this, so it is the boundary rather than the loop that drives it.
    let baseline_entry = guest.load_at(BASELINE_AT, &call_loop(CALLS, BASELINE_AT, None));
    let baseline = {
        let (mut cpu, sentinel) = guest.thread();
        measure(|| assert_eq!(run_inline(&mut cpu, baseline_entry, sentinel), 1))
    };
    report("baseline: the same loop, no call at all", &baseline, None);

    // The direct shape: `BL` straight into the thunk region, which is what `ARCHITECTURE.md`
    // section 5 describes.
    let entry = guest.load_at(DIRECT_AT, &call_loop(CALLS, DIRECT_AT, Some(THUNK_AT)));

    let exiting = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_thunk(thunk).expect("a thunk");
        measure(|| assert_eq!(run_exiting(&mut cpu, entry, sentinel, false), CALLS))
    };
    report("A: exit to Rust per call", &exiting, Some(&baseline));

    let exiting_marshal = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_thunk(thunk).expect("a thunk");
        measure(|| assert_eq!(run_exiting(&mut cpu, entry, sentinel, true), CALLS))
    };
    report("A: exit to Rust, + 8-argument marshal", &exiting_marshal, Some(&baseline));

    let inline = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_inline_thunk(thunk, nothing).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(run_inline(&mut cpu, entry, sentinel), 1, "design B must stay in the loop");
        });
        assert!(cpu.inline_calls() >= CALLS, "the inline path must have run");
        summary
    };
    report("B: dispatch inside the run loop", &inline, Some(&baseline));

    let inline_marshal = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_inline_thunk(thunk, marshal_eight_arguments).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(run_inline(&mut cpu, entry, sentinel), 1, "design B must stay in the loop");
        });
        assert!(cpu.inline_calls() >= CALLS, "the inline path must have run");
        summary
    };
    report("B: dispatch inline, + 8-argument marshal", &inline_marshal, Some(&baseline));

    // The shape the loader really produces: the guest calls the PLT stub, whose GOT slot the loader
    // bound to the thunk address, and the stub's `BR X17` is what enters the thunk region.
    let stub = guest.code + STUB_AT;
    let got_slot = guest.data;
    guest.write_u64(got_slot, thunk as u64);
    let via_stub = guest.load_at(VIA_STUB_AT, &call_loop(CALLS, VIA_STUB_AT, Some(STUB_AT)));
    guest.load_at(STUB_AT, &plt_stub(stub, got_slot));

    let inline_via_stub = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_inline_thunk(thunk, marshal_eight_arguments).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(
                run_inline(&mut cpu, via_stub, sentinel),
                1,
                "design B must stay in the loop"
            );
        });
        assert!(cpu.inline_calls() >= CALLS, "the inline path must have run");
        summary
    };
    report("B: inline + marshal, through a real PLT stub", &inline_via_stub, Some(&baseline));

    let exiting_via_stub = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_thunk(thunk).expect("a thunk");
        measure(|| assert_eq!(run_exiting(&mut cpu, via_stub, sentinel, true), CALLS))
    };
    report("A: exit + marshal, through a real PLT stub", &exiting_via_stub, Some(&baseline));

    // **A repeat of the direct cell, last.** The stub cells came out *faster* than the direct ones
    // for design A, which is the wrong direction -- the stub is four more guest instructions and an
    // indirect terminal. Either the stub really helps design A, or the figure depends on where in the
    // matrix a cell runs. Re-measuring the direct cell at the end is what tells the two apart, and it
    // costs one more cell.
    let exiting_marshal_again = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_thunk(thunk).expect("a thunk");
        measure(|| assert_eq!(run_exiting(&mut cpu, entry, sentinel, true), CALLS))
    };
    report("A: exit + marshal, direct, re-measured last", &exiting_marshal_again, Some(&baseline));

    let base = baseline.ns_per_call();
    let a = exiting.ns_per_call() - base;
    let b = inline.ns_per_call() - base;
    let marshal_a = exiting_marshal.ns_per_call() - exiting.ns_per_call();
    let marshal_b = inline_marshal.ns_per_call() - inline.ns_per_call();
    println!();
    println!("  A / B, boundary only                          {:>9.2}x", a / b);
    println!(
        "  A / B, as the loader builds it                {:>9.2}x",
        (exiting_via_stub.ns_per_call() - base) / (inline_via_stub.ns_per_call() - base)
    );
    println!(
        "  the 8-argument marshal costs                  {marshal_a:>9.2} ns in A, {marshal_b:.2} ns in B"
    );
    println!(
        "  the PLT stub in front of the thunk adds       {:>9.2} ns in B, {:.2} ns in A",
        inline_via_stub.ns_per_call() - inline_marshal.ns_per_call(),
        exiting_via_stub.ns_per_call() - exiting_marshal.ns_per_call()
    );
    println!(
        "  3,594 initializers x one import call each:    {:>9.3} ms in A, {:.3} ms in B",
        a * 3_594.0 / 1e6,
        b * 3_594.0 / 1e6
    );
    println!();
}

/// **What one `od_jit_run` entry costs, separated from the round trip.**
///
/// D5 amendment 2 derived "under 53 ns" for this from a warm pass over 870 real Roblox leaves, and
/// said isolating it exactly would need a guest function of zero instructions. This is as close as
/// the architecture allows: a single `RET` to the sentinel, so the timed body is one entry, one exit
/// and one guest instruction. It is here because it is the *floor* under design A — design A cannot
/// cost less than one entry plus one exit — and because it is the figure M3 was told to plan against.
#[test]
#[ignore = "measurement, not a test"]
fn one_run_entry_and_exit_alone() {
    let _serial = serialized();
    let guest = Guest::new();
    guest.assert_high_addresses();
    let entry = guest.load_at(BASELINE_AT, &[ret(30)]);
    let (mut cpu, sentinel) = guest.thread();
    let sentinel_value = sentinel as u64;

    const ENTRIES: u64 = 200_000;
    let summary = Summary::of(
        (0..N)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..ENTRIES {
                    cpu.set_x(x(30), sentinel_value);
                    match cpu.run(entry, RunLimit::Unlimited) {
                        Ok(ExitReason::Returned { .. }) => {}
                        other => panic!("unexpected {other:?}"),
                    }
                }
                t.elapsed()
            })
            .collect(),
    );
    let per_entry = summary.median.as_secs_f64() * 1e9 / ENTRIES as f64;
    let min = summary.min.as_secs_f64() * 1e9 / ENTRIES as f64;
    let max = summary.max.as_secs_f64() * 1e9 / ENTRIES as f64;
    println!();
    println!("ONE run ENTRY AND EXIT — {ENTRIES} per sample, n = {N} samples");
    println!(
        "  a one-instruction guest function             {per_entry:>9.2} ns/entry  \
         (min {min:.2}, max {max:.2})"
    );
    println!(
        "  (one `set_x`, one `RET`, one entry, one exit. D5 amendment 2's ceiling for the entry \
         alone was 53 ns, derived rather than isolated.)"
    );
    println!();
}
