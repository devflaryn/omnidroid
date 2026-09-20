//! **Hostile guest code, which is the expected case** (Global Constraint 11, D6).
//!
//! Every test here is real guest code doing something a well-behaved program would not, and every one
//! asserts that the result is a *typed refusal naming the symbol* — not a panic, not an abort, not a
//! plausible zero. A panic or abort reachable from guest-supplied arguments is Critical, and an abort
//! cannot be contained by any caller.
//!
//! The list the brief gives, and where each one is:
//!
//! | Hostile shape | Test |
//! |---|---|
//! | a null pointer | [`a_null_pointer_argument_is_refused_by_name`] |
//! | a wild pointer | [`a_wild_pointer_argument_is_refused_by_name`] |
//! | lying about a length | [`a_length_the_guest_lied_about_is_refused_whole`] |
//! | re-entering from inside a callback | [`recursion_through_the_boundary_stops_at_the_depth_limit`] |
//! | branching into the middle of a thunk | [`a_branch_into_the_middle_of_a_thunk_names_the_symbol_and_the_offset`] |
//!
//! Plus the ones this boundary's own shape adds: an unbound symbol, a data symbol called as a
//! function, a thunk reached by a tail call with a wild link register, a callback entered with a
//! misaligned stack pointer, a callback that faults, and a guest that spins through the exit path.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::sync::Mutex;

use harness::a64::*;
use harness::{serialized, x, Asm, Guest, BUDGET};
use omni_android::{AbiError, AbiResult, GuestArg, ImportCall, ReentrantCall, SLOT_BYTES};
use omni_cpu::{AccessKind, ExitReason, GuestAddr, GuestCpu, RunLimit};

/// The one piece of state a handler needs, for the tests that give it a callback to call.
static TARGET: Mutex<GuestAddr> = Mutex::new(0);

fn set_target(address: GuestAddr) {
    *TARGET.lock().unwrap_or_else(|p| p.into_inner()) = address;
}

fn target() -> GuestAddr {
    *TARGET.lock().unwrap_or_else(|p| p.into_inner())
}

// --------------------------------------------------------------------------------- bad pointers

/// `size_t len(const char* s)` — the simplest thing that dereferences what the guest handed over.
fn len(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let pointer = call.args().next_pointer()?;
    let text = call.mem().cstr(pointer, call.blame(0))?;
    call.ret().u64(text.len() as u64);
    Ok(())
}

/// `void copy(void* dst, const void* src, size_t n)` — three guest-supplied values, and `n` is the
/// one the guest lies about.
fn copy(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    let (dst, src, n) = {
        let mut args = call.args();
        (args.next_pointer()?, args.next_pointer()?, args.next_u64()?)
    };
    let n = usize::try_from(n).map_err(|_| AbiError::UnsupportedShape {
        symbol: call.symbol().to_string(),
        address: call.address(),
        shape: "a length that does not fit in a host usize",
    })?;
    let bytes = call.mem().read_bytes(src, n, call.blame(1))?;
    call.mem().write_bytes(dst, &bytes, call.blame(0))?;
    call.ret().u64(n as u64);
    Ok(())
}

/// Call `thunk` with `X0`-`X2` set to `args`, and return whatever the boundary said.
fn call_with(
    guest: &Guest,
    boundary: &std::sync::Arc<omni_android::Boundary>,
    thunk: GuestAddr,
    args: &[u64],
) -> Result<ExitReason, AbiError> {
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    for (index, value) in args.iter().enumerate() {
        asm.mov(index as u32, *value);
    }
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());
    let mut cpu = guest.thread(boundary);
    boundary.run(&mut cpu, entry, BUDGET)
}

#[test]
fn a_null_pointer_argument_is_refused_by_name() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    let error = call_with(&guest, &boundary, thunk, &[0]).expect_err("null must be refused");
    match &error {
        AbiError::BadPointer { symbol, pointer, address, access, .. } => {
            assert_eq!(symbol, "strlen");
            assert_eq!(*pointer, 0);
            assert_eq!(*address, thunk, "and the thunk address, which is what names the symbol");
            assert_eq!(*access, "reading");
        }
        other => panic!("{other:?}"),
    }
    // The message is what somebody reads 3,000 initializers deep, so it is asserted and not assumed.
    let text = error.to_string();
    assert!(text.contains("strlen") && text.contains(&format!("{thunk:#x}")), "{text}");
}

#[test]
fn a_wild_pointer_argument_is_refused_by_name() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    // Three shapes of wild: a hole inside the guest's own address space, a value far outside it, and
    // the largest address there is.
    for pointer in [guest.unmapped as u64, 0x1234_5678_9ABC, u64::MAX] {
        let error = call_with(&guest, &boundary, thunk, &[pointer])
            .expect_err("a wild pointer must be refused, not dereferenced");
        assert!(
            matches!(&error, AbiError::BadPointer { pointer: p, .. } if *p as u64 == pointer),
            "{pointer:#x}: {error:?}"
        );
        assert_eq!(error.symbol(), Some("strlen"), "{pointer:#x}");
    }
}

/// **A string that is mapped and never ends.** The walk is bounded by the region, which is a fact
/// about the mapping rather than a promise from the guest.
#[test]
fn a_string_the_guest_never_terminated_is_refused_at_the_end_of_its_region() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    // Fill the guest's whole data region with non-NUL bytes.
    guest.write_bytes(guest.data, &vec![b'Z'; harness::DATA_BYTES]);
    let error = call_with(&guest, &boundary, thunk, &[(guest.data + 0x100) as u64])
        .expect_err("an unterminated string must be refused");
    match error {
        AbiError::Unterminated { symbol, limit, .. } => {
            assert_eq!(symbol, "strlen");
            assert_eq!(
                limit,
                harness::DATA_BYTES - 0x100,
                "the walk stopped at the end of the region, not at the policy cap"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// **The guest lies about a length.** A pointer to eight valid bytes with a claimed length of 64 KiB:
/// the range must be refused *whole*, not truncated to what fits.
#[test]
fn a_length_the_guest_lied_about_is_refused_whole() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("memcpy", copy).expect("bind");
    let boundary = builder.finish();

    let src = guest.data + harness::DATA_BYTES - 8;
    let dst = guest.data;
    let error = call_with(&guest, &boundary, thunk, &[dst as u64, src as u64, 65536])
        .expect_err("a length running off the end of the mapping must be refused");
    match error {
        AbiError::BadPointer { symbol, len, pointer, argument, .. } => {
            assert_eq!(symbol, "memcpy");
            assert_eq!(len, 65536, "the length reported is the one the guest claimed");
            assert_eq!(pointer, src);
            assert_eq!(argument, 1, "the source, which is the argument that was wrong");
        }
        other => panic!("{other:?}"),
    }
    // Nothing was copied. A boundary that had truncated the length would have copied eight bytes and
    // returned, which is the failure this test exists to exclude.
    assert_eq!(guest.read_u64(dst), 0);

    // An absurd length that does not even fit a host `usize` is refused too, rather than wrapping.
    let error = call_with(&guest, &boundary, thunk, &[dst as u64, src as u64, u64::MAX])
        .expect_err("u64::MAX bytes must be refused");
    assert!(
        matches!(error, AbiError::BadPointer { len: usize::MAX, .. } | AbiError::UnsupportedShape { .. }),
        "{error:?}"
    );
    assert_eq!(guest.read_u64(dst), 0);
}

/// A guest handing its own read-only mapping over as an output buffer.
#[test]
fn a_write_to_a_mapping_the_guest_made_read_only_is_refused_with_the_access_named() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("memcpy", copy).expect("bind");
    let boundary = builder.finish();

    let error = call_with(&guest, &boundary, thunk, &[guest.readonly as u64, guest.data as u64, 8])
        .expect_err("a write to read-only guest memory must be refused");
    match error {
        AbiError::BadPointer { access, argument, .. } => {
            assert_eq!(access, "writing", "which is what says it was the destination");
            assert_eq!(argument, 0);
        }
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------------------ the region's own shapes

/// **Branching into the middle of a thunk.** A relocation applied at the wrong width, or a guest
/// jumping four bytes past a slot's start, must be told apart from a call to an unbound symbol —
/// reporting it as "unbound" sends a reader looking for an implementation that is not missing.
#[test]
fn a_branch_into_the_middle_of_a_thunk_names_the_symbol_and_the_offset() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    // Every word of the slot but its first, so the answer is not right for one offset by luck.
    for into in [4usize, 8, 12] {
        let entry = guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        asm.bl(thunk + into);
        asm.push(ret(21));
        guest.load(asm.words());

        let mut cpu = guest.thread(&boundary);
        let error = boundary
            .run(&mut cpu, entry, BUDGET)
            .expect_err("a branch into the middle of a slot is not a call");
        match error {
            AbiError::MidThunk { symbol, slot, address, offset } => {
                assert_eq!(symbol, "strlen");
                assert_eq!(slot, thunk);
                assert_eq!(address, thunk + into);
                assert_eq!(offset, into);
            }
            other => panic!("offset {into}: {other:?}"),
        }
    }
    // A slot bigger than a word is what makes this shape exist at all: with 4-byte slots there
    // would be no "inside a slot" to branch to.
    assert_eq!(SLOT_BYTES, 16);
}

/// An address in the region that no symbol was ever given.
#[test]
fn a_branch_to_an_unallocated_address_in_the_region_names_the_region() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(64);
    builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    // Well past the two slots that exist — the sentinel's and `strlen`'s.
    let nowhere = boundary.region().functions_start() + 40 * SLOT_BYTES;
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(nowhere);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("nothing is bound there");
    match error {
        AbiError::NoSuchThunk { address, start, end } => {
            assert_eq!(address, nowhere);
            assert_eq!(start, boundary.region().functions_start());
            assert_eq!(end, boundary.region().functions_end());
        }
        other => panic!("{other:?}"),
    }
}

/// **An unbound symbol**, which is the error the whole boundary exists to produce.
#[test]
fn calling_an_unbound_symbol_names_the_symbol_and_the_guest_address() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    // Declared, as the loader would declare it while relocating, and never implemented.
    let thunk = builder.declare_function("pthread_rwlock_init").expect("declare");
    let boundary = builder.finish();

    let error = call_with(&guest, &boundary, thunk, &[]).expect_err("nothing implements it");
    match &error {
        AbiError::Unbound { symbol, address } => {
            assert_eq!(symbol, "pthread_rwlock_init");
            assert_eq!(*address, thunk);
        }
        other => panic!("{other:?}"),
    }
    let text = error.to_string();
    assert!(
        text.contains("pthread_rwlock_init") && text.contains(&format!("{thunk:#x}")),
        "the message must identify the symbol precisely: {text}"
    );
    // **And no fabricated return value.** `X0` is whatever the guest left there, which is nothing the
    // guest can mistake for a successful `pthread_rwlock_init`.
    assert_eq!(boundary.crossings().exits, 1, "it went out through the exit path, as it must");
}

/// A data symbol called as if it were a function.
#[test]
fn calling_a_data_symbol_says_so_rather_than_executing_it() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let environ = builder.declare_data("environ", 8, 8).expect("declare");
    let boundary = builder.finish();
    // Something that would be a legal instruction if it were executed, so the test is about the
    // refusal rather than about the bytes happening to be undecodable.
    guest.write_u64(environ, u64::from(ret(30)));

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(environ);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("data is not a function");
    match error {
        AbiError::DataSymbolCalled { symbol, address } => {
            assert_eq!(symbol, "environ");
            assert_eq!(address, environ);
        }
        other => panic!("{other:?}"),
    }
}

/// A thunk reached by a plain `B` — a tail call — with a link register the guest never set.
///
/// The boundary resumes at `X30` because that is what the hardware would do, so a wild `X30` produces
/// a wild resume. The requirement is not that this be prevented: it is that the *guest* takes the
/// fault, typed, with the address named, and that the host survives it.
#[test]
fn a_thunk_tail_called_with_a_wild_link_register_faults_the_guest_and_not_the_host() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    let wild = guest.unmapped;
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.mov(0, (guest.data + 0x100) as u64);
    guest.write_bytes(guest.data + 0x100, b"hi\0");
    asm.mov(30, wild as u64);
    asm.b(thunk);
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let exit = boundary.run(&mut cpu, entry, BUDGET).expect("the host must survive it");
    match exit {
        ExitReason::MemoryFault { address, access: AccessKind::Execute, .. } => {
            assert_eq!(address, wild, "the guest faulted fetching from where it pointed X30");
        }
        other => panic!("{other:?}"),
    }
    // The call itself was serviced — the handler ran and returned a real answer — so the fault is
    // squarely the guest's own choice of return address.
    assert_eq!(cpu.inline_thunk_calls().serviced, 1);
    assert_eq!(cpu.x(x(0)), 2, "and the handler's answer is in X0");
}

// -------------------------------------------------------------------------------- re-entrancy

/// A handler that calls whatever guest function the test pointed it at.
fn call_the_target(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let result = call.call_guest(target(), &[GuestArg::Int(0)], BUDGET)?;
    call.ret(|mut ret| ret.u64(result.x0));
    Ok(())
}

/// **Re-entering the boundary from inside a callback, without end.**
///
/// The guest calls a host function, whose callback calls the same host function, and so on. Every
/// level is a legitimate thing for a `qsort` comparator or a `pthread_once` initialiser to do; the
/// host's stack is the only thing that stops it, and an abort reachable from guest data is Critical
/// and cannot be contained by any caller.
#[test]
fn recursion_through_the_boundary_stops_at_the_depth_limit() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("pthread_once", call_the_target).expect("bind");
    let boundary = builder.finish();

    // The callback calls the very thunk that is calling it.
    let recurse_at = guest.next_entry();
    let mut recurse = Asm::at(recurse_at);
    recurse.push(mov_reg(21, 30));
    recurse.bl(thunk);
    recurse.push(ret(21));
    guest.load(recurse.words());
    set_target(recurse_at);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("it cannot recurse for ever");
    match error {
        AbiError::TooDeep { symbol, depth, limit, address } => {
            assert_eq!(symbol, "pthread_once");
            assert_eq!(address, thunk);
            assert_eq!(limit, omni_android::MAX_GUEST_DEPTH);
            assert_eq!(depth, limit + 1, "it is the level past the limit that is refused");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        boundary.crossings().deepest,
        omni_android::MAX_GUEST_DEPTH,
        "and it got exactly as deep as it is allowed, not one level less"
    );
}

/// A guest callback that faults must be reported as the callback failing, with the exit that happened,
/// rather than as a return value.
#[test]
fn a_guest_callback_that_faults_is_reported_with_the_exit_that_happened() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("pthread_once", call_the_target).expect("bind");
    let boundary = builder.finish();

    // `*(long*)unmapped = 1;`
    let bad_at = guest.next_entry();
    let mut bad = Asm::at(bad_at);
    bad.mov(9, guest.unmapped as u64);
    bad.mov(10, 1);
    bad.push(str_imm(10, 9, 0));
    bad.push(ret(30));
    guest.load(bad.words());
    set_target(bad_at);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("the callback faulted");
    match error {
        AbiError::GuestCallbackStopped { symbol, target: called, exit } => {
            assert_eq!(symbol, "pthread_once");
            assert_eq!(called, bad_at);
            assert!(
                matches!(exit, ExitReason::MemoryFault { address, access: AccessKind::Write, .. }
                    if address == guest.unmapped),
                "{exit:?}"
            );
        }
        other => panic!("{other:?}"),
    }
}

/// A callback cannot be entered on a stack pointer AArch64 forbids.
///
/// Every `SP`-relative access in the callee's own prologue assumes 16-byte alignment, so a misaligned
/// `SP` inherited from a hostile guest would make the *callee* fault at an address nothing here chose.
#[test]
fn a_callback_is_refused_on_a_misaligned_or_unmapped_stack_pointer() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("pthread_once", call_the_target).expect("bind");
    let boundary = builder.finish();

    let nothing_at = guest.next_entry();
    guest.load(&[ret(30)]);
    set_target(nothing_at);

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    // Misaligned by eight, which is the alignment a guest gets wrong by accident.
    let mut cpu = guest.thread(&boundary);
    cpu.set_sp(guest.stack_top - 8);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("SP is not 16-byte aligned");
    match error {
        AbiError::BadCallbackStack { symbol, sp, why, .. } => {
            assert_eq!(symbol, "pthread_once");
            assert_eq!(sp, guest.stack_top - 8);
            assert!(why.contains("16-byte aligned"), "{why}");
        }
        other => panic!("{other:?}"),
    }

    // And a stack pointer that is aligned but has nothing below it: the callee's prologue would push
    // into unmapped memory.
    let mut cpu = guest.thread(&boundary);
    cpu.set_sp(guest.stack_base);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("there is nothing below SP");
    match error {
        AbiError::BadCallbackStack { why, sp, .. } => {
            assert_eq!(sp, guest.stack_base);
            assert!(why.contains("prologue"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// A callback given more arguments than the register banks hold is refused by name rather than pushed
/// onto a guest stack the boundary does not own.
fn nine_arguments(call: &mut ReentrantCall<'_>) -> AbiResult<()> {
    let args: Vec<GuestArg> = (0..9).map(GuestArg::Int).collect();
    call.call_guest(target(), &args, BUDGET)?;
    Ok(())
}

#[test]
fn a_callback_with_more_arguments_than_registers_is_refused_rather_than_written_to_the_stack() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("dl_iterate_phdr", nine_arguments).expect("bind");
    let boundary = builder.finish();

    let nothing_at = guest.next_entry();
    guest.load(&[ret(30)]);
    set_target(nothing_at);

    let error = call_with(&guest, &boundary, thunk, &[]).expect_err("nine will not fit in eight");
    match error {
        AbiError::BadCallbackStack { symbol, why, .. } => {
            assert_eq!(symbol, "dl_iterate_phdr");
            assert!(why.contains("integer"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------------------- unbounded guest loops

/// A guest that loops through an **inline** thunk is stopped by the backend's own budget.
///
/// Which is why the inline path needs no crossing cap: every iteration costs guest instructions, and
/// those are counted.
#[test]
fn a_guest_spinning_through_an_inline_thunk_is_stopped_by_the_step_budget() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    guest.write_bytes(guest.data + 0x100, b"x\0");
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    let loop_at = asm.pc();
    // `X0` is reloaded every iteration, because the handler writes its answer there. A loop that set
    // the pointer once would pass the *length* as the pointer on its second pass, and the test would
    // then be about a bad pointer rather than about the budget — which is what it did at first.
    asm.mov(0, (guest.data + 0x100) as u64);
    asm.bl(thunk);
    asm.b(loop_at);
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let exit = boundary
        .run(&mut cpu, entry, RunLimit::Instructions(10_000))
        .expect("a bounded run must return");
    assert!(matches!(exit, ExitReason::StepLimitReached { .. }), "{exit:?}");
    assert!(cpu.inline_thunk_calls().serviced > 100, "it really did loop through the boundary");
}

/// A guest that loops through the **exit** path defeats the backend's budget — every crossing returns
/// to Rust — so the boundary's own cap is what stops it, and it must say so rather than looking like a
/// watchdog firing.
#[test]
fn a_guest_spinning_through_the_exit_path_is_stopped_by_the_boundarys_own_cap() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    // Unbound, so every crossing is an exit — and the *first* one already fails, which is the point:
    // a guest cannot spin through an unbound symbol either, because the error is immediate.
    let unbound = builder.declare_function("never_implemented").expect("declare");
    let boundary = builder.finish();

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    let loop_at = asm.pc();
    asm.bl(unbound);
    asm.b(loop_at);
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("the first one fails");
    assert!(matches!(error, AbiError::Unbound { .. }), "{error:?}");

    // The halt handle is the other containment, and the one a watchdog uses: it does not need the
    // guest to be making progress. A halt requested before the run is sticky, so the boundary must
    // honour it before letting the guest go at all.
    let mut cpu = guest.thread(&boundary);
    cpu.halt_handle().request();
    let exit = boundary
        .run(&mut cpu, entry, RunLimit::Unlimited)
        .expect("a halt is not an error");
    assert!(matches!(exit, ExitReason::Halted { .. }), "{exit:?}");
    assert_eq!(boundary.crossings().exits, 1, "the second run never crossed at all");
}

/// **A handler that fails runs exactly once.**
///
/// An inline handler with no return channel records its error and defers, and the exit path reports the
/// recorded error. It *could* instead let the exit path re-run the handler over the CPU's register file
/// and take the error from there — the values are the same — and the mutation harness showed that
/// removing the recorded-error check leaves every other test passing for exactly that reason. It is not
/// equivalent: a handler that had already written half its output before failing would write it twice.
/// So the property is that the handler is entered once, and this is what says so.
static ATTEMPTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn always_fails(call: &mut ImportCall<'_, '_>) -> AbiResult<()> {
    ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Fails on the argument, which is the ordinary way a handler fails.
    let pointer = call.args().next_pointer()?;
    call.mem().cstr(pointer, call.blame(0))?;
    Ok(())
}

#[test]
fn a_handler_that_fails_is_entered_once_and_not_re_run_on_the_exit_path() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", always_fails).expect("bind");
    let boundary = builder.finish();

    ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 0);
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, BUDGET).expect_err("it always fails");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(
        ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the handler must be entered once: a handler that had written half its output before failing          would otherwise write it twice"
    );
    // And the inline path is what entered it — once, and once deferred — so this is not passing because
    // the fast path was skipped and the exit path did the work.
    assert_eq!(cpu.inline_thunk_calls().serviced, 1);
    assert_eq!(cpu.inline_thunk_calls().deferred, 1);
    // `exits` stays at zero on purpose: the recorded error is read *before* the slot is serviced, so a
    // deferred failure never reaches `service_exit` at all. That is the property the mutation row
    // removes.
    assert_eq!(boundary.crossings().exits, 0);
}

/// **A handler that fails on the budget's last instruction reports the FAILURE, not a step limit.**
///
/// The late-budget fix moved the charge below the `match` so a terminal exit landing on the budget's
/// last instruction is reported as what it is. But it left the budget arm ABOVE the pending-error
/// check, which introduced a new misclassification of the same class it had just fixed.
///
/// The interleaving: an inline handler fails, records its typed error and defers; on that same
/// crossing the counted budget is exactly exhausted. The budget arm returned
/// `Ok(ExitReason::StepLimitReached)`, and the `AbiError` was left in the thread-local for the next
/// `Boundary::run` to drop with `let _ = take_pending()`.
///
/// Two things go wrong at once. The caller is told "budget expired, resumable" when an imported call
/// actually failed; and a caller that acts on that and resumes enters the handler a SECOND time —
/// exactly the once-only property `a_handler_that_fails_is_entered_once_and_not_re_run_on_the_exit_path`
/// and row `boundary-A6` exist to protect.
///
/// Self-calibrating for the same reason the exact-budget test is: how many instructions a program
/// charges is the backend's business.
#[test]
fn a_handler_that_fails_on_the_budgets_last_instruction_reports_the_failure_not_a_step_limit() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", always_fails).expect("bind");
    let boundary = builder.finish();

    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, 0); // a null pointer: the handler fails on its argument
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    // How many instructions does the backend charge to reach the crossing?
    ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, RunLimit::Unlimited).expect_err("it always fails");
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    let charged = cpu.last_run_instructions();
    assert!(charged > 0, "the backend must charge something for a program that ran");

    // The same program with EXACTLY that allowance, so the budget runs out on the very crossing the
    // handler failed on.
    ATTEMPTS.store(0, std::sync::atomic::Ordering::Relaxed);
    let mut cpu = guest.thread(&boundary);
    let error = boundary.run(&mut cpu, entry, RunLimit::Instructions(charged)).expect_err(
        "a handler that has already run and failed is not unserviced and resumable: reporting a step          limit here both drops the typed error and invites the caller to resume into a second entry",
    );
    assert!(matches!(error, AbiError::BadPointer { .. }), "{error:?}");
    assert_eq!(
        ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "and it was still entered exactly once",
    );
}

/// **The exit-path crossing cap, reached.**
///
/// `MAX_EXIT_CROSSINGS` is 2^32, which no test can reach, so the cap is lowered for this one. A limit
/// no test reaches is a limit nobody knows works — Global Constraint 13's difference between code that
/// runs and a bug that is detected.
#[test]
fn a_guest_that_keeps_crossing_the_exit_path_hits_the_cap_with_a_typed_error() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("pthread_once", call_the_target).expect("bind");
    builder.with_exit_crossing_limit(4);
    let boundary = builder.finish();

    let nothing_at = guest.next_entry();
    guest.load(&[ret(30)]);
    set_target(nothing_at);

    // A guest loop of two instructions through an exit thunk: the backend's own budget never expires,
    // because every crossing returns to Rust before a block finishes.
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    let loop_at = asm.pc();
    asm.bl(thunk);
    asm.b(loop_at);
    guest.load(asm.words());

    // **A counted budget as well as the cap**, so that a build in which the cap does not work *fails*
    // rather than hanging. That is not hypothetical: two mutation rows remove or defeat the cap, and
    // with `RunLimit::Unlimited` they made the harness hang instead of reporting a catch — which is
    // how the budget-per-crossing defect below was found in the first place.
    let mut cpu = guest.thread(&boundary);
    let error = boundary
        .run(&mut cpu, entry, RunLimit::Instructions(1_000))
        .expect_err("the cap must stop it, and must say so");
    let text = format!("{error:?}");
    match error {
        AbiError::CrossingLimit { crossings, limit, .. } => {
            assert_eq!(limit, 4);
            assert_eq!(crossings, 4, "it stopped at the cap, not before it and not after");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(boundary.crossings().exits, 4);
    // And it is **not** an `ExitReason::Halted`, which is what a watchdog firing looks like. A caller
    // has to be able to tell "my halt fired" from "the guest is spinning through the boundary".
    assert!(!text.contains("Halted"), "{text}");
}

/// **A counted budget must bound the whole run, not each crossing of it.**
///
/// The exit path returns to Rust before any block finishes, so each `cpu.run` starts a new counted run.
/// A driver that handed the caller's `limit` to every segment would give a guest that crosses N times N
/// times the allowance it asked for — and Global Constraint 11's point is that a bound is only as
/// trustworthy as its least-validated input. Found by the mutation harness hanging rather than failing.
#[test]
fn a_counted_budget_is_spent_down_across_crossings_and_not_handed_out_afresh() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_reentrant("pthread_once", call_the_target).expect("bind");
    // High enough that the cap is not what stops this: the budget has to be.
    builder.with_exit_crossing_limit(1_000_000);
    let boundary = builder.finish();

    let nothing_at = guest.next_entry();
    guest.load(&[ret(30)]);
    set_target(nothing_at);

    // Two guest instructions per crossing, so a budget of `n` permits about `n / 2` crossings in total
    // — and *many* times that if the budget is re-handed to each one.
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    let loop_at = asm.pc();
    asm.bl(thunk);
    asm.b(loop_at);
    guest.load(asm.words());

    let budget = 2_000u64;
    let mut cpu = guest.thread(&boundary);
    let exit = boundary
        .run(&mut cpu, entry, RunLimit::Instructions(budget))
        .expect("a budget expiring is not an error");
    assert!(matches!(exit, ExitReason::StepLimitReached { .. }), "{exit:?}");
    let crossings = boundary.crossings().exits;
    assert!(crossings > 1, "it must really have crossed more than once; it crossed {crossings}");
    // The arithmetic that matters. Each crossing costs at least the `BL` and the `B`, so the whole run
    // cannot have crossed more than `budget` times however the segments were sliced. A driver that
    // re-handed the budget would run until the crossing cap, which is 500x higher.
    assert!(
        crossings <= budget,
        "{crossings} crossings under a budget of {budget} guest instructions: the budget is being          handed out per crossing rather than spent down"
    );
}

/// **A budget that is exactly enough must not turn a return into a step limit.**
///
/// The first version of the budget accounting charged the allowance immediately after every `cpu.run`
/// and pre-empted whenever it had run out — so *any* terminal exit landing on the budget's last
/// instruction was reported as `StepLimitReached`. For `Returned` that loses the return. For
/// `MemoryFault` it is worse: a fault is **not** resumable and a step limit is, so a caller acting on
/// the exit would have resumed a faulting guest.
///
/// The budget is self-calibrating rather than guessed, because how many instructions a program charges
/// is the backend's business: the same program is run once unbounded to find out, and then again with
/// exactly that many.
#[test]
fn a_budget_that_is_exactly_spent_still_reports_the_exit_that_happened() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    guest.write_bytes(guest.data + 0x100, b"hi ");
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, (guest.data + 0x100) as u64);
    asm.bl(thunk);
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let exit = boundary.run(&mut cpu, entry, RunLimit::Unlimited).expect("an unbounded run");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    let charged = cpu.last_run_instructions();
    assert!(charged > 0, "the backend must charge something for a program that ran");

    // Exactly the allowance the program needs for its final segment. `saturating_sub` then leaves
    // nothing, which is the condition the old accounting pre-empted on.
    let mut cpu = guest.thread(&boundary);
    let exit = boundary
        .run(&mut cpu, entry, RunLimit::Instructions(charged))
        .expect("a budget that is exactly enough");
    assert!(
        matches!(exit, ExitReason::Returned { .. }),
        "a return that lands on the budget's last instruction is a return, not a step limit: {exit:?}"
    );
    assert_eq!(guest.read_u64(guest.data + 0x100) & 0xFF, u64::from(b'h'), "and it really ran");
}

/// Nothing above may have left the host in a state where the next guest can misbehave differently.
///
/// The cheapest possible whole-suite invariant, and the one that would catch a handler that recorded a
/// pending error and never had it consumed: a fresh boundary on a fresh guest, after all of the above,
/// still completes an ordinary call.
#[test]
fn zz_an_ordinary_call_still_works_after_every_hostile_shape() {
    let _guard = serialized();
    let guest = Guest::new();
    let builder = guest.boundary(8);
    let thunk = builder.bind_inline("strlen", len).expect("bind");
    let boundary = builder.finish();

    guest.write_bytes(guest.data + 0x100, b"omnidroid\0");
    let entry = guest.next_entry();
    let mut asm = Asm::at(entry);
    asm.push(mov_reg(21, 30));
    asm.mov(0, (guest.data + 0x100) as u64);
    asm.bl(thunk);
    asm.mov(22, guest.data as u64);
    asm.push(str_imm(0, 22, 0));
    asm.push(ret(21));
    guest.load(asm.words());

    let mut cpu = guest.thread(&boundary);
    let exit = boundary.run(&mut cpu, entry, BUDGET).expect("an ordinary call");
    assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
    assert_eq!(guest.read_u64(guest.data), 9);
}
