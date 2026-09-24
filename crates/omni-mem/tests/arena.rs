//! The JIT code arena: two views of the same pages, one writable and one executable.
//!
//! The tests that matter here are the ones that *execute* what they wrote. A test that only checks
//! that two pointers differ, or that a write through one view is readable through the other, would
//! pass just as happily if the executable view were not executable or if the pages were silently
//! privatised — and privatisation is exactly the failure mode D12 and Task 1 both warn about, since
//! it produces no error anywhere.
#![cfg(any(target_os = "windows", target_os = "macos"))]

mod common;

use std::sync::Mutex;

use common::{KIB, MIB};
use omni_mem::{ArenaConfig, CodeArena, MemError};
use omni_platform::vm;

/// Serialises the one test that measures commit charge, which is a per-process quantity.
static SERIAL: Mutex<()> = Mutex::new(());

/// x86-64: `mov eax, imm32; ret`. Returns a constant, in six bytes, with no prologue and no stack
/// use, so it is safe to call as a bare `extern "C" fn() -> u32` under either x64 ABI.
#[cfg(target_arch = "x86_64")]
fn returns_constant(value: u32) -> [u8; 6] {
    let mut code = [0xB8, 0, 0, 0, 0, 0xC3];
    code[1..5].copy_from_slice(&value.to_le_bytes());
    code
}

#[test]
fn a_block_is_writable_at_one_address_and_executable_at_another() {
    let arena = CodeArena::new().expect("create the arena");
    let block = arena.alloc(64).expect("allocate a block");

    assert_eq!(block.len(), 64);
    assert_ne!(
        block.write_ptr() as usize,
        block.exec_ptr() as usize,
        "the writable and executable addresses must differ"
    );
    assert_ne!(block.view_distance(), 0);
    let write = block.write_ptr() as usize;
    let exec = block.exec_ptr() as usize;
    assert!(
        write + block.len() <= exec || exec + block.len() <= write,
        "the two views of a block must not overlap: {write:#x} and {exec:#x}"
    );

    // A write through the writable view is visible through the executable view immediately: they are
    // two sets of page-table entries over the same physical pages, not two copies.
    arena.write(&block, 0, &[0x11, 0x22, 0x33, 0x44]).expect("write");
    // SAFETY: the block is 64 bytes of live mapping and this reads the first four of them.
    unsafe {
        assert_eq!(std::slice::from_raw_parts(block.exec_ptr(), 4), &[0x11, 0x22, 0x33, 0x44]);
    }

    let stats = arena.stats();
    assert_eq!(stats.chunks, 1);
    assert_eq!(stats.used, 64);
    assert_eq!(stats.mapped, arena.config().chunk_size);
}

/// Write a real function through the writable view and call it through the executable view.
///
/// This is the test the brief asks for, and the only one that proves the arrangement end to end: if
/// the executable view were a private copy of the pages, the bytes would not be there and this would
/// either fault or execute whatever was in the section before.
#[cfg(target_arch = "x86_64")]
#[test]
fn code_written_through_the_writable_view_executes_through_the_executable_view() {
    let arena = CodeArena::new().expect("create the arena");
    let block = arena.alloc(16).expect("allocate");
    arena.write(&block, 0, &returns_constant(42)).expect("emit");

    // SAFETY: the block holds six bytes of valid x86-64 that load EAX with a constant and return.
    // It takes no arguments, touches no stack beyond the return address, and clobbers only EAX, so
    // calling it as `extern "C" fn() -> u32` is correct under the Microsoft x64 ABI.
    let result = unsafe {
        let function: extern "C" fn() -> u32 = std::mem::transmute(block.exec_ptr());
        function()
    };
    assert_eq!(result, 42, "the code written through the writable view did not execute");
}

/// Sealing and unsealing a block, then rewriting and re-executing it.
///
/// This is the regression test for the trap Task 1 defused: a dual-mapped arena's writable view is
/// `MEM_MAPPED`, so resolving [`Protection::ReadWrite`] to `PAGE_WRITECOPY` — which is the *correct*
/// answer for a private file view — would privatise the pages on the way back from sealed. Every
/// later write would land on a copy, the executable view would keep running the old code, and
/// nothing anywhere would report an error.
///
/// So this asserts on the value the *second* emission returns. If the shared case were ever collapsed
/// into the copy-on-write case, this test would see 42 where it demands 99.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_resealed_block_is_still_shared_with_its_executable_view() {
    let arena = CodeArena::new().expect("create the arena");
    // A page-sized block, so that sealing it does not disturb a neighbour.
    let block = arena.alloc(vm::page_size()).expect("allocate");
    arena.write(&block, 0, &returns_constant(42)).expect("emit");

    // SAFETY: as in the test above — six bytes of valid, argument-free x86-64.
    let function: extern "C" fn() -> u32 =
        unsafe { std::mem::transmute(block.exec_ptr()) };
    assert_eq!(function(), 42);

    arena.seal(&block).expect("seal");
    // Sealed code still runs; it just cannot be modified through the writable view.
    assert_eq!(function(), 42, "sealing must not affect the executable view");

    arena.unseal(&block).expect("unseal");
    arena.write(&block, 0, &returns_constant(99)).expect("re-emit");
    assert_eq!(
        function(),
        99,
        "the rewritten code did not reach the executable view: the writable view was privatised, \
         which is the silent failure D12 and omni-platform's RegionFlavour classification exist to \
         prevent"
    );

    // And once more, to show the block can be patched repeatedly.
    arena.seal(&block).expect("seal again");
    arena.unseal(&block).expect("unseal again");
    arena.write(&block, 0, &returns_constant(7)).expect("patch");
    assert_eq!(function(), 7);
}

/// Writing through the executable pointer must fault.
///
/// The W^X guarantee is only real if the hardware enforces it, so this runs the write in a child
/// process and asserts the child died of an access violation. There is no way to assert this in
/// process: the whole point is that the store is not survivable.
#[test]
fn the_executable_view_is_not_writable() {
    const CHILD: &str = "OMNI_MEM_ARENA_EXEC_WRITE_CHILD";
    const NO_FAULT: i32 = 7;
    /// `STATUS_ACCESS_VIOLATION`, which is what a Windows process exits with when it stores to a
    /// page it has no write access to.
    const ACCESS_VIOLATION: i32 = 0xC000_0005u32 as i32;

    if std::env::var_os(CHILD).is_some() {
        let arena = CodeArena::new().expect("create the arena");
        let block = arena.alloc(64).expect("allocate");
        // SAFETY: none. This is the point of the test: the store is expected to raise an access
        // violation and kill this process, because the executable view is `PAGE_EXECUTE_READ`.
        // `write_volatile` so that nothing can optimise the store away.
        unsafe {
            std::ptr::write_volatile(block.exec_ptr().cast_mut(), 0x90);
        }
        // Reached only if the executable view was writable, which would be a defect.
        std::process::exit(NO_FAULT);
    }

    let status = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["the_executable_view_is_not_writable", "--exact", "--nocapture"])
        .env(CHILD, "1")
        .status()
        .expect("run the child");
    let code = status.code();
    assert_ne!(
        code,
        Some(NO_FAULT),
        "the child wrote through the executable view without faulting: the arena's executable view \
         is writable, which breaks the W^X guarantee"
    );
    #[cfg(target_os = "windows")]
    assert_eq!(
        code,
        Some(ACCESS_VIOLATION),
        "expected the child to die of STATUS_ACCESS_VIOLATION ({ACCESS_VIOLATION:#x}), got {code:?}"
    );
    // On macOS a store to a page with no write permission is a signal, not an exit code: SIGBUS
    // (10) for a present page whose protection forbids it, SIGSEGV (11) for one that is absent.
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::process::ExitStatusExt;
        let _ = ACCESS_VIOLATION;
        assert!(
            matches!(status.signal(), Some(10 | 11)),
            "expected the child to die of SIGBUS or SIGSEGV, got {status:?}"
        );
    }
}

#[test]
fn blocks_are_aligned_do_not_overlap_and_the_arena_grows_by_chunks() {
    let chunk = 64 * KIB;
    let arena = CodeArena::with_config(ArenaConfig {
        chunk_size: chunk,
        max_total: 4 * chunk,
        block_alignment: 16,
    })
    .expect("create the arena");

    let mut blocks = Vec::new();
    for index in 0..12 {
        let block = arena.alloc(7 * KIB + index).expect("allocate");
        assert_eq!(block.write_ptr() as usize % 16, 0, "block {index} is misaligned");
        assert_eq!(block.exec_ptr() as usize % 16, 0, "block {index} is misaligned");
        blocks.push(block);
    }

    // No two blocks overlap, in either view.
    for (index, block) in blocks.iter().enumerate() {
        for other in &blocks[index + 1..] {
            let (a, b) = (block.write_ptr() as usize, other.write_ptr() as usize);
            assert!(
                a + block.len() <= b || b + other.len() <= a,
                "two blocks share writable memory: {a:#x} and {b:#x}"
            );
            let (a, b) = (block.exec_ptr() as usize, other.exec_ptr() as usize);
            assert!(
                a + block.len() <= b || b + other.len() <= a,
                "two blocks share executable memory: {a:#x} and {b:#x}"
            );
        }
    }

    // Each block is distinguishable through its own pointers, which is what proves they are really
    // separate ranges of one shared section and not aliases.
    for (index, block) in blocks.iter().enumerate() {
        arena.write(block, 0, &[index as u8]).expect("write");
    }
    for (index, block) in blocks.iter().enumerate() {
        // SAFETY: each block is at least one byte of live mapping.
        unsafe {
            assert_eq!(*block.exec_ptr(), index as u8, "block {index} was overwritten");
        }
    }

    let stats = arena.stats();
    assert!(stats.chunks > 1, "12 blocks of 7 KiB should not fit in one 64 KiB chunk");
    assert_eq!(stats.mapped, stats.chunks * chunk);
    assert!(stats.wasted < stats.mapped);
    eprintln!("{stats:?}");
}

#[test]
fn the_arena_refuses_to_grow_past_its_limit_and_says_what_it_is() {
    let arena = CodeArena::with_config(ArenaConfig {
        chunk_size: 64 * KIB,
        max_total: 128 * KIB,
        block_alignment: 16,
    })
    .expect("create the arena");
    arena.alloc(64 * KIB).expect("the first chunk");
    arena.alloc(64 * KIB).expect("the second chunk");
    let error = arena.alloc(64 * KIB).expect_err("the third must be refused");
    match error {
        MemError::ArenaFull { requested, in_use, limit } => {
            assert_eq!(requested, 64 * KIB);
            assert_eq!(in_use, 128 * KIB);
            assert_eq!(limit, 128 * KIB);
        }
        other => panic!("expected ArenaFull, got {other}"),
    }

    // A block larger than a chunk gets a chunk of its own rather than being refused, as long as the
    // limit allows it.
    let arena = CodeArena::with_config(ArenaConfig {
        chunk_size: 64 * KIB,
        max_total: MIB,
        block_alignment: 16,
    })
    .expect("create the arena");
    let block = arena.alloc(300 * KIB).expect("an oversized block");
    assert_eq!(block.len(), 300 * KIB);
    arena.write(&block, 300 * KIB - 4, &[1, 2, 3, 4]).expect("write at the very end");
    assert_eq!(arena.stats().chunks, 1);
}

#[test]
fn a_write_past_the_end_of_a_block_is_refused() {
    let arena = CodeArena::new().expect("create the arena");
    let block = arena.alloc(32).expect("allocate");
    let error = arena.write(&block, 30, &[0; 4]).expect_err("a write past the end must be refused");
    match error {
        MemError::BlockOverflow { offset, len, block_len } => {
            assert_eq!((offset, len, block_len), (30, 4, 32));
        }
        other => panic!("expected BlockOverflow, got {other}"),
    }
    // A zero-length allocation is meaningless rather than silently rounded up.
    assert!(matches!(arena.alloc(0), Err(MemError::ZeroSize { .. })));
}

/// What the arena costs, measured.
///
/// Reported as a discovery rather than asserted as a budget: a pagefile-backed section is *shared*
/// memory, so it does not appear in `PrivateUsage`, which is what
/// [`vm::process_commit_charge`] returns. It still counts against the **system** commit limit, and
/// this API cannot see that — which is precisely why the arena grows a chunk at a time instead of
/// creating one large section up front.
#[test]
fn the_arena_reports_what_it_mapped_even_though_commit_charge_cannot_see_it() {
    let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
    let chunk = 4 * MIB;
    let before = vm::process_commit_charge().expect("commit charge") as i64;

    let arena = CodeArena::with_config(ArenaConfig {
        chunk_size: chunk,
        max_total: 16 * MIB,
        block_alignment: 16,
    })
    .expect("create the arena");
    assert_eq!(arena.stats().mapped, 0, "an arena maps nothing until it is asked for a block");
    // Tolerant rather than exact: the other tests in this binary run in parallel and allocate a
    // little of their own, and this is the only one that reads a per-process counter. The effects
    // being measured are megabytes.
    let idle = vm::process_commit_charge().expect("commit charge") as i64 - before;
    assert!(idle.abs() < chunk as i64 / 4, "creating an arena cost {idle} bytes");

    let block = arena.alloc(chunk).expect("allocate a whole chunk");
    let mapped = vm::process_commit_charge().expect("commit charge") as i64 - before;
    assert_eq!(arena.stats().mapped, chunk);

    // Touch every page through the writable view, then read it back through the executable one.
    for offset in (0..chunk).step_by(vm::page_size()) {
        arena.write(&block, offset, &[(offset / vm::page_size()) as u8]).expect("write");
    }
    let touched = vm::process_commit_charge().expect("commit charge") as i64 - before;
    for offset in (0..chunk).step_by(vm::page_size()) {
        // SAFETY: the whole chunk is a live executable view of the same pages just written.
        unsafe {
            assert_eq!(
                *block.exec_ptr().add(offset),
                (offset / vm::page_size()) as u8,
                "the executable view disagrees with the writable view at {offset:#x}"
            );
        }
    }

    drop(arena);
    let after = vm::process_commit_charge().expect("commit charge") as i64 - before;
    eprintln!(
        "a {} MiB arena chunk: PrivateUsage after mapping {} bytes, after touching every page {} \
         bytes, after dropping the arena {} bytes",
        chunk / MIB,
        mapped,
        touched,
        after
    );
    // The two views cost page tables and nothing else as far as this process's private commit is
    // concerned, and dropping the arena gives even that back.
    assert!(
        mapped < chunk as i64 / 4,
        "the arena's views should not appear in PrivateUsage as private commit; saw {mapped} bytes"
    );
    assert!(after.abs() < chunk as i64 / 4, "dropping the arena left {after} bytes behind");
}


/// **I2 regression.** An absurd allocation size must be refused, not wrap past the limit check.
///
/// `mapped + chunk_len` overflowed: in a debug build that panicked, and in a release build it wrapped
/// to a small number, passed the ceiling check, and went on to try to map something — which is the
/// one outcome worse than refusing.
#[test]
fn an_absurd_allocation_size_is_refused_rather_than_overflowing_the_limit_check() {
    let arena = CodeArena::new().expect("create the arena");
    let first = arena.alloc(64).expect("one real block, so a chunk exists to add to");

    for size in [usize::MAX, usize::MAX - 65535, usize::MAX / 2, isize::MAX as usize] {
        match arena.alloc(size) {
            Err(MemError::ArenaFull { requested, limit, .. }) => {
                assert_eq!(requested, size);
                assert_eq!(limit, arena.config().max_total);
            }
            Err(other) => panic!("expected ArenaFull for {size:#x}, got {other}"),
            Ok(_) => panic!("a block of {size:#x} bytes cannot possibly have been allocated"),
        }
    }

    // The arena is still usable afterwards, and the block from before is untouched.
    arena.write(&first, 0, &[0x42]).expect("write");
    // SAFETY: the block is at least one byte of live mapping.
    unsafe { assert_eq!(*first.exec_ptr(), 0x42) };
    arena.alloc(64).expect("the arena still works");
    assert_eq!(arena.stats().chunks, 1, "no chunk should have been created for a refused request");
}

// -------------------------------------------------------------------------------------------
// Arena identity. A `CodeBlock` is a plain `Copy` value holding addresses rather than a borrow, so
// nothing in the type system tied one to the arena that made it — and per-thread code caches (D5:
// 20-35 MiB each, not shared between threads) mean several live arenas is M2's expected shape.
// -------------------------------------------------------------------------------------------

/// A block minted by one arena is refused by another, through every operation that dereferences it.
///
/// Without this check `let b = a.alloc(16)?; drop(a); other.write(&b, 0, &bytes)` is expressible in
/// **safe** Rust and writes through an unmapped address: `write` bounds-checks only against
/// `block.len` and then stores at `block.write + offset`, which it never verifies belongs to this
/// arena. `reprotect` was worse — it indexed `chunks[block.chunk]`, which panics for an out-of-range
/// index, and then computed `end - start` behind a `debug_assert!`, so in a release build a foreign
/// address below `chunk.write` underflowed into a huge page-aligned length handed to `vm::protect`.
#[test]
fn a_block_from_another_arena_is_refused_by_every_operation() {
    let one = CodeArena::new().expect("arena one");
    let two = CodeArena::with_config(ArenaConfig {
        // A different chunk size, so the two arenas cannot accidentally agree on anything.
        chunk_size: 2 * MIB,
        ..ArenaConfig::default()
    })
    .expect("arena two");
    assert_ne!(one.id(), two.id(), "each arena has its own identity");

    let block = one.alloc(64).expect("allocate from arena one");
    assert_eq!(block.arena(), one.id(), "a block carries the identity of its arena");
    assert_ne!(block.arena(), two.id());

    // Its own arena accepts it.
    one.write(&block, 0, &[0xCC; 64]).expect("its own arena writes it");
    one.seal(&block).expect("its own arena seals it");
    one.unseal(&block).expect("and unseals it");

    // The other refuses it, and says whose block it is.
    for (label, result) in [
        ("write", two.write(&block, 0, &[0xCC; 64])),
        ("seal", two.seal(&block)),
        ("unseal", two.unseal(&block)),
    ] {
        match result {
            Err(MemError::ForeignBlock { arena, block_arena }) => {
                assert_eq!(arena, two.id().0, "{label}: the arena that refused");
                assert_eq!(block_arena, one.id().0, "{label}: the arena that minted the block");
            }
            Err(other) => panic!("{label}: expected ForeignBlock, got {other}"),
            Ok(()) => panic!("{label} accepted a block from another arena"),
        }
    }

    // A zero-length write is refused for a foreign block too: the identity check comes before the
    // early return, because "nothing happened" is not a reason to accept a block that is not ours.
    assert!(matches!(two.write(&block, 0, &[]), Err(MemError::ForeignBlock { .. })));

    // And the refusals cost the refusing arena nothing: it never touched a chunk.
    assert_eq!(two.stats().chunks, 0, "arena two never allocated anything");
}

/// The block of a *dropped* arena is refused by a surviving one, which is the use-after-free the
/// identity check exists to stop. Nothing here dereferences the stale address.
#[test]
fn a_block_outliving_its_arena_cannot_be_used_through_a_different_arena() {
    let survivor = CodeArena::new().expect("survivor");
    let stale = {
        let short_lived = CodeArena::new().expect("short-lived");
        short_lived.alloc(4096).expect("allocate")
        // `short_lived` is dropped here, and its two views are unmapped. `stale` is still a
        // perfectly good value naming addresses that no longer exist.
    };
    assert_ne!(stale.arena(), survivor.id());
    assert!(
        matches!(survivor.write(&stale, 0, &[0x90; 16]), Err(MemError::ForeignBlock { .. })),
        "a write through a dropped arena's block must be refused, not attempted"
    );
    assert!(matches!(survivor.seal(&stale), Err(MemError::ForeignBlock { .. })));
    assert!(matches!(survivor.unseal(&stale), Err(MemError::ForeignBlock { .. })));
}

/// Two arenas emit and execute code at once, each keeping W^X, which is the shape per-thread code
/// caches will have.
#[cfg(target_arch = "x86_64")]
#[test]
fn two_arenas_emit_and_execute_independently_and_both_keep_w_xor_x() {
    let arenas = [CodeArena::new().expect("one"), CodeArena::new().expect("two")];
    let mut blocks = Vec::new();
    for (index, arena) in arenas.iter().enumerate() {
        let block = arena.alloc(16).expect("allocate");
        arena
            .write(&block, 0, &returns_constant(0x1000 + index as u32))
            .expect("emit through the writable view");
        // The two views are distinct address ranges in every arena, so no single address is ever
        // both writable and executable.
        assert_ne!(block.view_distance(), 0, "arena {index} has overlapping views");
        blocks.push(block);
    }
    for (index, block) in blocks.iter().enumerate() {
        // SAFETY: the block holds six bytes of `mov eax, imm32; ret`, emitted just above through the
        // writable view of the same pages, and the executable view is `PAGE_EXECUTE_READ`. The
        // function takes no arguments, touches no stack and returns in a register.
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(block.exec_ptr()) };
        assert_eq!(f(), 0x1000 + index as u32, "arena {index} executed its own code");
    }
    // Sealing one arena's block leaves the other arena's block writable: they share nothing.
    arenas[0].seal(&blocks[0]).expect("seal");
    arenas[1].write(&blocks[1], 0, &returns_constant(0x2001)).expect("the other is unaffected");
    // SAFETY: as above; the block was just rewritten with a valid six-byte function.
    let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(blocks[1].exec_ptr()) };
    assert_eq!(f(), 0x2001, "the patched code runs through the executable view");
    arenas[0].unseal(&blocks[0]).expect("unseal");
    arenas[0].write(&blocks[0], 0, &returns_constant(0x3001)).expect("patch after unseal");
    // SAFETY: as above.
    let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(blocks[0].exec_ptr()) };
    assert_eq!(f(), 0x3001);
}
