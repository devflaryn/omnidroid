//! **The thunk round trip**: what it costs for the guest to call out of its world and come back,
//! measured two ways, because the two ways are M3's actual design decision.
//!
//! The only cost figure on the branch before this was "under 53 ns" (D5 amendment 2), and **two
//! things about it were wrong**. The M3 brief described it as an *entry* ceiling excluding the exit;
//! `DECISIONS.md:1064-1074` measures "870 entries to **and exits from** `od_jit_run`", so it was
//! already an entry-and-exit figure. And it is not a ceiling that holds: review measured the same path
//! at **41.8 / 89.0 / 91.7 ns** in one process, so 53 ns describes the first measurement in a pristine
//! process and not the ordinary cost. See `z_the_same_entry_and_exit_measured_last`.
//!
//! What M3 needs either way is the **round trip**: the guest branches into the thunk region, the host
//! services the call, the guest resumes. Every one of `libroblox.so`'s imported symbols crosses this
//! boundary, and it is crossed from all 3,594 static initializers before a frame is ever drawn. The
//! budget that came out of this file is **about 33 ns per call for design B against 80-105 ns for
//! design A, a factor of 3**, measured through a real PLT stub because that is the shape the loader
//! produces.
//!
//! Two shapes, and the measurement decides between them:
//!
//! * **Exit to Rust per call.** [`ExitReason::Thunk`] comes back out of [`GuestCpu::run`], the caller
//!   services the symbol and calls `run` again at the link register. Simple, and it pays a full
//!   unwind of the generated frame plus a full re-entry every time.
//! * **Dispatch inside the run loop.** `GuestCpu::add_inline_thunk` services the call in the `SVC`
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

#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use std::time::{Duration, Instant};

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::DynarmicCpu;
use omni_cpu::{ExitReason, GuestAddr, GuestCpu, GuestCpuBackend, RunLimit, ThunkCall, ThunkContext};

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
const DEFAULT_DIRECT_AT: usize = 0x0400;
const VIA_STUB_AT: usize = 0x0800;
/// The thunk itself. Inside the code region on purpose: the real loader gives imported symbols a
/// reserved region (`ARCHITECTURE.md` section 5), but what the backend sees is a branch to an address
/// it was told about, and `read_code` plants the stop there whatever guest memory holds.
const THUNK_AT: usize = 0x2000;
const STUB_AT: usize = 0x4000;

/// Where the direct call loop is placed, overridable through `OMNI_THUNK_DIRECT_AT`.
///
/// Configurable **because the sweep needs it**. Design A's round trip came out 19-24 ns cheaper
/// through a PLT stub than through a direct `BL`, and the first check anyone makes on a result like
/// that is whether the guest program's address moved it. Doing that by editing a constant and
/// rebuilding measures four binaries; reading it from the environment measures one, which is the
/// point. `tools/thunk_sweep.py` drives it.
fn direct_at() -> usize {
    match std::env::var("OMNI_THUNK_DIRECT_AT") {
        Ok(value) => {
            let text = value.trim();
            let parsed = text
                .strip_prefix("0x")
                .map_or_else(|| text.parse::<usize>(), |hex| usize::from_str_radix(hex, 16))
                .unwrap_or_else(|e| panic!("OMNI_THUNK_DIRECT_AT={value:?} is not a number: {e}"));
            assert!(
                parsed % 4 == 0 && parsed + 64 <= THUNK_AT,
                "OMNI_THUNK_DIRECT_AT={parsed:#x} must be word-aligned and leave room before the \
                 thunk at {THUNK_AT:#x}"
            );
            parsed
        }
        Err(_) => DEFAULT_DIRECT_AT,
    }
}

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
fn nothing(_call: &mut ThunkCall<'_>) {}

/// `LDMXCSR` from a `u32`.
#[cfg(target_arch = "x86_64")]
fn write_mxcsr(value: u32) {
    // SAFETY: SSE2 is baseline on x86-64 and this file is `cfg(target_arch = "x86_64")`. Every value
    // this is called with was read out of `MXCSR` or is one of its documented bits.
    unsafe { core::arch::asm!("ldmxcsr [{}]", in(reg) &value, options(nostack)) };
}

/// `STMXCSR` into a `u32`. `_mm_getcsr` is deprecated in favour of exactly this.
#[cfg(target_arch = "x86_64")]
fn read_mxcsr() -> u32 {
    let mut out: u32 = 0;
    // SAFETY: SSE2 is baseline on x86-64 and this file is `cfg(target_arch = "x86_64")`.
    // `stmxcsr` writes four bytes to a `u32` this frame owns.
    unsafe { core::arch::asm!("stmxcsr [{}]", in(reg) &mut out, options(nostack)) };
    out
}

/// The bits of the host's floating-point control word that flush denormals: `MXCSR.FTZ` (bit 15)
/// and `MXCSR.DAZ` (bit 6) on x86-64.
#[cfg(target_arch = "x86_64")]
const HOST_FLUSH_BITS: u32 = (1 << 15) | (1 << 6);

/// On an `aarch64` host the control word the dispatcher's guard switches is `FPCR`, and there is
/// one flush bit, `FPCR.FZ` (bit 24), which governs inputs and outputs alike -- the very bit the
/// guest program below sets. `MSR FPCR` from a `u32`.
#[cfg(target_arch = "aarch64")]
fn write_mxcsr(value: u32) {
    // SAFETY: `FPCR` is writable at EL0; every value this is called with was read out of `FPCR` or
    // is `FPCR.FZ`, a defined bit.
    unsafe { core::arch::asm!("msr fpcr, {}", in(reg) u64::from(value), options(nomem, nostack)) };
}

/// `MRS` of `FPCR`; see [`write_mxcsr`].
#[cfg(target_arch = "aarch64")]
fn read_mxcsr() -> u32 {
    let out: u64;
    // SAFETY: `FPCR` is readable at EL0 and `mrs` touches no memory.
    unsafe { core::arch::asm!("mrs {}, fpcr", out(reg) out, options(nomem, nostack)) };
    out as u32
}

/// `FPCR.FZ`; see [`write_mxcsr`].
#[cfg(target_arch = "aarch64")]
const HOST_FLUSH_BITS: u32 = 1 << 24;

/// A representative AAPCS64 marshal: read the eight integer argument registers, combine them so the
/// optimizer cannot delete the reads, write the result register.
///
/// Eight and one because that is AAPCS64's integer argument set plus its return register. The cost is
/// the same nine `JitState` accesses in either design, so reporting it separately keeps the comparison
/// about *dispatch* and hands the marshal cost to task 2 as a number of its own.
fn marshal_eight_arguments(call: &mut ThunkCall<'_>) {
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
    let entry = guest.load_at(direct_at(), &call_loop(CHECK_CALLS, direct_at(), Some(THUNK_AT)));

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
    cpu.add_inline_thunk(thunk, marshal_eight_arguments, ThunkContext::default()).expect("an inline thunk");
    arm_arguments(&mut cpu);
    let entries = run_inline(&mut cpu, entry, sentinel);
    assert_eq!(
        entries, 1,
        "inline dispatch is the whole point: {CHECK_CALLS} guest calls must have been serviced \
         inside ONE run. {entries} entries means the callback raised a halt after all, and design \
         B's timing is then measuring design A"
    );
    assert_eq!(
        cpu.inline_thunk_calls().serviced,
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
    let before = cpu.inline_thunk_calls().serviced;
    rearm(&mut cpu, sentinel);
    match cpu.run(entry, RunLimit::Unlimited).expect("a run after removal") {
        ExitReason::Thunk { pc } => assert_eq!(pc, thunk),
        other => panic!("after removal the address must exit, got {other}"),
    }
    assert_eq!(cpu.inline_thunk_calls().serviced, before, "a removed inline thunk must not still be serviced");
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
    cpu.add_inline_thunk(guest.code + inline_at, marshal_eight_arguments, ThunkContext::default()).expect("inline");
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
    assert_eq!(cpu.inline_thunk_calls().serviced, 1, "the inline thunk must have been serviced on the way past");
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
    cpu.add_inline_thunk(thunk, marshal_eight_arguments, ThunkContext::default()).expect("an inline thunk");
    arm_arguments(&mut cpu);
    assert_eq!(
        run_inline(&mut cpu, entry, sentinel),
        1,
        "a call through the stub must not leave the run loop either"
    );
    assert_eq!(cpu.inline_thunk_calls().serviced, CHECK_CALLS);
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
    let entry = guest.load_at(direct_at(), &call_loop(1 << 40, direct_at(), Some(THUNK_AT)));

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, nothing, ThunkContext::default()).expect("an inline thunk");
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
        cpu.inline_thunk_calls().serviced > 0,
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

/// **The dispatcher's MXCSR guard, in both directions.**
///
/// The hazard, found by reading the vendored emitter and confirmed by measuring it before the guard
/// existed: `A64EmitX64::EmitA64CallSupervisor` calls `Devirtualize<CallSVC>::EmitCall` with **no**
/// `code.SwitchMxcsrOnExit()` in front of it, and `BlockOfCode::GenRunCode` restores the host's word
/// only on the two `FORCE_RETURN` paths. So before the guard, a handler ran with the guest's rounding
/// mode and its flush-to-zero and denormals-are-zero bits. Rust's `f32`/`f64` compile to SSE and
/// `exp`, `log`, `powf` and `sincosf` are all among the imports the 3,594 initializers reach, so a
/// host `powf` serviced inline would have computed with denormals flushed and returned a plausible
/// number.
///
/// `dynarmic::mxcsr::Guard` closes it **in the dispatcher**, at the one point every inline handler
/// passes through, rather than in the handlers — a guard a handler has to remember is silently absent
/// from the handler that forgot it. This test asserts both halves of it, because a guard that installs
/// the host's word and does not put the guest's back has merely moved the corruption into the guest:
///
/// 1. the handler sees the **host's** `MXCSR`, not the guest's;
/// 2. the guest's floating-point control state is **still in force after the call returns**, observed
///    from guest code by a denormal multiply, which is the only way guest code can see `FPCR.FZ`.
#[test]
fn the_dispatcher_puts_the_host_mxcsr_under_a_handler_and_the_guest_s_back() {
    use core::sync::atomic::{AtomicU32, Ordering};

    /// What the handler read out of `MXCSR`.
    static SEEN: AtomicU32 = AtomicU32::new(0);

    fn record_mxcsr(_call: &mut ThunkCall<'_>) {
        SEEN.store(read_mxcsr(), Ordering::Relaxed);
    }

    let _serial = serialized();
    let guest = Guest::new();
    let thunk = guest.code + THUNK_AT;
    let host = read_mxcsr();
    assert_eq!(host & HOST_FLUSH_BITS, 0, "the host thread already had FTZ/DAZ set; this test is void");

    // The smallest positive double subnormal, and 1.0. Under `FPCR.FZ` the multiply flushes to +0;
    // without it the result is the subnormal itself.
    const SUBNORMAL: u64 = 1;
    const ONE: u64 = 0x3FF0_0000_0000_0000;

    // Run the same program twice, once with FPCR.FZ set and once with FPCR clear, and compare what
    // the guest computes *after* the call. Two directions, because a test of a guard that only ever
    // sees one control word cannot tell a restore from a no-op.
    for (label, fpcr_fz) in [("FPCR.FZ set", true), ("FPCR clear", false)] {
        guest.write_u64(guest.data, SUBNORMAL);
        guest.write_u64(guest.data + 8, ONE);
        guest.write_u64(guest.data + 16, 0xDEAD_BEEF);
        SEEN.store(0, Ordering::Relaxed);

        // MOV X21, X30 ; MOV X1, #data ; MOVZ X0, #FZ ; MSR FPCR, X0 ; BL thunk
        //   ; LDR D0,[X1] ; LDR D1,[X1,#8] ; FMUL D2, D0, D1 ; STR D2,[X1,#16] ; RET X21
        let mut program = vec![mov_reg(21, 30)];
        program.extend(mov64(1, guest.data as u64));
        // `FPCR.FZ` is bit 24, i.e. 0x0100 shifted left by 16.
        program.push(if fpcr_fz { movz(0, 0x0100, 1) } else { movz(0, 0, 0) });
        program.push(msr_fpcr(0));
        program.push(bl(THUNK_AT as i32 / 4 - program.len() as i32));
        program.push(ldr_d(0, 1, 0));
        program.push(ldr_d(1, 1, 8));
        program.push(fmul_d(2, 0, 1));
        program.push(str_d(2, 1, 16));
        program.push(ret(21));
        let entry = guest.load_at(0, &program);

        let (mut cpu, sentinel) = guest.thread();
        cpu.add_inline_thunk(thunk, record_mxcsr, ThunkContext::default()).expect("an inline thunk");
        rearm(&mut cpu, sentinel);
        match cpu.run(entry, RunLimit::Unlimited).expect("run") {
            ExitReason::Returned { pc } => assert_eq!(pc, sentinel),
            other => panic!("{label}: expected the sentinel, got {other}"),
        }
        assert_eq!(cpu.inline_thunk_calls().serviced, 1, "{label}: the handler must have run");

        // 1. The handler saw the host's word.
        let seen = SEEN.load(Ordering::Relaxed);
        assert_eq!(
            seen, host,
            "{label}: the handler saw MXCSR {seen:#06x} against the host's {host:#06x}. Without the \
             dispatcher's guard it sees the GUEST's word, and any host floating point in a handler -- \
             `powf`, `exp`, `log`, `sincosf`, all of which the initializers reach -- computes under \
             the guest's rounding mode and denormal control"
        );

        // 2. The guest's word survived the call, observed by the guest itself.
        let product = guest.read_u64(guest.data + 16);
        let expected = if fpcr_fz { 0 } else { SUBNORMAL };
        assert_eq!(
            product, expected,
            "{label}: the guest multiplied the smallest subnormal by 1.0 after the call and got \
             {product:#018x}, wanted {expected:#018x}. A guard that installs the host's MXCSR and \
             does not restore the guest's has only moved the corruption into the guest, where no \
             host-side assertion can see it"
        );
    }
}

/// **The guest's vector file is coherent at an inline callback, and a handler's writes reach the
/// resumed guest.**
///
/// The review asked for this, and it is the right thing to ask: `add_inline_thunk`'s documentation
/// claims "guest registers are coherent in `JitState` at every callback", and the evidence offered
/// was the integer file. AAPCS64 passes floating-point and vector arguments in `V0`-`V7` and returns
/// in `V0`, so a marshal that could only reach `X0`-`X7` would silently drop every `double`
/// argument — a whole class of imported symbols returning plausible garbage. `exp`, `log`, `powf` and
/// `sincosf` are in the reachable set and every one of them takes and returns a `double` or a `float`.
///
/// `A64EmitX64::EmitA64SetQ` stores to `JitState.vec` with a `movaps`, exactly as the integer setters
/// store to `JitState.reg`, so the symmetry is real. This asserts it instead of relying on it, in both
/// directions: the handler must *read* what the guest put in `V0`, and what the handler *writes* to
/// `V1` must be what the guest then stores to memory.
#[test]
fn an_inline_handler_sees_and_writes_the_guest_vector_file() {
    use core::sync::atomic::{AtomicU64, Ordering};

    /// The two halves of `V0` as the handler saw them.
    static SEEN_LO: AtomicU64 = AtomicU64::new(0);
    static SEEN_HI: AtomicU64 = AtomicU64::new(0);

    /// A 128-bit pattern with every byte distinct, so a half-swap or a truncation to 64 bits is
    /// visible rather than plausible.
    const ARGUMENT: u128 = 0x0F1E_2D3C_4B5A_6978_8796_A5B4_C3D2_E1F0;
    /// What the handler returns. Not `!ARGUMENT`, so a handler that did nothing at all cannot pass.
    const RESULT: u128 = 0x0123_4567_89AB_CDEF_FEDC_BA98_7654_3210;

    fn vector_marshal(call: &mut ThunkCall<'_>) {
        let seen = call.v(0);
        SEEN_LO.store(seen as u64, Ordering::Relaxed);
        SEEN_HI.store((seen >> 64) as u64, Ordering::Relaxed);
        call.set_v(1, RESULT);
    }

    let _serial = serialized();
    let guest = Guest::new();
    let thunk = guest.code + THUNK_AT;

    guest.write_u64(guest.data, ARGUMENT as u64);
    guest.write_u64(guest.data + 8, (ARGUMENT >> 64) as u64);
    guest.write_u64(guest.data + 16, 0);
    guest.write_u64(guest.data + 24, 0);

    // MOV X21, X30 ; MOV X1, #data ; LDR Q0, [X1] ; BL thunk ; STR Q1, [X1, #16] ; RET X21
    let mut program = vec![mov_reg(21, 30)];
    program.extend(mov64(1, guest.data as u64));
    program.push(ldr_q(0, 1, 0));
    program.push(bl(THUNK_AT as i32 / 4 - program.len() as i32));
    program.push(str_q(1, 1, 16));
    program.push(ret(21));
    let entry = guest.load_at(0, &program);

    let (mut cpu, sentinel) = guest.thread();
    cpu.add_inline_thunk(thunk, vector_marshal, ThunkContext::default()).expect("an inline thunk");
    rearm(&mut cpu, sentinel);
    match cpu.run(entry, RunLimit::Unlimited).expect("run") {
        ExitReason::Returned { pc } => assert_eq!(pc, sentinel),
        other => panic!("expected the sentinel, got {other}"),
    }
    assert_eq!(cpu.inline_thunk_calls().serviced, 1, "the handler must have run");

    let seen = u128::from(SEEN_LO.load(Ordering::Relaxed))
        | (u128::from(SEEN_HI.load(Ordering::Relaxed)) << 64);
    assert_eq!(
        seen, ARGUMENT,
        "the handler read V0 as {seen:#034x}, wanted {ARGUMENT:#034x}. If the low half is right and \
         the high half is zero, the vector file is only half coherent at a callback and a marshal \
         must not read 128-bit arguments through it"
    );

    let written = u128::from(guest.read_u64(guest.data + 16))
        | (u128::from(guest.read_u64(guest.data + 24)) << 64);
    assert_eq!(
        written, RESULT,
        "the guest stored {written:#034x} from V1 after the call, wanted the {RESULT:#034x} the \
         handler wrote. A handler that cannot return a value in the vector file cannot implement any \
         imported symbol that returns a double"
    );

    // And the register file the runtime reads afterwards agrees with what the guest saw, so a caller
    // does not get a third answer.
    assert_eq!(cpu.v(omni_cpu::VReg::new(1).expect("V1")), RESULT);
}

/// **The measurement.** Both designs, with and without a representative marshal, plus the PLT-stub
/// shape, against a baseline that is the same guest loop with the call removed.
///
/// The baseline is measured rather than assumed, which is the one thing D5 amendment 2 got right:
/// subtracting a loop cost that has not been measured is how a figure ends up containing ten guest
/// instructions of somebody else's work. `tools/thunk_sweep.py` runs this across processes and
/// placements and reports which cells are stable enough to quote a median from — design A's are not.
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
    let entry = guest.load_at(direct_at(), &call_loop(CALLS, direct_at(), Some(THUNK_AT)));

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
        cpu.add_inline_thunk(thunk, nothing, ThunkContext::default()).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(run_inline(&mut cpu, entry, sentinel), 1, "design B must stay in the loop");
        });
        assert!(cpu.inline_thunk_calls().serviced >= CALLS, "the inline path must have run");
        summary
    };
    report("B: dispatch inside the run loop", &inline, Some(&baseline));

    let inline_marshal = {
        let (mut cpu, sentinel) = guest.thread();
        cpu.add_inline_thunk(thunk, marshal_eight_arguments, ThunkContext::default()).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(run_inline(&mut cpu, entry, sentinel), 1, "design B must stay in the loop");
        });
        assert!(cpu.inline_thunk_calls().serviced >= CALLS, "the inline path must have run");
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
        cpu.add_inline_thunk(thunk, marshal_eight_arguments, ThunkContext::default()).expect("an inline thunk");
        let summary = measure(|| {
            assert_eq!(
                run_inline(&mut cpu, via_stub, sentinel),
                1,
                "design B must stay in the loop"
            );
        });
        assert!(cpu.inline_thunk_calls().serviced >= CALLS, "the inline path must have run");
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

/// **What the dispatcher's MXCSR guard costs**, measured on its own.
///
/// Reported separately from the boundary because it is the price of correction 1 — the guest's SSE
/// control word being live inside a host callback — and a correctness fix whose cost is unknown is an
/// argument waiting to be had. Two cases, because the guard branches on them: the guest has not
/// touched `FPCR`, so the words match and the guard is one `stmxcsr` and a compare; and the guest has,
/// so it is that plus two `ldmxcsr`.
///
/// Measured in host code rather than through the guest, on purpose. Against design B's ~30 ns the
/// guard is a couple of nanoseconds, which is inside the spread of the B cells, so a before-and-after
/// of the boundary could not resolve it. This can.
#[test]
#[ignore = "measurement, not a test"]
fn what_the_mxcsr_guard_costs() {
    let _serial = serialized();
    const ROUNDS: u64 = 2_000_000;
    let host = read_mxcsr();

    println!();
    println!("THE MXCSR GUARD — {ROUNDS} enter/exit pairs per sample, n = {N} samples");
    for (label, guest_word) in [("words already equal (the common case)", host), ("words differ", host | HOST_FLUSH_BITS)] {
        let summary = Summary::of(
            (0..N)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..ROUNDS {
                        // Stand the guest's word up, then do exactly what the dispatcher does.
                        write_mxcsr(guest_word);
                        let guard = omni_cpu::dynarmic::mxcsr_guard_for_measurement(host);
                        core::hint::black_box(&guard);
                        drop(guard);
                    }
                    t.elapsed()
                })
                .collect(),
        );
        // Net of the `write_mxcsr` the loop needs to set the scene, which the guard does not pay.
        let setup = Summary::of(
            (0..N)
                .map(|_| {
                    let t = Instant::now();
                    for _ in 0..ROUNDS {
                        write_mxcsr(guest_word);
                        core::hint::black_box(&guest_word);
                    }
                    t.elapsed()
                })
                .collect(),
        );
        let gross = summary.median.as_secs_f64() * 1e9 / ROUNDS as f64;
        let base = setup.median.as_secs_f64() * 1e9 / ROUNDS as f64;
        println!("  {label:<46} {:>9.2} ns   (gross {gross:.2}, loop {base:.2})", gross - base);
    }
    write_mxcsr(host);
    println!();
}

/// One `od_jit_run` entry and exit, on a one-instruction guest function.
///
/// Shared by the two tests below, whose only difference is **where in the process they run**.
fn measure_one_entry_and_exit(label: &str) {
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
    let per = |d: Duration| d.as_secs_f64() * 1e9 / ENTRIES as f64;
    println!(
        "  {label:<44} {:>9.2} ns/entry   (n = {N}: min {:.2}, max {:.2})",
        per(summary.median),
        per(summary.min),
        per(summary.max)
    );
}

/// **One `od_jit_run` entry and exit, measured FIRST in the process.**
///
/// D5 amendment 2 derived "under 53 ns" for this from a warm pass over 870 real Roblox leaves and said
/// isolating it exactly would need a guest function of zero instructions. This is as close as the
/// architecture allows: a single `RET` to the sentinel, so the timed body is one `set_x`, one entry,
/// one guest instruction and one exit.
///
/// **Read it with its twin below.** The name is chosen so this sorts *before* `the_thunk_round_trip`
/// and its twin sorts *after*, because the whole point is that the figure depends on that.
#[test]
#[ignore = "measurement, not a test"]
fn a_one_run_entry_and_exit_measured_first() {
    let _serial = serialized();
    println!();
    println!("ONE run ENTRY AND EXIT — 200000 per sample, n = {N} samples");
    measure_one_entry_and_exit("first in the process");
    println!();
}

/// **The same entry and exit, measured LAST in the process — and it is roughly twice as expensive.**
///
/// This is the review's central correction, made reproducible. `od_jit_run`'s entry-and-exit path is
/// **bimodal on this host**: about 40 ns for the first measurement in a pristine process and 85-100 ns
/// afterwards, with host frequency, thermal state, live contexts, code placement, entry count and the
/// exit reason all ruled out. A fresh `Guest`, a fresh backend and a fresh context each time, so it is
/// not a property of the objects.
///
/// **Why it matters more than a curiosity.** The first version of the task 1 report put design A's
/// round trip at 90.6 ns and this figure at 41.9 ns and concluded the round trip was "2.2x an entry
/// and exit" — which cannot be true, because a design-A round trip *is* one entry and one exit. The
/// two numbers were simply measured in different modes: the entry/exit cell always ran first in its
/// process and the round-trip cells always ran after it. Same-mode, they agree.
///
/// Two consequences the report carries: D5 amendment 2's 53 ns is **not** a ceiling that holds in
/// general, only for the first measurement in a pristine process; and every design-A figure has to be
/// quoted as a band, because it is unstable by a factor of about two. Design B is immune — it does not
/// leave the run loop — and measures the same in either position.
#[test]
#[ignore = "measurement, not a test"]
fn z_the_same_entry_and_exit_measured_last() {
    let _serial = serialized();
    println!();
    println!("THE SAME ENTRY AND EXIT, LATER IN THE PROCESS");
    measure_one_entry_and_exit("last in the process");
    println!(
        "  If this is roughly twice the figure above, `od_jit_run`'s entry/exit path is bimodal on \
         this host and every design-A number must be quoted as a band. `tools/thunk_sweep.py` \
         reports both and computes the ratio."
    );
    println!();
}
