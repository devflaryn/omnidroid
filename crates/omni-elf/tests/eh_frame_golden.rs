//! The function map and the leaf scan, against the real `libroblox.so`.
//!
//! These are the numbers M2's choice of function rests on, so they are pinned exactly rather than
//! bounded. A scan that found *a* leaf would be worth nothing: what makes the M2 gate meaningful is
//! that the population is known, the grading is known, and the function that was run is one this
//! scan names.
//!
//! Skips (loudly) when the APK is not present; see `tests/common/mod.rs`.

mod common;

use omni_elf::leaf::{self, LeafKind, TextRelocations};
use omni_elf::{EhFrameHdr, ElfImage};

/// From the file: `.eh_frame_hdr` at `p_vaddr` 0x10b2f98, 0x1debfc bytes.
const EH_FRAME_HDR_VADDR: u64 = 0x10b_2f98;
const EH_FRAME_VADDR: u64 = 0x129_1b98;
/// The figure the JNI analysis reported and the brief repeats.
const FUNCTION_COUNT: usize = 245_117;

/// Leaf counts by grade. Exact, because the whole value of the scan is that it is deterministic:
/// a change here means either the binary changed or the classifier did, and both are things a
/// reviewer must be told about rather than left to notice.
const PURE_REGISTER_LEAVES: usize = 819;
const STACK_ONLY_LEAVES: usize = 6;
const STACK_AND_THREAD_POINTER_LEAVES: usize = 0;
const STACK_GUARD_PROTECTED_LEAVES: usize = 45;

/// The PLT stub every stack-guard-protected leaf calls on its failure path.
const STACK_CHK_FAIL_STUB: u64 = 0x62d_67d0;

/// The two functions the M2 gate in `omni-cpu` executes, by `p_vaddr`.
const BASE64_SEXTET: u64 = 0x2c1_1e34;
const TIMEVAL_TO_MILLIS: u64 = 0x222_7844;
const STACK_GUARD_LEAF: u64 = 0x287_2aac;

fn image(data: &[u8]) -> ElfImage<'_> {
    ElfImage::parse(data).expect("libroblox.so must parse")
}

#[test]
fn the_eh_frame_header_is_where_the_program_headers_say() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let hdr = EhFrameHdr::parse(&elf).expect("parse .eh_frame_hdr").expect("it has one");
    assert_eq!(hdr.hdr_vaddr, EH_FRAME_HDR_VADDR);
    assert_eq!(hdr.eh_frame_vaddr, EH_FRAME_VADDR);
    assert_eq!(hdr.fde_count as usize, FUNCTION_COUNT);
    // `DW_EH_PE_datarel | DW_EH_PE_sdata4`. Named because reading it as `udata4` would still
    // produce addresses, just wrong ones for every function below the header.
    assert_eq!(hdr.table_encoding, 0x3b);
}

#[test]
fn the_function_map_is_exact_sorted_and_complete() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let funcs = elf.eh_frame_functions().expect("read the map").expect("it has a map");
    assert_eq!(funcs.len(), FUNCTION_COUNT);

    // The table is a binary-search table, so it must be sorted; an unsorted one would mean the
    // unwinder in the guest cannot find its own frames, and that our reading of it is wrong.
    assert!(
        funcs.windows(2).all(|w| w[0].start <= w[1].start),
        ".eh_frame_hdr's table must be sorted by address"
    );

    // First and last, exactly. These come from the file and pin the pc-relative arithmetic: an
    // off-by-one in the `sdata4` base shifts every entry by four bytes and would still look sorted.
    assert_eq!((funcs[0].start, funcs[0].len), (0x1d9_5980, 0xb4));
    let last = funcs[funcs.len() - 1];
    assert_eq!((last.start, last.len), (0x62d_5b5c, 0x4a8));

    // An additive checksum over every start and length. A decoder that got the count right and the
    // values wrong passes every assertion above and fails this one.
    let start_sum: u64 = funcs.iter().fold(0, |a, f| a.wrapping_add(f.start));
    let len_sum: u64 = funcs.iter().map(|f| f.len).sum();
    assert_eq!(start_sum, 0xEF5_1653_82CC, "sum of every function start");
    assert_eq!(len_sum, 69_943_828, "sum of every function length");

    // Exactly one FDE describes an empty range. It is kept rather than refused (see
    // `eh_frame.rs`), and it is named here so that "one" cannot quietly become "several".
    let empty: Vec<_> = funcs.iter().filter(|f| f.len == 0).map(|f| f.start).collect();
    assert_eq!(empty, vec![0x364_f404]);

    // Every non-empty function is a whole number of instructions and lies inside the file image.
    assert!(funcs.iter().all(|f| f.len % 4 == 0));
    assert!(funcs
        .iter()
        .all(|f| f.len == 0 || elf.slice_at_vaddr("body", f.start, f.len).is_ok()));
}

/// The premise the whole "no relocation-bearing loads" argument rests on, measured rather than
/// assumed: **not one** of `libroblox.so`'s 568,806 relocations writes into an executable segment.
#[test]
fn nothing_relocates_into_the_text() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let relocations = TextRelocations::collect(&elf).expect("collect the relocation targets");
    assert_eq!(relocations.examined, 568_806, "every relocation must have been examined");
    assert_eq!(
        relocations.total(),
        0,
        "a relocation landing in the text would mean the bytes in the file are not the bytes that \
         execute, and every function the leaf scan accepted would be a guess"
    );
}

#[test]
fn the_leaf_population_is_what_the_m2_choice_was_made_from() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let leaves = leaf::find_leaves(&elf).expect("scan for leaves");

    let count = |k: LeafKind| leaves.iter().filter(|l| l.kind == k).count();
    assert_eq!(count(LeafKind::PureRegister), PURE_REGISTER_LEAVES);
    assert_eq!(count(LeafKind::StackOnly), STACK_ONLY_LEAVES);
    assert_eq!(count(LeafKind::StackAndThreadPointer), STACK_AND_THREAD_POINTER_LEAVES);
    assert_eq!(count(LeafKind::StackGuardProtected), STACK_GUARD_PROTECTED_LEAVES);
    assert_eq!(count(LeafKind::NotALeaf), 0, "`find_leaves` returns only leaves");

    // Every stack-guard-protected leaf calls exactly one address, and it is the same one for all
    // 45 of them: the PLT stub for `__stack_chk_fail`. If that were not true, "the call is only
    // taken when the guard fails" would be a guess about several different callees.
    for l in leaves.iter().filter(|l| l.kind == LeafKind::StackGuardProtected) {
        assert_eq!(
            l.facts.direct_calls.iter().copied().collect::<Vec<_>>(),
            vec![STACK_CHK_FAIL_STUB],
            "{:#x} calls something other than the stack-protector stub",
            l.bounds.start
        );
        assert_eq!(l.facts.calls_before_last_return, 0, "{:#x}", l.bounds.start);
        assert!(l.facts.stack_guard_loads >= 1, "{:#x}", l.bounds.start);
    }

    // And no leaf of any grade reaches memory through a register a caller would have to fill.
    assert!(leaves.iter().all(|l| l.facts.foreign_memory_bases.is_empty()));
    assert!(leaves.iter().all(|l| l.facts.relocated_words == 0));
}

/// The three functions the M2 gate runs, each confirmed to be in the scan's output with the grade
/// the gate assumes. The gate asserts the instruction words separately; this asserts that the
/// *selection* is reproducible from the binary rather than a magic address somebody wrote down.
#[test]
fn the_functions_the_m2_gate_runs_are_ones_this_scan_names() {
    let Some(bytes) = common::main_lib() else { return };
    let elf = image(bytes);
    let leaves = leaf::find_leaves(&elf).expect("scan for leaves");

    for (vaddr, len, kind) in [
        (BASE64_SEXTET, 104u64, LeafKind::PureRegister),
        (TIMEVAL_TO_MILLIS, 116, LeafKind::PureRegister),
        (STACK_GUARD_LEAF, 60, LeafKind::StackGuardProtected),
    ] {
        let found = leaves
            .iter()
            .find(|l| l.bounds.start == vaddr)
            .unwrap_or_else(|| panic!("{vaddr:#x} is not in the leaf scan's output"));
        assert_eq!(found.bounds.len, len, "{vaddr:#x} length");
        assert_eq!(found.kind, kind, "{vaddr:#x} grade");
    }
}
