//! The code arena, exercised the way a JIT actually uses it: emit, execute, seal, patch, re-execute
//! — across two arenas at once, with [`CommitBudget`] asserted at every step.
//!
//! # Why this suite exists separately from `tests/arena.rs`
//!
//! `tests/arena.rs` proves the arena's *mechanics*: two views, no overlap, identity, refusals. This
//! one proves the three properties those tests cannot reach, each of which is a thing a translator
//! will depend on from its first day:
//!
//! 1. **Emitted code can branch to other emitted code.** A JIT computes a branch displacement from
//!    the addresses the code will *run* at, and writes it through a view at a completely different
//!    address. Get that wrong — compute the displacement from the writable view — and every block
//!    still writes and reads back correctly, every existing test still passes, and the first chained
//!    call jumps into nothing. The chain here is the only thing that catches it.
//! 2. **Several arenas at once is the real shape.** D5 measured dynarmic committing **20-35 MiB per
//!    guest thread with fully duplicated code caches**, and Roblox is heavily multithreaded, so one
//!    arena per guest thread is what M2 will have. Two arenas emit, execute and patch here
//!    simultaneously, and each is asserted to compute its own answer.
//! 3. **The arena's cost is invisible to the counter that watches everything else.** D15: a
//!    pagefile-backed section is charged against the system commit limit in full when it is created,
//!    and does not appear in `PrivateUsage` — which is what
//!    [`omni_mem::process_commit_charge`] returns. [`CommitBudget`] exists to close that gap, and
//!    this is the only place the gap is instrumented against a realistic volume of generated code.
//!
//! Every execution here asserts a **computed** value, not merely that the code ran: each emitted
//! function computes an affine map over its argument, the expected result is folded independently in
//! Rust from the same constants, and the constants differ per block and per arena. A block that
//! aliased another, a patch that did not reach the executable view, or a chain that jumped to the
//! wrong place all produce a different number rather than a crash.

// -------------------------------------------------------------------------------------------
// The visible skip. `target_arch = "x86_64"` gates everything below, and a gate that simply makes
// tests vanish is indistinguishable from a suite nobody wrote.
// -------------------------------------------------------------------------------------------

/// Deliberately reported as ignored, with the reason, on every host that cannot run this suite.
///
/// `cargo test` prints `ignored, <reason>` in its default output, so the skip is in the same place a
/// pass would be. Forcing it with `--ignored` fails loudly rather than passing vacuously.
#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
#[test]
#[ignore = "SKIPPED: this suite emits raw x86-64 machine code and runs it, so it needs \
            target_arch = \"x86_64\" on Windows. The arena's mechanics are still covered by \
            tests/arena.rs; its code *execution* is not covered on this host."]
fn generated_code_is_executed_only_on_x86_64_windows() {
    panic!(
        "this suite emits x86-64 machine code and cannot run on target_arch = \"{}\" / \
         target_os = \"{}\"; it is marked #[ignore] for that reason and must not be forced with \
         --ignored",
        std::env::consts::ARCH,
        std::env::consts::OS
    );
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod x86_64 {
    use std::sync::Mutex;

    use omni_mem::{ArenaConfig, CodeArena, CodeBlock, CommitBudget, GuestSpace, MemError};
    use omni_platform::vm::page_size;

    const MIB: usize = 1024 * 1024;

    /// **Every** test in this binary takes this, not just the two that measure.
    ///
    /// `CommitBudget::process_private` is `PrivateUsage`, which is **process-global**: it counts
    /// every allocation any thread in this process makes. libtest runs these four tests in parallel
    /// by default, so serialising only the measuring tests left the other two churning the heap
    /// underneath the measurement — and the real signal turned out to be **4-16 KiB**, far smaller
    /// than that churn. That reproduced as `private_delta` going *negative* in roughly one run in
    /// ten.
    ///
    /// It mattered more than a flaky test usually would: a spurious failure here appears in
    /// `tools/mutate.py`'s output as a mutation "caught" by a test that has nothing to do with it,
    /// which silently corrupts the one table Global Constraint 12 asks us to trust.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// No guest address spaces are involved here; only arenas are.
    ///
    /// Generic over the lifetime so that it unifies with the arenas' borrow rather than forcing
    /// `'static` on both halves of [`CommitBudget::measure`].
    fn no_spaces<'a>() -> std::iter::Empty<&'a GuestSpace> {
        std::iter::empty()
    }

    /// Bytes of arena each of the two arenas fills in the cost measurement.
    ///
    /// Chosen to be a realistic per-guest-thread code cache rather than a round number that proves
    /// nothing: D5 measured **20-35 MiB per guest thread**, and dynarmic produces **12-30 bytes of
    /// host code per guest instruction**, so 8 MiB per arena is on the order of 280,000-700,000
    /// translated guest instructions, and the two arenas together sit just under D5's lower bound
    /// for a single guest thread.
    const REALISTIC_CODE_PER_ARENA: usize = 8 * MIB;

    /// One translated block's worth of arena. 256 bytes is 9-21 guest instructions at D5's measured
    /// expansion, which is a plausible basic block.
    const BLOCK: usize = 256;

    /// Blocks in the chained-execution test. Small; the point is the branches between them.
    const CHAIN_LEN: usize = 8;

    /// Size of a chain block. The largest form emitted below is 18 bytes; 32 keeps every block
    /// 16-byte aligned with room to be patched into any of the three forms.
    const CHAIN_BLOCK: usize = 32;

    /// One step of the computation each emitted function performs: `x -> x * m + a`, on `u32`.
    type Step = (u32, u32);

    /// The value the emitted code must produce, folded independently in Rust.
    ///
    /// This is what makes the assertions non-vacuous: it is computed from the same constants the
    /// bytes were built from, by different code, without running anything.
    fn predict(x: u32, steps: &[Step]) -> u32 {
        steps.iter().fold(x, |acc, &(m, a)| acc.wrapping_mul(m).wrapping_add(a))
    }

    /// x86-64 for `eax <- eax * m + a`, 11 bytes.
    ///
    /// `69 /r id` is `imul r32, r/m32, imm32` with ModRM `C0` selecting `eax, eax`; `05 id` is
    /// `add eax, imm32`. Both wrap on overflow, which is why [`predict`] uses `wrapping_*`.
    fn affine(m: u32, a: u32) -> [u8; 11] {
        let mut code = [0x69, 0xC0, 0, 0, 0, 0, 0x05, 0, 0, 0, 0];
        code[2..6].copy_from_slice(&m.to_le_bytes());
        code[7..11].copy_from_slice(&a.to_le_bytes());
        code
    }

    /// A standalone `extern "C" fn(u32) -> u32` computing `x * m + a`, 14 bytes.
    ///
    /// `89 C8` is `mov eax, ecx` — the Microsoft x64 ABI passes the first integer argument in `RCX`
    /// and returns in `EAX` — and `C3` is `ret`. No prologue, no stack use, no callee-saved register
    /// touched, so it is callable directly from Rust.
    ///
    /// Returns an array rather than a `Vec` on purpose: the cost measurement emits 65,536 of these
    /// *between* its two `PrivateUsage` readings, and 65,536 heap allocations inside the window is
    /// the measurement measuring itself. It was — the delta fell from about 102,400 bytes to 4-16
    /// KiB once this stopped allocating.
    fn standalone(m: u32, a: u32) -> [u8; 14] {
        let mut code = [0u8; 14];
        code[0..2].copy_from_slice(&[0x89, 0xC8]); // mov eax, ecx
        code[2..13].copy_from_slice(&affine(m, a));
        code[13] = 0xC3; // ret
        code
    }

    /// One block of a chain, ending in a `jmp rel32` to the next block or in `ret`.
    ///
    /// `this_exec` and `next_exec` are addresses in the **executable** view, because that is where
    /// the code will run — even though the bytes are written through a view at a different address
    /// entirely. Computing the displacement from the writable addresses instead would produce a
    /// chain that writes and reads back perfectly and jumps into unmapped memory when called.
    fn chain_block(
        this_exec: usize,
        first: bool,
        (m, a): Step,
        next_exec: Option<usize>,
    ) -> Vec<u8> {
        let mut code = Vec::with_capacity(CHAIN_BLOCK);
        if first {
            code.extend_from_slice(&[0x89, 0xC8]); // mov eax, ecx
        }
        code.extend_from_slice(&affine(m, a));
        match next_exec {
            Some(target) => {
                // `E9 cd`: the displacement is relative to the address of the instruction *after*
                // the jump, which is five bytes past its opcode.
                let after = this_exec + code.len() + 5;
                let displacement = i32::try_from(target as i64 - after as i64).expect(
                    "two blocks of one arena chunk are within 2 GiB of each other, so a rel32 \
                     displacement always fits",
                );
                code.push(0xE9);
                code.extend_from_slice(&displacement.to_le_bytes());
            }
            None => code.push(0xC3), // ret
        }
        assert!(code.len() <= CHAIN_BLOCK, "a chain block does not fit its allocation");
        code
    }

    /// Call a block through its executable view.
    ///
    /// # Safety
    ///
    /// The block must hold a complete `extern "C" fn(u32) -> u32` emitted by the helpers above:
    /// argument in `ECX`, result in `EAX`, no stack use beyond the return address, no callee-saved
    /// register touched. Every caller in this file emits exactly that, and the executable view is
    /// `PAGE_EXECUTE_READ`, so the bytes are the ones that were written through the writable view of
    /// the same pages.
    unsafe fn call(block: &CodeBlock, argument: u32) -> u32 {
        let function: extern "C" fn(u32) -> u32 = std::mem::transmute(block.exec_ptr());
        function(argument)
    }

    /// Emit a chain of blocks that branch to one another, and return them with their steps.
    fn emit_chain(arena: &CodeArena, seed: u32) -> (Vec<CodeBlock>, Vec<Step>) {
        let steps: Vec<Step> =
            (0..CHAIN_LEN).map(|i| (seed.wrapping_add(i as u32 * 7) | 1, seed ^ (i as u32))).collect();
        // Allocate every block first: a chain block's bytes depend on where the *next* block will
        // run, which is exactly the order a real translator works in.
        let blocks: Vec<CodeBlock> =
            (0..CHAIN_LEN).map(|_| arena.alloc(CHAIN_BLOCK).expect("allocate a chain block")).collect();
        write_chain(arena, &blocks, &steps);
        (blocks, steps)
    }

    /// (Re)write every block of a chain from `steps`. Used for the initial emission and for patching.
    fn write_chain(arena: &CodeArena, blocks: &[CodeBlock], steps: &[Step]) {
        for (index, block) in blocks.iter().enumerate() {
            let next = blocks.get(index + 1).map(|next| next.exec_ptr() as usize);
            let code = chain_block(block.exec_ptr() as usize, index == 0, steps[index], next);
            arena.write(block, 0, &code).expect("emit a chain block");
        }
    }

    // ---------------------------------------------------------------------------------------
    // The test the brief asks for.
    // ---------------------------------------------------------------------------------------

    /// Two arenas emit chained code, execute it, seal it, patch it and execute it again, with
    /// `CommitBudget` asserted at each stage.
    ///
    /// Everything here is asserted against an independently predicted value. In particular the
    /// patch step asserts the number the *second* emission produces: if the writable view were ever
    /// privatised on the way back from sealed — the silent failure D12 exists to prevent, which
    /// reports no error anywhere — the chain would keep returning its original answer and this is
    /// what would notice.
    #[test]
    fn two_arenas_emit_chain_seal_patch_and_re_execute_while_the_budget_tracks_the_invisible_half() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());

        let arenas = [CodeArena::new().expect("arena one"), CodeArena::new().expect("arena two")];
        assert_ne!(arenas[0].id(), arenas[1].id());

        // Before anything is emitted the arenas cost nothing, and the budget says so.
        let idle = CommitBudget::measure(no_spaces(), &arenas).expect("measure");
        assert_eq!(idle.arena_mapped, 0, "an arena maps nothing until it is asked for a block");
        assert_eq!(idle.invisible_to_process_counter(), 0);
        assert_eq!(
            idle.total_system_commit(),
            idle.process_private,
            "with nothing mapped, the total is exactly the private figure"
        );

        // Emit a differently-parameterised chain in each arena.
        let mut chains = Vec::new();
        for (index, arena) in arenas.iter().enumerate() {
            chains.push(emit_chain(arena, 0x1000_0001 + index as u32 * 0x0010_0000));
        }

        let emitted = CommitBudget::measure(no_spaces(), &arenas).expect("measure");
        let mapped: usize = arenas.iter().map(|arena| arena.stats().mapped).sum();
        assert!(mapped > 0, "emitting code must have grown the arenas");
        assert_eq!(emitted.arena_mapped, mapped, "the budget must sum every arena it was given");
        assert_eq!(
            emitted.invisible_to_process_counter(),
            mapped,
            "every byte of arena is invisible to process_commit_charge (D15)"
        );
        assert_eq!(
            emitted.total_system_commit(),
            emitted.process_private + mapped as u64,
            "the total must add the invisible half, or the budget is the counter it replaces"
        );
        assert!(
            (emitted.process_private as i64 - idle.process_private as i64) < mapped as i64 / 4,
            "the arena's sections must not appear in PrivateUsage: private moved by {} bytes while \
             {mapped} bytes were mapped",
            emitted.process_private as i64 - idle.process_private as i64
        );

        // Execute both chains. Each must compute its own answer.
        const ARGUMENT: u32 = 0x0BAD_F00D;
        for (index, (blocks, steps)) in chains.iter().enumerate() {
            let expected = predict(ARGUMENT, steps);
            // SAFETY: `blocks[0]` is the head of a chain emitted above: `mov eax, ecx`, a sequence
            // of `imul`/`add` reached by `jmp rel32`, and a final `ret`. See `call`.
            let actual = unsafe { call(&blocks[0], ARGUMENT) };
            assert_eq!(actual, expected, "arena {index} computed the wrong value");
        }
        assert_ne!(
            predict(ARGUMENT, &chains[0].1),
            predict(ARGUMENT, &chains[1].1),
            "the two arenas must be asserted against different answers, or the test proves nothing \
             about isolation"
        );

        // Seal every block. Sealed code still runs; it just cannot be written through.
        for (arena, (blocks, _)) in arenas.iter().zip(&chains) {
            for block in blocks {
                arena.seal(block).expect("seal");
            }
        }
        for (index, (blocks, steps)) in chains.iter().enumerate() {
            // SAFETY: as above. Sealing changes the writable view only.
            let actual = unsafe { call(&blocks[0], ARGUMENT) };
            assert_eq!(actual, predict(ARGUMENT, steps), "arena {index} broke when sealed");
        }
        // A sealed block refuses a write, and says which page stopped it. This is the check that
        // keeps `write` — a *safe* function — from storing through a `PAGE_READONLY` view and
        // killing the process, which is what it did before this suite was written.
        let (blocks, _) = &chains[0];
        assert_eq!(
            arenas[0].stats().sealed,
            page_size(),
            "eight 32-byte blocks are 256 bytes and share one page, so sealing them all seals \
             exactly one page — sealing is page-granular and `stats` must say so rather than \
             reporting the 256 bytes that were asked for"
        );
        match arenas[0].write(&blocks[0], 0, &[0x90]) {
            Err(MemError::BlockSealed { write, offset, len, page }) => {
                assert_eq!((write, offset, len), (blocks[0].write_ptr() as usize, 0, 1));
                assert_eq!(
                    page,
                    blocks[0].write_ptr() as usize & !(page_size() - 1),
                    "the refusal must name the sealed page"
                );
            }
            other => panic!("a write to a sealed block must be refused, got {other:?}"),
        }
        // The executable view is untouched by any of that: the code still runs.
        // SAFETY: as above.
        assert_eq!(unsafe { call(&blocks[0], ARGUMENT) }, predict(ARGUMENT, &chains[0].1));

        // Patch arena one only: unseal, rewrite every block from new constants, seal again.
        let patched_steps: Vec<Step> =
            chains[0].1.iter().map(|&(m, a)| (m ^ 0x0F0F_0F0F | 1, a ^ 0x00F0_F0F0)).collect();
        for block in &chains[0].0 {
            arenas[0].unseal(block).expect("unseal");
        }
        write_chain(&arenas[0], &chains[0].0, &patched_steps);
        for block in &chains[0].0 {
            arenas[0].seal(block).expect("seal again");
        }

        let expected = predict(ARGUMENT, &patched_steps);
        assert_ne!(expected, predict(ARGUMENT, &chains[0].1), "the patch must change the answer");
        // SAFETY: as above; the chain was rewritten in place with the same shape.
        let actual = unsafe { call(&chains[0].0[0], ARGUMENT) };
        assert_eq!(
            actual, expected,
            "the patched chain did not reach the executable view: either the writable view was \
             privatised on the way back from sealed (the silent failure D12 exists to prevent) or \
             the rel32 displacements were computed from the wrong view"
        );

        // Arena two was not touched, and still computes its own original answer.
        // SAFETY: as above.
        let untouched = unsafe { call(&chains[1].0[0], ARGUMENT) };
        assert_eq!(
            untouched,
            predict(ARGUMENT, &chains[1].1),
            "patching one arena changed another arena's code"
        );

        // Dropping the arenas gives the mapping back, and the budget sees it go.
        let before_drop = CommitBudget::measure(no_spaces(), &arenas).expect("measure");
        assert_eq!(before_drop.arena_mapped, mapped);
        drop(arenas);
        let after = CommitBudget::measure(no_spaces(), std::iter::empty::<&CodeArena>()).expect("measure");
        assert_eq!(after.arena_mapped, 0);
        assert_eq!(after.invisible_to_process_counter(), 0);
        assert_eq!(after.total_system_commit(), after.process_private);
    }

    /// What a realistic amount of generated code actually costs, measured.
    ///
    /// Two arenas — one per guest thread is M2's expected shape (D5) — each filled with 8 MiB of
    /// emitted functions, a sample of which are executed and checked against independently predicted
    /// values. The arena figures are asserted **exactly**: 8 MiB of 256-byte blocks in 1 MiB chunks
    /// wastes nothing, so `mapped == used == 8 MiB` per arena. That exactness is the assertion, not
    /// a decoration: an arena that mapped its whole 256 MiB ceiling up front instead of growing a
    /// chunk at a time would pass every functional test in this crate and charge the system commit
    /// limit thirty-two times over.
    #[test]
    fn the_measured_cost_of_a_realistic_amount_of_generated_code() {
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());

        let arenas = [CodeArena::new().expect("arena one"), CodeArena::new().expect("arena two")];
        let baseline = CommitBudget::measure(no_spaces(), &arenas).expect("measure");
        assert_eq!(baseline.arena_mapped, 0);

        const BLOCKS_PER_ARENA: usize = REALISTIC_CODE_PER_ARENA / BLOCK;
        // Every 512th block is kept and executed. Keeping all 32,768 per arena would put ~2.6 MiB of
        // `Vec` on the heap, which is private commit, and would muddy the very measurement this test
        // exists to take.
        const SAMPLE_EVERY: usize = 512;

        // Reserved rather than grown. Everything emitted between the two readings is built in
        // fixed-size arrays (see `standalone`) and every container is sized up front, so the only
        // thing that moves `PrivateUsage` inside the measurement window is the arenas themselves.
        let mut samples: Vec<Vec<(CodeBlock, Step)>> = Vec::with_capacity(arenas.len());
        for (arena_index, arena) in arenas.iter().enumerate() {
            let mut kept = Vec::with_capacity(BLOCKS_PER_ARENA / SAMPLE_EVERY + 2);
            for index in 0..BLOCKS_PER_ARENA {
                // Constants unique to (arena, block), so an aliased or stale block is a wrong
                // number rather than a lucky coincidence.
                let step: Step = (
                    (index as u32).wrapping_mul(2_654_435_761).wrapping_add(arena_index as u32) | 1,
                    (index as u32) ^ 0xA5A5_0000u32.wrapping_add(arena_index as u32),
                );
                let block = arena.alloc(BLOCK).expect("allocate a translated block");
                arena.write(&block, 0, &standalone(step.0, step.1)).expect("emit");
                if index % SAMPLE_EVERY == 0 || index == BLOCKS_PER_ARENA - 1 {
                    kept.push((block, step));
                }
            }
            samples.push(kept);

            let stats = arena.stats();
            assert_eq!(
                stats.used, REALISTIC_CODE_PER_ARENA,
                "arena {arena_index} handed out the wrong number of bytes"
            );
            assert_eq!(
                stats.mapped, REALISTIC_CODE_PER_ARENA,
                "arena {arena_index} mapped {} bytes for {REALISTIC_CODE_PER_ARENA} bytes of code: \
                 the arena must grow a chunk at a time, because a pagefile-backed section is charged \
                 against the system commit limit the moment it is created (D15)",
                stats.mapped
            );
            assert_eq!(stats.wasted, 0, "256-byte blocks divide a 1 MiB chunk exactly");
            assert_eq!(stats.chunks, REALISTIC_CODE_PER_ARENA / MIB);
        }

        // Every sampled function computes its own value.
        let argument = 0x1234_5678u32;
        let mut executed = 0usize;
        for (arena_index, kept) in samples.iter().enumerate() {
            for (block, step) in kept {
                // SAFETY: each block holds the 14-byte `standalone` function emitted just above.
                let actual = unsafe { call(block, argument) };
                assert_eq!(
                    actual,
                    predict(argument, std::slice::from_ref(step)),
                    "arena {arena_index} block at {:#x} computed the wrong value",
                    block.exec_ptr() as usize
                );
                executed += 1;
            }
        }
        // Every 512th block plus the last, which is not a multiple of 512.
        assert_eq!(executed, 2 * (BLOCKS_PER_ARENA / SAMPLE_EVERY + 1), "sampled block count");

        let full = CommitBudget::measure(no_spaces(), &arenas).expect("measure");
        let mapped = 2 * REALISTIC_CODE_PER_ARENA;
        assert_eq!(full.arena_mapped, mapped);
        assert_eq!(full.invisible_to_process_counter(), mapped);
        let private_delta = full.process_private as i64 - baseline.process_private as i64;
        eprintln!(
            "MEASURED: {} MiB of generated code across 2 arenas ({} blocks of {BLOCK} bytes each) \
             cost {} bytes of PrivateUsage and {} bytes of system commit that PrivateUsage does not \
             count. Budget: {full}",
            mapped / MIB,
            2 * BLOCKS_PER_ARENA,
            private_delta,
            full.invisible_to_process_counter(),
        );
        // **One two-sided bound, not a lower bound at zero.**
        //
        // The first version of this asserted `private_delta > 0`, reasoning that two views must at
        // least cost page tables at D10's measured `size/512` — 65,536 bytes for 32 MiB of mapping.
        // Two things were wrong. It had no margin at all on the side it actually failed on. And it
        // was measuring the wrong thing: once this binary was serialised and the emission loop
        // stopped allocating, the delta collapsed from about 102,400 bytes to **4,096-16,384**, one
        // to four pages, across twelve runs. Almost all of the original figure was this test's own
        // heap, and reporting it as the arena's cost was false precision.
        //
        // Two conclusions, both recorded rather than smoothed over. D10's `size/512` page-table
        // model was measured on committed *anonymous* memory and does not transfer to a mapped
        // section view. And one to four pages is indistinguishable from allocator granularity, so
        // asserting that it *is* page tables would be inventing a mechanism from noise.
        //
        // What the measurement does support — and what D15 actually needs — is a two-sided bound:
        // whatever `PrivateUsage` does in either direction, it is negligible beside the bytes
        // charged against the system commit limit. `mapped / 128` sits 8x above the largest movement
        // observed and 128x below what was mapped, and unlike a bound at zero it cannot be failed by
        // noise going the wrong way.
        assert!(
            private_delta.unsigned_abs() as usize <= mapped / 128,
            "the arena's {mapped} bytes must be invisible to process_commit_charge (D15): \
             PrivateUsage moved by {private_delta} bytes, more than the {} that would still count \
             as negligible",
            mapped / 128
        );

        drop(arenas);
        let after = CommitBudget::measure(no_spaces(), std::iter::empty::<&CodeArena>()).expect("measure");
        let left_behind = after.process_private as i64 - baseline.process_private as i64;
        eprintln!("MEASURED: after dropping both arenas, PrivateUsage is {left_behind} bytes above \
                   the baseline");
        assert!(
            left_behind.abs() < mapped as i64 / 8,
            "dropping the arenas left {left_behind} bytes behind"
        );
    }

    /// The executable view is still not writable after a seal-and-unseal cycle.
    ///
    /// `tests/arena.rs` proves this for a block straight out of `alloc`. A JIT does not leave blocks
    /// there: it seals them and unseals them to patch, and `unseal` is the one call that reaches
    /// `vm::protect`. If that call were ever given the executable view's address, or if the two
    /// views' protections were ever confused, a freshly unsealed block would be writable **and**
    /// executable — a W+X page produced by an API that cannot name one. So the check is repeated on
    /// the far side of the cycle.
    ///
    /// Run in a child process, because the whole point is that the store is not survivable.
    #[test]
    fn the_executable_view_is_still_not_writable_after_a_seal_and_unseal_cycle() {
        // Serialised with every other test in this binary: it spawns a child and allocates, both of
        // which move the process-global `PrivateUsage` the cost tests read. See `SERIAL`.
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        const CHILD: &str = "OMNI_MEM_ARENA_PATCHED_EXEC_WRITE_CHILD";
        const NO_FAULT: i32 = 7;
        /// `STATUS_ACCESS_VIOLATION`.
        const ACCESS_VIOLATION: i32 = 0xC000_0005u32 as i32;
        const NAME: &str = "x86_64::the_executable_view_is_still_not_writable_after_a_seal_and_unseal_cycle";

        if std::env::var_os(CHILD).is_some() {
            let arena = CodeArena::new().expect("create the arena");
            let block = arena.alloc(CHAIN_BLOCK).expect("allocate");
            arena.write(&block, 0, &standalone(3, 4)).expect("emit");
            arena.seal(&block).expect("seal");
            arena.unseal(&block).expect("unseal");
            arena.write(&block, 0, &standalone(5, 6)).expect("patch");
            // SAFETY: the patched block holds the 14-byte `standalone` function. Executed first so
            // that the child proves it reached a *working* patched block before testing the store.
            let value = unsafe { call(&block, 10) };
            assert_eq!(value, 56, "the patched block must compute 10 * 5 + 6");
            // SAFETY: none, deliberately. This store is expected to raise an access violation and
            // kill this process: the executable view is `PAGE_EXECUTE_READ` and nothing above may
            // have changed that. `write_volatile` so it cannot be optimised away.
            unsafe {
                std::ptr::write_volatile(block.exec_ptr().cast_mut(), 0x90);
            }
            std::process::exit(NO_FAULT);
        }

        let status = std::process::Command::new(std::env::current_exe().expect("the test binary"))
            .args([NAME, "--exact", "--nocapture"])
            .env(CHILD, "1")
            .status()
            .expect("run the child");
        let code = status.code();
        assert_ne!(
            code,
            Some(NO_FAULT),
            "a block that had been sealed and unsealed was writable through its executable view: \
             that is a W+X mapping produced by an API with no W+X in it"
        );
        assert_eq!(
            code,
            Some(ACCESS_VIOLATION),
            "expected STATUS_ACCESS_VIOLATION ({ACCESS_VIOLATION:#x}) from the child, got {code:?}"
        );
    }

    /// Hostile arena inputs are refused, and the arena still emits and executes afterwards.
    ///
    /// Global Constraint 11. The last four lines are the ones that matter: a refusal that leaves the
    /// arena in a state where the next emission is wrong would pass every test that only checks the
    /// refusal.
    #[test]
    fn hostile_arena_inputs_are_refused_and_the_arena_still_works_afterwards() {
        // Serialised for the reason given on `SERIAL`: this test maps arenas and allocates, and
        // `PrivateUsage` is process-global.
        let _serial = SERIAL.lock().unwrap_or_else(|error| error.into_inner());
        let arena = CodeArena::new().expect("arena");
        let block = arena.alloc(CHAIN_BLOCK).expect("a real block");

        assert!(matches!(arena.alloc(0), Err(MemError::ZeroSize { .. })));
        for (offset, len) in [(usize::MAX, 1usize), (CHAIN_BLOCK, 1), (0, CHAIN_BLOCK + 1), (1, CHAIN_BLOCK)] {
            let bytes = vec![0x90u8; len];
            match arena.write(&block, offset, &bytes) {
                Err(MemError::BlockOverflow { offset: o, len: l, block_len }) => {
                    assert_eq!((o, l, block_len), (offset, len, CHAIN_BLOCK));
                }
                other => panic!("a write of {len} at {offset:#x} must be refused, got {other:?}"),
            }
        }

        // A configuration whose chunk size cannot be rounded up without overflowing must be refused
        // at the first allocation, not wrapped past the ceiling check into a mapping attempt.
        let absurd = CodeArena::with_config(ArenaConfig {
            chunk_size: usize::MAX,
            max_total: usize::MAX,
            block_alignment: 16,
        })
        .expect("the configuration itself is well-formed");
        match absurd.alloc(64) {
            Err(MemError::ArenaFull { requested, in_use, limit }) => {
                assert_eq!((requested, in_use, limit), (64, 0, usize::MAX));
            }
            other => panic!("an unroundable chunk size must be refused, got {other:?}"),
        }
        assert_eq!(absurd.stats().chunks, 0, "nothing may be mapped for a refused request");

        // A block from another arena is refused by every operation that would dereference it.
        let other = CodeArena::new().expect("another arena");
        for (label, result) in [
            ("write", other.write(&block, 0, &[0x90])),
            ("seal", other.seal(&block)),
            ("unseal", other.unseal(&block)),
        ] {
            assert!(
                matches!(result, Err(MemError::ForeignBlock { .. })),
                "{label} accepted a block minted by another arena"
            );
        }

        // And after every one of those refusals the arena still emits code that runs correctly.
        arena.write(&block, 0, &standalone(11, 13)).expect("emit after the refusals");
        // SAFETY: the block holds the 14-byte `standalone` function just written.
        assert_eq!(unsafe { call(&block, 7) }, 7 * 11 + 13);
        let second = arena.alloc(CHAIN_BLOCK).expect("the arena still allocates");
        arena.write(&second, 0, &standalone(2, 1)).expect("emit");
        // SAFETY: as above.
        assert_eq!(unsafe { call(&second, 21) }, 43);
    }
}
