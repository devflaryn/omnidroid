//! **The M2 gate**: real functions out of the real `libroblox.so`, executed through the CPU
//! backend, checked against results predicted from the instruction semantics.
//!
//! # What makes this a gate rather than a demonstration
//!
//! A test that asserts *execution completed* asserts nothing: a run that skipped every instruction
//! completes too. A counter cannot tell applied from skipped, or correct from wrong. So every
//! assertion here is against a value worked out **from what the instructions mean**, before the run
//! — and for the headline function that is 256 distinct predicted values, one per byte, derived
//! from the base64 alphabet rather than from anything the guest produced.
//!
//! Three functions, chosen by `omni-elf`'s `leaf-scan` out of the 245,117 that `.eh_frame_hdr`
//! names. `crates/omni-elf/tests/eh_frame_golden.rs` asserts the selection is reproducible; this
//! file asserts the **bytes at the loaded addresses are the bytes that were decoded**, so the two
//! halves cannot drift apart silently.
//!
//! `init_array` is **not** run: those 3,594 initializers call imported symbols, which needs the
//! thunk boundary, and that is M3.
//!
//! Gated on `x86_64`, because that is where the translating backend exists, and skipped loudly
//! when the APK is absent.

#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::roblox::{serialized, Roblox};
use harness::x;
use omni_cpu::dynarmic::{DynarmicCpu, DynarmicOptions};
use omni_cpu::{AccessKind, ExitReason, GuestAddr, GuestCpu, RunLimit};

// ---------------------------------------------------------------------------------------------
// The three functions, by `p_vaddr`, with the exact words that were decoded to predict them.
// ---------------------------------------------------------------------------------------------

/// `libroblox.so + 0x2c11e34`, 104 bytes: **a base64 character to its six-bit value**.
///
/// It takes the character in `W1` (`X0` is a `this` it never reads) and returns the sextet in `W0`,
/// with `-1` for the padding character `=` and `-2` for anything else. Decoded by hand:
///
/// ```text
///   cmp  w1, #0x2b ('+') ; b.eq -> mov w0, #0x3e (62) ; ret
///   cmp  w1, #0x2f ('/') ; b.ne -> .letters
///                          mov w0, #0x3f (63) ; ret
/// .letters:
///   sub  w0, w1, #0x41 ('A') ; cmp w0, #0x1a ; b.hs .lower ; ret        -- 'A'..'Z' -> 0..25
/// .lower:
///   sub  w8, w1, #0x61 ('a') ; cmp w8, #0x19 ; b.hi .digits
///   sub  w0, w1, #0x47       ; ret                                      -- 'a'..'z' -> 26..51
/// .digits:
///   sub  w8, w1, #0x30 ('0') ; cmp w8, #9 ; b.hi .other
///   add  w0, w1, #4          ; ret                                      -- '0'..'9' -> 52..61
/// .other:
///   cmp  w1, #0x3d ('=') ; mov w8, #-2 ; cinc w0, w8, eq ; ret          -- '=' -> -1, else -2
/// ```
///
/// Six `RET`s and five conditional branches, so it is also seven basic blocks of real engine code
/// rather than a straight line.
const BASE64_SEXTET: u64 = 0x2c1_1e34;

#[rustfmt::skip]
const BASE64_SEXTET_WORDS: [u32; 26] = [
    0x7100_ac3f, 0x5400_00a0, 0x7100_bc3f, 0x5400_00a1, 0x5280_07e0, 0xd65f_03c0,
    0x5280_07c0, 0xd65f_03c0, 0x5101_0420, 0x7100_681f, 0x5400_0042, 0xd65f_03c0,
    0x5101_8428, 0x7100_651f, 0x5400_0068, 0x5101_1c20, 0xd65f_03c0, 0x5100_c028,
    0x7100_251f, 0x5400_0068, 0x1100_1020, 0xd65f_03c0, 0x7100_f43f, 0x1280_0028,
    0x1a88_1500, 0xd65f_03c0,
];

/// `libroblox.so + 0x2227844`, 116 bytes: **a `(seconds, microseconds)` difference in
/// milliseconds, saturating**.
///
/// `X0`/`W1` are one timestamp's seconds and microseconds, `X2`/`W3` the other's. Decoded by hand:
///
/// ```text
///   sub  x8, x0, x2
///   cmp  x8, #0x0020_c49b_a5e3_53f6 ; b.gt -> mov x0, #i64::MAX ; ret
///   cmp  x8, #-0x0020_c49b_a5e3_53f6; b.lt -> mov x0, #i64::MIN ; ret
///   sub  w9, w1, w3 ; add w9, w9, #0x3e7 (999)
///   mul  x8, x8, #1000
///   smull/asr #38/add-sign  -> w9 = w9 / 1000, truncating toward zero
///   add  x0, x8, w9, sxtw ; ret
/// ```
///
/// The two saturation bounds are not arbitrary and are not read off the run: `0x20c49ba5e353f6` is
/// **9,223,372,036,854,774**, which is `(i64::MAX - 1000) / 1000` — the largest second-difference
/// whose millisecond form leaves room for the up-to-1000 ms the microsecond term can still add. It
/// is decoded from the instruction words rather than written down here; see [`seconds_limit`], which
/// also records that the obvious reading, `i64::MAX / 1000`, is one too large and was this test's
/// first (wrong) prediction. `0x1062_4dd3` with a 38-bit arithmetic shift is the standard signed
/// magic-number division by 1000 (Hacker's Delight), which is exact for every `i32`, so the model
/// below uses Rust's own truncating `/` and is not a transcription of the shift sequence.
const TIMEVAL_TO_MILLIS: u64 = 0x222_7844;

#[rustfmt::skip]
const TIMEVAL_TO_MILLIS_WORDS: [u32; 29] = [
    0xd28a_7ec9, 0xcb02_0008, 0xf2b4_bc69, 0xf2d8_9369, 0xf2e0_0409, 0xeb09_011f,
    0x5400_006d, 0x92f0_0000, 0xd65f_03c0, 0xd295_8149, 0xf2ab_4389, 0xf2c7_6c89,
    0xf2ff_fbe9, 0xeb09_011f, 0x5400_006a, 0xd2f0_0000, 0xd65f_03c0, 0x4b03_0029,
    0x5289_ba6a, 0x5280_7d0b, 0x110f_9d29, 0x72a2_0c4a, 0x9b0b_7d08, 0x9b2a_7d29,
    0xd37f_fd2a, 0x9366_fd29, 0x0b0a_0129, 0x8b29_c100, 0xd65f_03c0,
];

/// `libroblox.so + 0x2872aac`, 60 bytes: **a stack-protected accessor returning the constant
/// 0x20000**, and therefore the D13 test.
///
/// ```text
///   sub  sp, sp, #0x20
///   stp  x29, x30, [sp, #0x10]
///   add  x29, sp, #0x10
///   mrs  x8, tpidr_el0          -- the bionic thread pointer
///   ldr  x9, [x8, #0x28]        -- TLS_SLOT_STACK_GUARD
///   str  x9, [sp, #8]           -- the canary, on the frame
///   ldr  x8, [x8, #0x28]        -- read it again
///   ldr  x9, [sp, #8]
///   cmp  x8, x9 ; b.ne .fail
///   mov  w0, #0x20000
///   ldp  x29, x30, [sp, #0x10] ; add sp, sp, #0x20 ; ret
/// .fail:
///   bl   __stack_chk_fail       -- past the last RET, so no returning path calls it
/// ```
///
/// One of 45 such leaves in the binary, all calling the same PLT stub. This is what most real
/// engine code looks like, which is why the leaf scan grades it rather than discarding it.
const STACK_GUARD_LEAF: u64 = 0x287_2aac;

#[rustfmt::skip]
const STACK_GUARD_LEAF_WORDS: [u32; 15] = [
    0xd100_83ff, 0xa901_7bfd, 0x9100_43fd, 0xd53b_d048, 0xf940_1509, 0xf900_07e9,
    0xf940_1508, 0xf940_07e9, 0xeb09_011f, 0x5400_00a1, 0x52a0_0040, 0xa941_7bfd,
    0x9100_83ff, 0xd65f_03c0, 0x94e9_8f3b,
];

/// What `STACK_GUARD_LEAF` returns when the guard matches.
const STACK_GUARD_LEAF_RESULT: u64 = 0x20000;
/// The PLT stub its failure tail calls. Registered as a thunk so a taken call is *visible* rather
/// than being a jump into an unrelocated stub.
const STACK_CHK_FAIL_STUB: u64 = 0x62d_67d0;
/// Where in the body the second `ldr x8, [x8, #0x28]` sits: `STACK_GUARD_LEAF + 6 * 4`.
const STACK_GUARD_RELOAD_OFFSET: u64 = 6 * 4;

/// `CAS W0, W1, [X2]` at `libroblox.so + 0x2b9e630`: a real LSE atomic, which D5 measured as one of
/// the 231 unimplemented decoder entries.
const REAL_LSE_ATOMIC: u64 = 0x2b9_e630;
const REAL_LSE_ATOMIC_WORD: u32 = 0x88a0_7c41;

/// Relocation figures from M1, re-asserted here so that "the function ran" cannot quietly become
/// "the function ran out of an image that was never relocated".
const GRAND_TOTAL_RELOCATIONS: usize = 568_806;
const INIT_ARRAY_ENTRIES: usize = 3_594;

// ---------------------------------------------------------------------------------------------
// The independent models. Written from what the functions *mean*, not from their instructions.
// ---------------------------------------------------------------------------------------------

/// RFC 4648's base64 alphabet, in order. The value of a character is its index.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The sextet a base64 decoder gives a character: its index in the alphabet, `-1` for the padding
/// character, `-2` for anything else.
///
/// Derived from the alphabet, which is why it is an independent prediction: nothing here is
/// transcribed from the guest's instruction sequence, and the two agree only if the guest really
/// implements base64.
fn predicted_sextet(c: u8) -> i32 {
    if let Some(i) = BASE64_ALPHABET.iter().position(|&a| a == c) {
        return i as i32;
    }
    if c == b'=' {
        -1
    } else {
        -2
    }
}

/// The saturation bound, reconstructed from the `MOVZ`/`MOVK` sequence that builds it.
///
/// **A note on how this number was arrived at, because it is the one place in this file where the
/// first prediction was wrong.** The obvious reading of the guest's immediate is `i64::MAX / 1000`
/// — the largest second-difference whose millisecond form fits — and that is what this test
/// predicted first. The run disagreed by exactly one, and the guest was right: the bound has to
/// leave room for the *microsecond* term as well, which contributes up to ±1000 ms, so it is
/// `(i64::MAX - 1000) / 1000`.
///
/// Rather than quietly editing the constant to match the run — which *would* be fitting, and is what
/// this gate exists to rule out — the bound is decoded from the four instruction words the test
/// already asserts byte-for-byte, and *then* cross-checked against the derivation.
///
/// **Decoding it is not fitting, and three properties are what make the difference.** The decoded
/// value is checked against an independently stated closed form, `(i64::MAX - 1000) / 1000`, which
/// is the unique largest `L` with `L * 1000 + 1000 <= i64::MAX` rather than a number chosen to fit.
/// [`movz_movk_immediate`] refuses anything that is not a `MOVZ`/`MOVK`, so what is read is the ARM
/// ARM's meaning of fixed bits and not an arbitrary word. And the decoded constant drives the test's
/// **inputs** as well as its expectations, so a wrong decode desynchronises the two and fails rather
/// than agreeing with itself. It would be fitting if the value came from the *run*, if the
/// closed-form check were dropped, or if the whole model were decoded rather than this one
/// parameter.
fn movz_movk_immediate(words: &[u32]) -> u64 {
    // `sf opc:2 100101 hw:2 imm16:16 Rd:5`, with `sf = 1` for the 64-bit forms and
    // `opc = 10` for MOVZ, `11` for MOVK. So a 64-bit MOVZ is `0xD280_0000` and a MOVK
    // `0xF280_0000` in the fields above `hw`.
    const MOVZ_X: u32 = 0xD280_0000;
    const MOVK_X: u32 = 0xF280_0000;
    let mut value = 0u64;
    for (i, &w) in words.iter().enumerate() {
        let want = if i == 0 { MOVZ_X } else { MOVK_X };
        assert_eq!(
            w & 0xFF80_0000,
            want,
            "word {i} of the immediate sequence is {w:#010x}, which is not the \
             {} this decoding assumes",
            if i == 0 { "MOVZ" } else { "MOVK" }
        );
        let hw = (w >> 21) & 0x3;
        let imm16 = u64::from((w >> 5) & 0xFFFF);
        value |= imm16 << (16 * hw);
    }
    value
}

/// The largest second-difference the guest does not saturate on.
fn seconds_limit() -> i64 {
    // Words 0, 2, 3 and 4 of the body: `mov x9, #0x53f6` and its three `movk`s.
    let decoded = movz_movk_immediate(&[
        TIMEVAL_TO_MILLIS_WORDS[0],
        TIMEVAL_TO_MILLIS_WORDS[2],
        TIMEVAL_TO_MILLIS_WORDS[3],
        TIMEVAL_TO_MILLIS_WORDS[4],
    ]) as i64;
    assert_eq!(
        decoded,
        (i64::MAX - 1000) / 1000,
        "the guest's saturation bound must be the largest second-difference whose millisecond form \
         leaves room for the up-to-1000 ms the microsecond term can add"
    );
    assert_eq!(decoded, 9_223_372_036_854_774);
    // The negative bound is built the same way, out of words 9..13, and must be its negation.
    let low = movz_movk_immediate(&[
        TIMEVAL_TO_MILLIS_WORDS[9],
        TIMEVAL_TO_MILLIS_WORDS[10],
        TIMEVAL_TO_MILLIS_WORDS[11],
        TIMEVAL_TO_MILLIS_WORDS[12],
    ]) as i64;
    assert_eq!(low, -decoded, "the two saturation bounds must be each other's negation");
    decoded
}

/// The saturating `(seconds, microseconds)` difference, in milliseconds, with the microsecond part
/// rounded up by adding 999 before the division.
fn predicted_millis(sec_a: i64, usec_a: i32, sec_b: i64, usec_b: i32) -> i64 {
    let limit = seconds_limit();
    let seconds = sec_a.wrapping_sub(sec_b);
    if seconds > limit {
        return i64::MAX;
    }
    if seconds < -limit {
        return i64::MIN;
    }
    // Both halves wrap in 32 bits in the guest, so they wrap here.
    let micros = usec_a.wrapping_sub(usec_b).wrapping_add(999);
    // Truncating toward zero, which is what the magic-number sequence computes and what Rust's
    // `/` does for `i32`.
    seconds.wrapping_mul(1000).wrapping_add(i64::from(micros / 1000))
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Call a guest function, returning the exit and leaving the results in the register file.
fn call(roblox: &Roblox, cpu: &mut DynarmicCpu, entry: GuestAddr) -> ExitReason {
    roblox.rearm(cpu);
    cpu.run(entry, RunLimit::Instructions(100_000)).expect("the guest ran")
}

/// Assert that the words at `vaddr` are the ones that were decoded to predict the result.
///
/// This is the join between the static analysis and the run. Without it the predicted values would
/// be a claim about a disassembly listing rather than about the code that executed, and a change in
/// the binary would turn the whole file into a test of nothing.
fn assert_words(roblox: &Roblox, vaddr: u64, expected: &[u32]) {
    let base = roblox.at(vaddr);
    for (i, &want) in expected.iter().enumerate() {
        let at = base + i * 4;
        assert_eq!(
            roblox.word_at(at),
            want,
            "{vaddr:#x}+{:#x} (loaded at {at:#x}) is not the instruction this test decoded",
            i * 4
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

/// The premises, asserted before anything is executed: the real library is loaded and relocated,
/// `init_array` has **not** been run, and the three functions are the bytes this file decoded.
#[test]
fn the_library_is_loaded_relocated_and_holds_the_code_this_file_decoded() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };

    let stats = &roblox.object.stats;
    assert_eq!(
        stats.relocations.applied, GRAND_TOTAL_RELOCATIONS,
        "M2 runs out of a relocated image or it runs out of a lie"
    );
    assert_eq!(roblox.object.init_array.len(), INIT_ARRAY_ENTRIES);
    assert!(
        roblox.object.relro.is_some(),
        "PT_GNU_RELRO must have been sealed before any guest code runs"
    );
    assert_eq!(
        roblox.object.imports.total(),
        565,
        "all 565 imports are unresolved and bound to null: a leaf touches none of them"
    );

    // The guest lives high, which is the premise D4's identity mapping is interesting under: a
    // guest below 64 GiB would pass every test here under dynarmic's broken default width too.
    assert!(
        roblox.space.end() > 1usize << 36,
        "this guest space tops out at {:#x}, inside dynarmic's default 36-bit fastmem window",
        roblox.space.end() - 1
    );

    assert_words(&roblox, BASE64_SEXTET, &BASE64_SEXTET_WORDS);
    assert_words(&roblox, TIMEVAL_TO_MILLIS, &TIMEVAL_TO_MILLIS_WORDS);
    assert_words(&roblox, STACK_GUARD_LEAF, &STACK_GUARD_LEAF_WORDS);
    assert_eq!(roblox.word_at(roblox.at(REAL_LSE_ATOMIC)), REAL_LSE_ATOMIC_WORD);

    // And the `BL` in the stack-protected leaf really does reach the stub this file registers as a
    // thunk, computed from the encoding rather than taken on trust.
    let bl = STACK_GUARD_LEAF_WORDS[14];
    let imm26 = ((bl & 0x03FF_FFFF) as i32) << 6 >> 6;
    let target = (STACK_GUARD_LEAF as i64 + 14 * 4 + i64::from(imm26) * 4) as u64;
    assert_eq!(target, STACK_CHK_FAIL_STUB, "the stack-protector call target");
}

/// **M2.** A real function from `libroblox.so` executes through the CPU backend and returns a
/// correct, independently-predicted result — 256 of them, one per byte value.
#[test]
fn a_real_roblox_function_computes_the_base64_alphabet() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    assert_words(&roblox, BASE64_SEXTET, &BASE64_SEXTET_WORDS);

    let entry = roblox.at(BASE64_SEXTET);
    let mut cpu = roblox.thread();
    let before = cpu.slow_path_entries();

    let mut agreed = 0usize;
    for c in 0u8..=255 {
        // `X0` is a `this` the function never reads. It is poisoned so that a result which somehow
        // came from it would be visible, and because every path writes `W0` — which zeroes the top
        // half of `X0` — so the poison also pins the W-form's zero-extension.
        cpu.set_x(x(0), 0xDEAD_BEEF_DEAD_BEEF);
        cpu.set_x(x(1), u64::from(c));
        let exit = call(&roblox, &mut cpu, entry);
        assert_eq!(
            exit,
            ExitReason::Returned { pc: roblox.sentinel },
            "input {c:#04x} did not return through the sentinel"
        );

        let predicted = predicted_sextet(c);
        // The ABI returns in `W0`, and a W-form write zeroes bits 63:32, so the whole of `X0` is
        // predictable: the sign-extension of the negative answers must NOT appear.
        let expected = u64::from(predicted as u32);
        assert_eq!(
            cpu.x(x(0)),
            expected,
            "base64 value of {c:#04x} ({:?}): predicted {predicted}, guest returned {:#x}",
            char::from(c),
            cpu.x(x(0))
        );
        agreed += 1;
    }

    assert_eq!(agreed, 256, "every byte must have been checked");
    // A sanity check on the *predictions* themselves, so a model that answered -2 to everything
    // could not pass: the alphabet must really have been exercised.
    assert_eq!((0u8..=255).filter(|&c| predicted_sextet(c) >= 0).count(), 64);
    assert_eq!(predicted_sextet(b'A'), 0);
    assert_eq!(predicted_sextet(b'/'), 63);
    assert_eq!(predicted_sextet(b'='), -1);
    assert_eq!(predicted_sextet(b' '), -2);

    // D4, on real code: a function that touches no memory must take no callback.
    assert_eq!(
        cpu.slow_path_entries() - before,
        0,
        "a register-only function must reach the end without entering a memory callback"
    );
    assert_eq!(cpu.degraded_slices(), 0);
}

/// A second real function, exercising 64-bit multiply, signed saturation and the compiler's
/// magic-number division — none of which the first one touches.
#[test]
fn a_real_roblox_function_converts_a_timeval_difference_to_milliseconds() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    assert_words(&roblox, TIMEVAL_TO_MILLIS, &TIMEVAL_TO_MILLIS_WORDS);

    let entry = roblox.at(TIMEVAL_TO_MILLIS);
    let mut cpu = roblox.thread();

    // Decoded from the asserted instruction words and cross-checked against the derivation; see
    // `seconds_limit`.
    let limit = seconds_limit();

    let vectors: [(i64, i32, i64, i32); 18] = [
        (0, 0, 0, 0),
        (5, 250_000, 3, 100_000),
        (3, 0, 5, 0),
        (0, 0, 0, 1),
        (0, 1_000, 0, 0),
        (0, 0, 0, 1_000),
        (0, 0, 0, 2_000),
        (0, 999, 0, 0),
        (0, 1, 0, 0),
        (1, 500_000, 0, 999_999),
        (-4, 250_000, 7, -125_000),
        // Exactly at each saturation bound, and one past it. The pair is the point: a check that
        // only tested "far outside" would pass with the comparison written the wrong way round.
        (limit, 0, 0, 0),
        (limit + 1, 0, 0, 0),
        (-limit, 0, 0, 0),
        (-limit - 1, 0, 0, 0),
        // The 32-bit microsecond field at its extremes, where the `+999` wraps.
        (0, i32::MAX, 0, 0),
        (0, i32::MIN, 0, 0),
        (0, i32::MIN, 0, i32::MAX),
    ];

    for (sec_a, usec_a, sec_b, usec_b) in vectors {
        cpu.set_x(x(0), sec_a as u64);
        cpu.set_x(x(1), u64::from(usec_a as u32));
        cpu.set_x(x(2), sec_b as u64);
        cpu.set_x(x(3), u64::from(usec_b as u32));
        let exit = call(&roblox, &mut cpu, entry);
        assert_eq!(exit, ExitReason::Returned { pc: roblox.sentinel });

        let predicted = predicted_millis(sec_a, usec_a, sec_b, usec_b);
        assert_eq!(
            cpu.x(x(0)) as i64,
            predicted,
            "({sec_a}, {usec_a}) - ({sec_b}, {usec_b}): predicted {predicted} ms, guest returned {}",
            cpu.x(x(0)) as i64
        );
    }

    // And the vectors really do reach all three exits, or the saturation arms would be untested.
    assert_eq!(predicted_millis(limit + 1, 0, 0, 0), i64::MAX);
    assert_eq!(predicted_millis(-limit - 1, 0, 0, 0), i64::MIN);
    assert_eq!(predicted_millis(limit, 0, 0, 0), 9_223_372_036_854_774_000);
    assert_eq!(predicted_millis(5, 250_000, 3, 100_000), 2_150);
}

/// **D13, on real engine code, in all three directions.**
///
/// The positive direction alone would be weak: a function returning a constant returns it whatever
/// the thread pointer holds, unless the guard check really runs. So the failure tail's `BL` is
/// registered as a thunk — if it were ever taken, the exit would say so — and then it is *made* to
/// be taken by changing the guard between the two reads, and made to fault by pointing
/// `TPIDR_EL0` somewhere unmapped.
#[test]
fn real_guest_code_reads_the_thread_pointer_and_finds_the_stack_guard() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    assert_words(&roblox, STACK_GUARD_LEAF, &STACK_GUARD_LEAF_WORDS);
    let entry = roblox.at(STACK_GUARD_LEAF);
    let stub = roblox.at(STACK_CHK_FAIL_STUB);

    // ---- one: the guard matches, so the function returns and the call is not taken. ----
    let mut cpu = roblox.thread();
    cpu.add_thunk(stub).expect("plant a thunk on __stack_chk_fail");
    let tls = cpu.tls().expect("the context allocated its own TLS block");
    let (thread_pointer, guard) = (tls.thread_pointer(), tls.stack_guard());
    assert_eq!(cpu.tpidr_el0(), thread_pointer);
    assert_ne!(guard, 0, "a zero stack guard would make the check vacuous");
    assert_eq!(
        roblox.read_u64(thread_pointer + 0x28),
        guard,
        "the guard must really be at TLS_SLOT_STACK_GUARD, +0x28 from the thread pointer (D13)"
    );

    let exit = call(&roblox, &mut cpu, entry);
    assert_eq!(
        exit,
        ExitReason::Returned { pc: roblox.sentinel },
        "the guard matched, so the function must return rather than reach __stack_chk_fail"
    );
    assert_eq!(cpu.x(x(0)), STACK_GUARD_LEAF_RESULT);

    // ---- two: change the guard between the two reads, and the call IS taken. ----
    // Without this the test above would pass on a runtime that never executed the comparison.
    let mut cpu = roblox.thread();
    cpu.add_thunk(stub).expect("plant a thunk");
    let (thread_pointer, guard) = {
        let tls = cpu.tls().expect("a TLS block");
        (tls.thread_pointer(), tls.stack_guard())
    };
    let reload = entry + STACK_GUARD_RELOAD_OFFSET as usize;
    cpu.add_breakpoint(reload).expect("break before the second read of the guard");

    roblox.rearm(&mut cpu);
    let exit = cpu.run(entry, RunLimit::Instructions(100_000)).expect("the guest ran");
    assert_eq!(
        exit,
        ExitReason::Breakpoint { pc: reload },
        "execution must stop before re-reading the guard"
    );
    assert_eq!(
        roblox.read_u64(thread_pointer + 0x28),
        guard,
        "and the canary on the frame came from here"
    );

    roblox.write_u64(thread_pointer + 0x28, !guard);
    cpu.remove_breakpoint(reload).expect("remove the breakpoint");
    let exit = cpu.run(reload, RunLimit::Instructions(100_000)).expect("the guest ran on");
    assert_eq!(
        exit,
        ExitReason::Thunk { pc: stub },
        "with the guard changed under it, real engine code must call __stack_chk_fail -- which is \
         what proves the value it compared came from [TPIDR_EL0, #0x28] and not from thin air"
    );
    roblox.write_u64(thread_pointer + 0x28, guard);

    // ---- three: point the thread pointer at nothing, and the read faults at +0x28. ----
    // This pins the offset itself: the fault address is the thread pointer plus exactly 0x28.
    let mut cpu = roblox.thread();
    let nowhere = roblox.unmapped();
    cpu.set_tpidr_el0(nowhere);
    roblox.rearm(&mut cpu);
    let exit = cpu.run(entry, RunLimit::Instructions(100_000)).expect("a guest fault is an exit");
    assert_eq!(
        exit,
        ExitReason::MemoryFault {
            pc: entry + 4 * 4,
            address: nowhere + 0x28,
            access: AccessKind::Read
        },
        "the first stack-protected instruction must fault at TPIDR_EL0 + 0x28 (D13)"
    );
}

/// An instruction the backend cannot run produces a typed exit **naming it**, out of real engine
/// code rather than a hand-built encoding: `CAS W0, W1, [X2]`, one of the LSE atomics D5 measured
/// as unimplemented.
#[test]
fn a_real_unimplemented_instruction_produces_a_typed_exit_naming_it() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    let at = roblox.at(REAL_LSE_ATOMIC);
    assert_eq!(roblox.word_at(at), REAL_LSE_ATOMIC_WORD);

    let mut cpu = roblox.thread();
    roblox.rearm(&mut cpu);
    let exit = cpu.run(at, RunLimit::Instructions(100_000)).expect("an unsupported instruction is an exit, not an error");

    match exit {
        ExitReason::UnsupportedInstruction { pc, encoding } => {
            assert_eq!(pc, at);
            assert_eq!(
                encoding, REAL_LSE_ATOMIC_WORD,
                "the exit has to carry the encoding, or nobody can tell which of the 231 \
                 unimplemented decoder entries was hit"
            );
            assert!(exit.to_string().contains("0x88a07c41"), "{exit}");
            assert!(!exit.is_resumable());
        }
        other => panic!("expected an unsupported-instruction exit, got {other}"),
    }
}

/// Real guest code branching to an address with no executable mapping stops as a typed exit rather
/// than taking the host process with it.
///
/// The branch is the function's own `RET`, with `X30` pointed at nothing — which is exactly the
/// shape a corrupted stack produces in a real crash.
#[test]
fn real_guest_code_jumping_to_an_unmapped_address_produces_a_typed_exit() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    let entry = roblox.at(BASE64_SEXTET);
    let nowhere = roblox.unmapped() & !3;

    let mut cpu = roblox.thread();
    cpu.set_x(x(1), u64::from(b'Q'));
    cpu.set_x(x(30), nowhere as u64);
    let exit = cpu.run(entry, RunLimit::Instructions(100_000)).expect("a guest fault is an exit");

    assert_eq!(
        exit,
        ExitReason::MemoryFault { pc: nowhere, address: nowhere, access: AccessKind::Execute },
        "a guest branch into unmapped memory must be a typed exit naming the address"
    );
    assert!(!exit.is_resumable());
    // The function still computed its answer before returning into nothing, which is what makes
    // this a fault on the *branch* rather than a run that never started.
    assert_eq!(cpu.x(x(0)), u64::from(predicted_sextet(b'Q') as u32));
}

/// Execution is repeatable and leak-free: the same inputs give the same answers a thousand times
/// over, and guest threads that come and go give their commit charge back.
#[test]
fn execution_is_repeatable_and_leak_free() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    let entry = roblox.at(BASE64_SEXTET);

    // Repeatable, on one context: 1,000 calls, every answer checked against the model.
    let mut cpu = roblox.thread();
    const REPEATS: usize = 1_000;
    for i in 0..REPEATS {
        let c = BASE64_ALPHABET[i % 64];
        cpu.set_x(x(1), u64::from(c));
        assert_eq!(call(&roblox, &mut cpu, entry), ExitReason::Returned { pc: roblox.sentinel });
        assert_eq!(cpu.x(x(0)), u64::from(predicted_sextet(c) as u32), "call {i}");
    }
    assert_eq!(cpu.degraded_slices(), 0, "and no slice degraded onto the callback path");
    drop(cpu);

    // Leak-free, across contexts. The counter is process-global, which is why this whole binary is
    // serialized; the baseline is taken after one context has already been created and dropped, so
    // that first-time allocator growth is not charged to the loop.
    {
        let mut warm = roblox.thread();
        warm.set_x(x(1), u64::from(b'A'));
        call(&roblox, &mut warm, entry);
    }
    let baseline = omni_mem::process_commit_charge().expect("commit charge");
    const CYCLES: usize = 24;
    for _ in 0..CYCLES {
        let mut cpu = roblox.thread();
        for c in [b'A', b'z', b'9', b'+', b'/', b'='] {
            cpu.set_x(x(1), u64::from(c));
            assert_eq!(call(&roblox, &mut cpu, entry), ExitReason::Returned { pc: roblox.sentinel });
            assert_eq!(cpu.x(x(0)), u64::from(predicted_sextet(c) as u32));
        }
    }
    let after = omni_mem::process_commit_charge().expect("commit charge");

    // Each cycle creates and destroys a jit whose code cache alone is measured in tens of MiB, so a
    // leak of even one per cycle would be hundreds of MiB. The tolerance is for allocator
    // retention, not for a leaked context.
    const TOLERANCE: u64 = 4 * 1024 * 1024;
    let residue = after.saturating_sub(baseline);
    println!(
        "after {CYCLES} create/run/drop cycles: {:+} bytes ({:.3} MiB) against baseline",
        after as i64 - baseline as i64,
        residue as f64 / 1048576.0
    );
    assert!(
        residue < TOLERANCE,
        "{CYCLES} guest threads came and went and left {residue} bytes of commit charge behind, \
         which is more than the {TOLERANCE}-byte allowance for allocator retention"
    );
}

/// **The per-thread CPU memory cost, measured and held to a ceiling.**
///
/// D5 measured 20-35 MiB of unshared code cache per guest thread against dynarmic's 128 MiB
/// default, and named it a primary risk against D10. This backend defaults the cache to 8 MiB, and
/// the dominant term turned out not to be the cache at all — upstream `A64EmitX64` holds a
/// `std::array<FastDispatchEntry, 0x100000>`, a flat 16 MiB per jit, constructed and written
/// whether or not the FastDispatch optimization is on, and this backend turns it off (D16).
/// Patch 0017 (D32) allocates it only when it is on, and trims the cache's up-front commit.
///
/// The figure is measured with `process_commit_charge`, which sees dynarmic's cache because the
/// cache is a private `VirtualAlloc` commit rather than a section (D15's invisible half is the
/// *arena*, which this backend does not use).
#[test]
fn the_per_thread_cpu_cost_is_measured_and_under_its_ceiling() {
    let _serial = serialized();
    const THREADS: usize = 8;
    let Some(roblox) = Roblox::with_options(DynarmicOptions {
        max_threads: THREADS as u32,
        ..Default::default()
    }) else {
        return;
    };
    let entry = roblox.at(BASE64_SEXTET);

    // One context first, so that any one-off growth is outside the window being measured.
    {
        let mut warm = roblox.thread();
        warm.set_x(x(1), u64::from(b'A'));
        call(&roblox, &mut warm, entry);
    }

    let baseline = omni_mem::process_commit_charge().expect("commit charge");
    let mut threads: Vec<DynarmicCpu> = (0..THREADS).map(|_| roblox.thread()).collect();
    let created = omni_mem::process_commit_charge().expect("commit charge");
    for cpu in &mut threads {
        cpu.set_x(x(1), u64::from(b'Z'));
        assert_eq!(call(&roblox, cpu, entry), ExitReason::Returned { pc: roblox.sentinel });
        assert_eq!(cpu.x(x(0)), 25);
    }
    let warm = omni_mem::process_commit_charge().expect("commit charge");

    let per_thread_created = (created - baseline) / THREADS as u64;
    let per_thread_warm = (warm - baseline) / THREADS as u64;
    let reported: usize = threads.iter().map(|c| c.cost().total()).sum::<usize>() / THREADS;
    println!(
        "per guest thread (n = {THREADS} threads, 1 measurement): {:.3} MiB at creation, \
         {:.3} MiB after translating real Roblox code; GuestCpu::cost() reports {:.3} MiB \
         (the TLS page, plus the 16 MiB fast-dispatch table only when FastDispatch is on)",
        per_thread_created as f64 / 1048576.0,
        per_thread_warm as f64 / 1048576.0,
        reported as f64 / 1048576.0,
    );

    // **The omission is bounded, not merely admitted.** `cost()` reports two derived terms and
    // leaves out the code cache's committed high-water mark, which this pin gives no way to read.
    // Both ends of that are asserted here: the reported figure never exceeds what was measured, and
    // what it misses never exceeds `code_cache_size`, which is the public ceiling on the missing
    // term. Without the second half, "we omit the cache" would be compatible with omitting anything
    // at all.
    let options = roblox.backend.options();
    assert!(
        reported as u64 <= per_thread_warm,
        "cost() reports {reported} bytes per thread but only {per_thread_warm} were measured, so \
         it is no longer a floor"
    );
    let missing = per_thread_warm - reported as u64;
    assert!(
        missing <= options.code_cache_size + MAX_OTHER_PER_JIT_BYTES,
        "cost() misses {missing} bytes per thread, past the {} bytes of code cache plus the \
         {MAX_OTHER_PER_JIT_BYTES}-byte allowance for dynarmic's other per-jit state -- so \
         something else grew",
        options.code_cache_size
    );
    println!(
        "  cost() is a floor: it misses {:.3} MiB, of which at most {:.3} MiB is the code cache \
         and the remaining {:.3} MiB is dynarmic's other per-jit state plus the counter's noise",
        missing as f64 / 1048576.0,
        options.code_cache_size as f64 / 1048576.0,
        missing.saturating_sub(options.code_cache_size) as f64 / 1048576.0,
    );

    assert!(
        per_thread_warm <= MAX_THREAD_COMMIT_BYTES,
        "a guest thread cost {per_thread_warm} bytes ({:.3} MiB), past the {:.0} MiB ceiling. \
         D5 records 20-35 MiB/thread as a primary risk against D10, so this growing is a \
         regression in the thing the ceiling exists to watch",
        per_thread_warm as f64 / 1048576.0,
        MAX_THREAD_COMMIT_BYTES as f64 / 1048576.0
    );
    // And a floor, so the ceiling cannot be met by a measurement that measured nothing: creating
    // eight jits has to move the counter.
    //
    // On macOS the counter is `phys_footprint`, charged when a page is touched, and a new jit
    // touches little: dynarmic-sys patch 0009 stopped the prelude from invalidating -- and so
    // faulting in -- the whole code cache. MEASURED there: 0.027 and 0.029 MiB per thread at
    // creation (two runs, n = 8 each). So on macOS the floor is the one host page a jit writes its
    // prelude into, and the instrument is shown seeing 16 MiB being touched, so that a floor met
    // is not a counter that sees nothing.
    #[cfg(target_os = "macos")]
    {
        let before = omni_mem::process_commit_charge().expect("commit charge");
        let touched = vec![1u8; 16 << 20];
        std::hint::black_box(&touched);
        let grew = omni_mem::process_commit_charge().expect("commit charge").saturating_sub(before);
        assert!(
            grew >= 15 << 20,
            "touching 16 MiB moved the commit charge by {grew} bytes -- the measurement is not \
             measuring what it claims"
        );
    }
    let floor = if cfg!(target_os = "macos") { omni_platform::vm::page_size() as u64 } else { 1024 * 1024 };
    assert!(
        per_thread_created >= floor,
        "creating a guest thread moved the commit charge by {per_thread_created} bytes, which is \
         too little to be a real jit -- the measurement is not measuring what it claims"
    );
    drop(threads);
}

/// What dynarmic's per-jit state may cost **beyond** the code cache and the fixed table.
///
/// **Fitted, not derived, and it is here because the obvious bound turned out to be wrong.** The
/// first version of this assertion bounded the term `GuestCpu::cost()` omits by `code_cache_size`
/// alone, on the reasoning that the cache is the one thing it knowingly leaves out. It is not:
/// measured, the gap is **8.52 MiB** against an 8 MiB cache, so about **536 KiB per jit** is
/// something else — `JitState`, the block-range map, xbyak's labels, the shim's own allocation and
/// ours, plus whatever the process-global counter picked up in the window.
///
/// 2 MiB is that 536 KiB with roughly 3.8x of headroom. It bounds "something else grew" without
/// pretending the residue is understood; naming it is the point, because a gap that is merely
/// admitted can absorb any regression at all.
const MAX_OTHER_PER_JIT_BYTES: u64 = 2 * 1024 * 1024;

/// The ceiling one guest thread's CPU context may cost in commit charge.
///
/// **Fitted to the measurement, not derived, and said so.** This backend measures **24.5 MiB** per
/// guest thread after translating real Roblox code (n = 8 threads, 1 measurement, serialized) —
/// which decomposes as the 16 MiB `FastDispatchEntry` table plus the 8 MiB code cache plus the
/// 4 KiB TLS page, and therefore tracks `code_cache_size` rather than translated volume.
///
/// **Since patch 0017 (D32) it measures 4.47 MiB** (2026-09-24, n = 8 threads, 1 measurement):
/// the table is not allocated with FastDispatch off, and the cache commits 2 MiB of prelude
/// beyond its 2 MiB constant pool instead of 16 -- so the figure now tracks what was translated.
///
/// 6 MiB is that figure with about 34% of headroom for the process-global counter's noise, and it
/// is the detector for both halves of the patch, each MEASURED by hand-applying it (2026-09-24):
/// the table allocated again, 20.48 MiB; the 16 MiB prelude commit back, 8.49 MiB (capped at this
/// test's 8 MiB cache). The 32 MiB this was before 0017 could see neither.
///
/// **On Linux the counter charges the whole code cache at once**, so there it is 10 MiB. The
/// counter there is `Committed_AS` (`omni_platform::vm::process_commit_charge`), which the kernel
/// charges for a private writable mapping when it is *made*; dynarmic's cache is one such `mmap`,
/// and its `EnsureMemoryCommitted` -- the step 0017 trims -- is a Windows mechanism with nothing to
/// do on a host with no reserve/commit split. MEASURED on the Linux x86-64 host after the merge:
/// 8.364 MiB per thread at creation, 8.410 after translating, i.e. the 8 MiB cache plus the same
/// residue. So the prelude half of 0017 is unobservable there, and 10 MiB (the cache plus
/// [`MAX_OTHER_PER_JIT_BYTES`]) keeps the half that is: the 16 MiB table allocated again would read
/// 24 MiB. macOS's counter (`phys_footprint`) counts touched pages, as Windows' does commits, and
/// is held to 6 MiB.
#[cfg(not(target_os = "linux"))]
const MAX_THREAD_COMMIT_BYTES: u64 = 6 * 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_THREAD_COMMIT_BYTES: u64 = 8 * 1024 * 1024 + MAX_OTHER_PER_JIT_BYTES;

/// The per-slice callback invariant, on real engine code: armed, and clean across a whole sweep.
#[test]
fn the_per_slice_invariant_is_armed_and_real_roblox_code_never_trips_it() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    assert!(
        roblox.backend.slice_invariant_armed(),
        "the invariant must really be in force, or this test asserts nothing"
    );
    assert!(roblox.backend.owns_guest_paging(), "Omnidroid must own guest page faults (D10)");

    let mut cpu = roblox.thread();
    for (vaddr, setup) in [
        (BASE64_SEXTET, 1u8),
        (STACK_GUARD_LEAF, 0),
        (TIMEVAL_TO_MILLIS, 0),
    ] {
        let entry = roblox.at(vaddr);
        for c in 0u8..=64 {
            cpu.set_x(x(1), u64::from(c.wrapping_add(setup)));
            let exit = call(&roblox, &mut cpu, entry);
            assert_eq!(exit, ExitReason::Returned { pc: roblox.sentinel }, "{vaddr:#x}");
        }
    }
    assert_eq!(cpu.slow_path_entries(), 0, "no guest access left the fast path");
    assert_eq!(cpu.degraded_slices(), 0);
}

// ---------------------------------------------------------------------------------------------
// Measurements. `#[ignore]`d so an ordinary `cargo test` does not pay for them:
//
//   cargo test -p omni-cpu --release --test roblox -- --ignored --nocapture
//
// Release only; a debug build measures the harness rather than the jit.
// ---------------------------------------------------------------------------------------------

/// **Cold and warm translation throughput on real Roblox code.**
///
/// Every measurement in this project so far has been synthetic: hand-written loops of four to eight
/// instructions, chosen to isolate one effect. This one runs the engine's own code — every leaf the
/// scan found that needs nothing but registers, a stack and a thread pointer — which is a different
/// shape in three ways that matter, and the differences are what the Task 4 report is for.
///
/// The figures to compare against are D5's: **0.15-0.31 Mguest-insn/s cold**, and a steady state of
/// about 2.0x native on memory-heavy code, 2.2x on NEON/FP and **33x on register-bound integer
/// code**. These functions are overwhelmingly the third kind, so 33x is the row they belong in.
#[test]
#[ignore = "measurement, not a test"]
fn cold_and_warm_translation_throughput_on_real_roblox_code() {
    use std::time::{Duration, Instant};

    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    let bytes = harness::roblox::main_lib_bytes().expect("the library bytes");
    let elf = omni_elf::ElfImage::parse(bytes).expect("parse libroblox.so");

    let scan_started = Instant::now();
    let leaves = omni_elf::leaf::find_leaves(&elf).expect("scan for leaves");
    let scan_elapsed = scan_started.elapsed();

    // Everything the scan graded as runnable. `LeafKind::NotALeaf` never appears here.
    let entries: Vec<(GuestAddr, u64)> =
        leaves.iter().map(|l| (roblox.at(l.bounds.start), l.bounds.len)).collect();
    let body_bytes: u64 = leaves.iter().map(|l| l.bounds.len).sum();

    /// One pass over every candidate, on one context. Returns the wall time, the guest instructions
    /// executed, and how each function ended.
    fn pass(
        roblox: &Roblox,
        cpu: &mut DynarmicCpu,
        entries: &[(GuestAddr, u64)],
        keep: &mut [bool],
    ) -> (Duration, u64, [usize; 4]) {
        let mut executed = 0u64;
        let mut outcomes = [0usize; 4];
        let started = Instant::now();
        for (i, &(entry, _)) in entries.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            // A fixed, arbitrary argument pattern. These are real functions with real argument
            // conventions we do not know, so the inputs are not meaningful — what is being measured
            // is translation and execution, not results, and the results are checked elsewhere.
            for r in 0..8u8 {
                cpu.set_x(x(r), 0x0101_0101_0101_0101u64.wrapping_mul(u64::from(r) + 1));
            }
            roblox.rearm(cpu);
            match cpu.run(entry, RunLimit::Instructions(200_000)) {
                Ok(ExitReason::Returned { .. }) => outcomes[0] += 1,
                Ok(ExitReason::UnsupportedInstruction { .. }) => {
                    outcomes[1] += 1;
                    keep[i] = false;
                }
                Ok(_) => {
                    outcomes[2] += 1;
                    keep[i] = false;
                }
                Err(_) => {
                    outcomes[3] += 1;
                    keep[i] = false;
                }
            }
            executed += cpu.last_run_instructions();
        }
        (started.elapsed(), executed, outcomes)
    }

    // A dry pass on a throwaway context, to find the functions that do not simply return. They are
    // excluded from the timed passes so that cold and warm measure the same work; how many there
    // are, and why, is itself a finding.
    let mut keep = vec![true; entries.len()];
    {
        let mut probe = roblox.thread();
        let (_, _, outcomes) = pass(&roblox, &mut probe, &entries, &mut keep);
        println!("\n== real libroblox.so leaf functions ==");
        println!(
            "  .eh_frame_hdr names 245,117 functions; the scan took {:.3} s and graded {} of them \
             runnable ({} bytes of body)",
            scan_elapsed.as_secs_f64(),
            entries.len(),
            body_bytes
        );
        println!(
            "  first (untimed) pass: {} returned, {} hit an unimplemented instruction, {} ended \
             another way, {} errored",
            outcomes[0], outcomes[1], outcomes[2], outcomes[3]
        );
    }
    let kept = keep.iter().filter(|k| **k).count();

    // Cold: **n passes, each on a brand-new context**, so every block is translated for the first
    // time in every sample. The first version of this measurement was n = 1, which is not a
    // measurement of anything repeatable.
    const COLD_N: usize = 11;
    let mut cold_samples = Vec::with_capacity(COLD_N);
    let mut cold_executed = 0u64;
    for _ in 0..COLD_N {
        let mut fresh = roblox.thread();
        let mut k = keep.to_vec();
        let (elapsed, executed, _) = pass(&roblox, &mut fresh, &entries, &mut k);
        cold_executed = executed;
        cold_samples.push(elapsed);
    }
    cold_samples.sort_unstable();
    let cold = cold_samples[COLD_N / 2];

    // Warm: one context, run repeatedly, so nothing is translated at all.
    const N: usize = 31;
    let mut cpu = roblox.thread();
    let mut samples = Vec::with_capacity(N);
    let mut warm_executed = 0u64;
    for _ in 0..N + 1 {
        let mut k = keep.to_vec();
        let (elapsed, executed, _) = pass(&roblox, &mut cpu, &entries, &mut k);
        warm_executed = executed;
        samples.push(elapsed);
    }
    // Drop the first, which is this context's cold pass.
    samples.remove(0);
    samples.sort_unstable();
    let warm = samples[N / 2];

    // **What fraction of the "per call" figure is the call?** The timed region also holds eight
    // `set_x` calls, `rearm`, and about ten guest instructions of real work, so the per-call number
    // is an upper bound on the boundary and not the boundary. This measures the harness half of it
    // directly, by doing everything except the `run`.
    let mut setup_samples = Vec::with_capacity(N);
    for _ in 0..N {
        let t = Instant::now();
        for _ in 0..kept {
            for r in 0..8u8 {
                cpu.set_x(x(r), 0x0101_0101_0101_0101u64.wrapping_mul(u64::from(r) + 1));
            }
            roblox.rearm(&mut cpu);
        }
        setup_samples.push(t.elapsed());
    }
    setup_samples.sort_unstable();
    let setup = setup_samples[N / 2];

    // And how cold translation scales with function length, which is one testable half of the
    // "short functions are cheaper per instruction" hypothesis: if translation cost were dominated
    // by a per-function overhead, the short third would be far worse per instruction.
    let mut by_length: Vec<usize> = (0..entries.len()).filter(|&i| keep[i]).collect();
    by_length.sort_by_key(|&i| entries[i].1);
    let third = by_length.len() / 3;
    let mut thirds = Vec::new();
    for (name, slice) in [
        ("shortest third", &by_length[..third]),
        ("longest third", &by_length[by_length.len() - third..]),
    ] {
        let mut only = vec![false; entries.len()];
        for &i in slice {
            only[i] = true;
        }
        let mut fresh = roblox.thread();
        let mut k = only.clone();
        let (elapsed, executed, _) = pass(&roblox, &mut fresh, &entries, &mut k);
        let bytes: u64 = slice.iter().map(|&i| entries[i].1).sum();
        thirds.push((name, elapsed, executed, bytes, slice.len()));
    }

    let m = |insns: u64, d: Duration| insns as f64 / d.as_secs_f64() / 1e6;
    println!(
        "  {kept} functions run to completion, {cold_executed} guest instructions per pass \
         ({:.2} per function)",
        cold_executed as f64 / kept as f64
    );
    println!(
        "  cold (n = {COLD_N} passes, median, a fresh context each time) : {:8.3} ms, \
         {:8.3} Mguest-insn/s  [{:7.3} .. {:7.3}]",
        cold.as_secs_f64() * 1e3,
        m(cold_executed, cold),
        cold_samples[0].as_secs_f64() * 1e3,
        cold_samples[COLD_N - 1].as_secs_f64() * 1e3,
    );
    println!(
        "  warm (n = {N} passes, median, no translation) \
         : {:8.3} ms, \
         {:8.1} Mguest-insn/s",
        warm.as_secs_f64() * 1e3,
        m(warm_executed, warm)
    );
    let translation = cold.saturating_sub(warm);
    println!(
        "  translation alone (cold - warm) \
         : {:8.3} ms for \
         {cold_executed} guest instructions, {:8.3} Mguest-insn/s",
        translation.as_secs_f64() * 1e3,
        m(cold_executed, translation)
    );
    println!(
        "  cold/warm ratio: {:.1}x. D5 measured cold translation at 0.15-0.31 Mguest-insn/s on \
         synthetic code.",
        cold.as_secs_f64() / warm.as_secs_f64()
    );
    // The warm figure is NOT a steady-state translated-code throughput and must not be read as
    // one. The average function here is ten instructions long, so a warm pass is 870 entries to
    // and exits from `od_jit_run` around 8,679 instructions of work. And even *that* is an upper
    // bound on the boundary, because the timed region also holds the register setup measured
    // above.
    let per_call_ns = warm.as_secs_f64() * 1e9 / kept as f64;
    let setup_ns = setup.as_secs_f64() * 1e9 / kept as f64;
    println!(
        "  warm per iteration: {per_call_ns:6.1} ns, of which {setup_ns:5.1} ns is 8 set_x plus \
         rearm, leaving {:5.1} ns for the run loop, the dispatcher round trip AND ~{:.0} guest \
         instructions -- so THAT is the upper bound on the call boundary itself, not the {:.1} ns",
        per_call_ns - setup_ns,
        cold_executed as f64 / kept as f64,
        per_call_ns,
    );
    println!("  cold translation against function length (n = 1 pass each, fresh context):");
    for (name, elapsed, executed, bytes, count) in &thirds {
        println!(
            "    {name:<15} {count:>4} functions, {:>6} body bytes, {executed:>6} insns : \
             {:8.3} ms, {:7.3} Mguest-insn/s",
            bytes,
            elapsed.as_secs_f64() * 1e3,
            m(*executed, *elapsed)
        );
    }
    println!(
        "    If translation were dominated by a per-function overhead the shortest third would be \
         far worse per instruction. **The causal claim in the report -- that short functions \
         translate faster per instruction than loop bodies because the IR optimizer has less to \
         work over -- is a HYPOTHESIS these figures are consistent with, not one they establish.** \
         Testing it properly means instrumenting dynarmic's optimization passes, or translating the \
         same instruction count as a loop and as straight-line code in this harness."
    );
    println!(
        "  callback-path entries across every pass: {} (D4 says this must be 0 for code that \
         only touches its own stack)",
        cpu.slow_path_entries()
    );
}
