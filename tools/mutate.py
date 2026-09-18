"""Mutation testing across the workspace, table-driven.

    python tools/mutate.py               # from the repository root
    python tools/mutate.py --only mem    # one prefix
    python tools/mutate.py --list

Global Constraint 12: a test that does not fail when the logic it covers is reverted is not
evidence. `crates/omni-elf/tools/mutate_loader.py` made that checkable for the loader; this is the
generalisation the whole-branch review asked for — it takes a table of (file, old, new, command)
rather than being loader-shaped, so a new fix anywhere in the workspace costs one table row.

Each mutation is applied on its own, the named test command is run, the result is recorded, and the
file is restored via `try`/`finally` -- including on a crash, but **not** if the interpreter is
killed. That gap is real and has been hit: a run killed mid-row left `arena.rs` carrying its
mutation, and `git status` showed only "modified", which is what the file looks like during ordinary
work. The pre-flight below is what turns that from a silent corruption into a one-second refusal on
the next run, because a stale tree makes some pattern fail to match. If a run is ever killed, check
`git diff` before trusting the tree.

A mutation that does not compile, does not match its pattern, or is caught by nothing is reported as
`MISS`, never as a pass: a mutation nothing notices means the fix has no test behind it.

Two directions, and the second is the point:

* **A** reverts a fix. Something must fail.
* **B** over-corrects — bounds something that should not be bounded, commits eagerly where the
  design commits lazily. These read as correct and destroy a measured property, and they are the
  direction that is normally missing.

`--no-fail-fast` is not optional: without it `cargo test` stops after the first failing binary and
silently attributes every mutation to whichever binary happened to run first.
"""

import argparse
import subprocess
import sys
import time

PLAT = "crates/omni-platform/src/vm/mod.rs"
SPACE = "crates/omni-mem/src/space.rs"
ARENA = "crates/omni-mem/src/arena.rs"
BUDGET = "crates/omni-mem/src/budget.rs"
LOADER = "crates/omni-elf/src/loader/mod.rs"
ZIP = "crates/omni-apk/src/zip.rs"
CPU_CONTEXT = "crates/omni-cpu/src/context.rs"
CPU_REGS = "crates/omni-cpu/src/regs.rs"
CPU_FASTMEM = "crates/omni-cpu/src/fastmem.rs"
CPU_RUN = "crates/omni-cpu/src/run.rs"
CPU_TLS = "crates/omni-cpu/src/tls.rs"
CPU_CLOCK = "crates/omni-cpu/src/clock.rs"
CPU_CALLBACKS = "crates/omni-cpu/src/dynarmic/callbacks.rs"
CPU_DYN = "crates/omni-cpu/src/dynarmic/mod.rs"
PAGER = "crates/omni-mem/src/pager.rs"
FAULT = "crates/omni-platform/src/fault/windows.rs"
EH_FRAME = "crates/omni-elf/src/eh_frame.rs"
LEAF = "crates/omni-elf/src/leaf.rs"

# Commands, kept narrow so the whole run stays under a few minutes.
MEM = ["cargo", "test", "-p", "omni-mem", "--no-fail-fast"]
CPU = ["cargo", "test", "-p", "omni-cpu", "--no-fail-fast"]
PLATFORM = ["cargo", "test", "-p", "omni-platform", "--no-fail-fast"]
# The demand pager is policy in `omni-mem` driven by execution in `omni-cpu`, so a mutation of it
# has to run both: its unit tests live with the code and its behavioural tests live with the guest
# that provokes the faults. A row scoped to one of the two reported a MISS that was a gap in the
# harness rather than in the tests, which is worth leaving written down.
MEM_AND_CPU = ["cargo", "test", "-p", "omni-mem", "-p", "omni-cpu", "--no-fail-fast"]
ELF = ["cargo", "test", "-p", "omni-elf", "--no-fail-fast"]
APK = ["cargo", "test", "-p", "omni-apk", "--no-fail-fast"]
# The leaf scan decodes 245,117 function bodies, which is minutes in a debug build and a quarter of
# a second in release. Its own command rather than widening `ELF`, so the rest of the ELF rows keep
# running against the build everything else uses.
ELF_SCAN = [
    "cargo", "test", "-p", "omni-elf", "--release", "--lib", "--test", "eh_frame_golden",
    "--no-fail-fast",
]
# The commit-charge figures are only meaningful in a release build.
ELF_RELEASE = [
    "cargo", "test", "-p", "omni-elf", "--release", "--test", "loader_commit", "--no-fail-fast",
]

# (id, direction, description, file, old, new, command)
MUTATIONS = [
    # ---- the commit ceiling: the Critical -------------------------------------------------------
    ("mem-A1", "A", "per-request commit ceiling removed", SPACE,
     """        if len > self.max_commit_request {""",
     """        if false && len > self.max_commit_request {""",
     MEM),

    ("mem-A2", "A", "total commit ceiling removed", SPACE,
     """        if would_total > self.max_committed {""",
     """        if false && would_total > self.max_committed {""",
     MEM),

    ("mem-A3", "A", "the ceiling checked after the commit instead of before", SPACE,
     """            self.check_commit_allowed(operation, from, to - from)?;

            self.make_exact_placeholder(operation, from, to - from, false)?;""",
     """            self.make_exact_placeholder(operation, from, to - from, false)?;""",
     MEM),

    ("mem-A4", "A", "committed total never comes back down on unmap", SPACE,
     """                    self.committed -= entry.len;
                    self.map.free_range(start, entry.len);
                    position = entry_end;""",
     """                    self.map.free_range(start, entry.len);
                    position = entry_end;""",
     MEM),

    ("mem-A5", "A", "committed total never comes back down on reclaim_idle", SPACE,
     """            entry.os = OsState::Placeholder;
            self.committed -= len;""",
     """            entry.os = OsState::Placeholder;""",
     MEM),

    ("mem-A6", "A", "a refused eager commit leaves its mapping behind", SPACE,
     """                if let Err(rollback) = inner.unmap_range(OP, address, size) {""",
     """                if let Err(rollback) = (if true { Ok(()) } else { inner.unmap_range(OP, address, size) }) {""",
     MEM),

    # ---- the ceilings must separate the attack from legitimate growth ---------------------------
    # The pair only works if each stays on its own side of the gap, so both sides are mutated. A1-A3
    # above prove the attack is refused; these two prove the legitimate case is not, which is the
    # direction that would let a security fix quietly break the feature.
    ("mem-B2", "B", "the per-request ceiling tightened below a legitimate eager mapping", SPACE,
     """pub const DEFAULT_MAX_COMMIT_REQUEST: usize = 128 * 1024 * 1024;""",
     """pub const DEFAULT_MAX_COMMIT_REQUEST: usize = 32 * 1024 * 1024;""",
     MEM),

    ("mem-B3", "B", "the total ceiling lowered below D10's validated 3 GB of live use", SPACE,
     """pub const DEFAULT_MAX_COMMITTED: usize = 3584 * 1024 * 1024;""",
     """pub const DEFAULT_MAX_COMMITTED: usize = 2048 * 1024 * 1024;""",
     MEM),

    # ---- the ceiling must not bind on the lazy path (direction B) -------------------------------
    ("mem-B1", "B", "lazy commit made eager: a granule becomes the whole mapping", SPACE,
     """                CommitPolicy::Lazy => owner.granule,""",
     """                CommitPolicy::Lazy => owner.mapping_len,""",
     MEM),

    # The first attempt at this mutation made `commit_range`'s lazy granule the whole mapping, which
    # was NOT CAUGHT — correctly, because nothing ever calls `commit_range` on a lazy `.bss` mapping
    # in this library: no relocation targets `.bss`. The mutation has to be where the policy is
    # *chosen*, not where it is applied.
    ("elf-B2", "B", "the .bss policy ignored, so lazy .bss is committed eagerly anyway", LOADER,
     """                    config.bss_commit,""",
     """                    CommitPolicy::Eager,""",
     ELF_RELEASE),

    # ---- arena identity --------------------------------------------------------------------------
    ("mem-A7", "A", "foreign blocks accepted again", ARENA,
     """        if block.arena != self.id {""",
     """        if false && block.arena != self.id {""",
     MEM),

    # Reached through the pure `block_fits_chunk` rather than through `reprotect`: after `check_own`
    # the containment check is unreachable from the public API, which is the point of having both, so
    # the arithmetic is asked directly. The underflow it guards was a release-only defect.
    ("mem-A8", "A", "a block below its chunk no longer refused (the release underflow)", ARENA,
     """    write >= chunk_write""",
     """    write >= chunk_write.saturating_sub(usize::MAX)""",
     MEM),

    ("mem-A9", "A", "an absurd block alignment accepted again", ARENA,
     """        if config.block_alignment > config.chunk_size {""",
     """        if false && config.block_alignment > config.chunk_size {""",
     MEM),

    # ---- the arena's sealed pages, and the budget that instruments what the OS counter cannot see -
    # `CodeArena::write` is a *safe* function that stores through the writable view, and `seal` makes
    # that view PAGE_READONLY. Reverting the check does not make a test fail politely: it kills the
    # test process with an access violation, which is exactly the point — that is what safe code
    # could reach before it existed.
    ("mem-A11", "A", "the sealed-page check removed, so a safe write faults the process", ARENA,
     """        let sealed_path = self.sealed_pages.load(Ordering::Acquire) != 0;""",
     """        let sealed_path = false && self.sealed_pages.load(Ordering::Acquire) != 0;""",
     MEM),

    # The guard held across the store (the review's I1). Previously unpinnable without a racing
    # test, which was rightly refused: a test that has to lose a race to fail corrupts this very
    # table. `debug_assert!(!sealed_path || self.inner.is_locked())` makes it deterministic instead,
    # and the slow path's success-path test is what executes it.
    # `let _ = expr` drops the temporary at the end of *that statement*, while `let _name = expr`
    # holds it to the end of scope. So this one-character edit is the real shape of the bug, and it
    # compiles -- `drop(chunks); None` does not, because both arms would then be `None` and the
    # guard type becomes uninferable.
    ("mem-A14", "A", "the arena guard dropped before the store instead of held across it", ARENA,
     """        let _sealed_guard = if sealed_path {""",
     """        let _ = if sealed_path {""",
     MEM),

    ("mem-A12", "A", "the budget stops adding the arena's invisible commit to the total", BUDGET,
     """        self.process_private + self.arena_mapped as u64""",
     """        self.process_private""",
     MEM),

    ("mem-A13", "A", "the budget reports nothing as invisible to the process counter", BUDGET,
     """    pub fn invisible_to_process_counter(&self) -> usize {
        self.arena_mapped
    }""",
     """    pub fn invisible_to_process_counter(&self) -> usize {
        0
    }""",
     MEM),

    # Direction B for the arena, and the reason chunking exists at all. A pagefile-backed section is
    # charged against the system commit limit when it is *created* (D15), so an arena that maps its
    # whole 256 MiB ceiling up front is correct in every functional sense and thirty-two times more
    # expensive for the 8 MiB of code that was actually emitted.
    ("mem-B4", "B", "the arena maps its whole ceiling up front instead of growing a chunk at a time",
     ARENA,
     """pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;""",
     """pub const DEFAULT_CHUNK_SIZE: usize = 256 * 1024 * 1024;""",
     MEM),

    # Direction B for the sealed-page fix itself: a refusal that goes too far. Every hostile-input
    # test still passes — more of them pass, in fact — and the arena can never patch a block again,
    # which is the one thing a JIT must be able to do on invalidation.
    ("mem-B5", "B", "unseal leaves the pages marked sealed, so no block can ever be patched", ARENA,
     """        let changed = chunk.set_sealed(pages, protection == Protection::Read);""",
     """        let changed = chunk.set_sealed(pages, true);""",
     MEM),

    # ---- the CPU seam ----------------------------------------------------------------------------
    # D13: 1,276 of libroblox.so's 1,282 MRS TPIDR_EL0 instructions load [Xt, #0x28], and the first
    # runs before the first static initializer. A thread that can be created without a thread pointer
    # is a thread that crashes inexplicably later.
    ("cpu-A1", "A", "a guest thread may be created with a null TPIDR_EL0 again", CPU_CONTEXT,
     """        if tpidr_el0 == 0 {""",
     """        if false && tpidr_el0 == 0 {""",
     CPU),

    ("cpu-A2", "A", "X31 becomes a general-purpose register again", CPU_REGS,
     """        if index as usize >= Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "X", index: index as u32, count: 31 });""",
     """        if index as usize > Self::COUNT {
            return Err(CpuError::NoSuchRegister { class: "X", index: index as u32, count: 31 });""",
     CPU),

    # Direction B for the thread-pointer check: a stricter rule that is not required by anything.
    # bionic's TLS block is not page-aligned in general, so this refuses legitimate threads while
    # every "hostile input is rejected" assertion keeps passing.
    ("cpu-B1", "B", "the thread pointer is additionally required to be page-aligned", CPU_CONTEXT,
     """        if tpidr_el0 == 0 {""",
     """        if tpidr_el0 == 0 || tpidr_el0 % 4096 != 0 {""",
     CPU),

    # ---- the guest space's other edges -----------------------------------------------------------
    ("mem-A10", "A", "an alignment larger than the space accepted again", SPACE,
     """        if align > self.len {""",
     """        if false && align > self.len {""",
     MEM),

    # ---- the platform seam's descriptor edges ----------------------------------------------------
    ("plat-A1", "A", "zero-length subrange accepted again", PLAT,
     """        check_size("subrange", len)?;""",
     """        if false { check_size("subrange", len)?; }""",
     PLATFORM),

    ("plat-A2", "A", "contains() admits a zero-length range at end()", PLAT,
     """        if len == 0 {
            return false;
        }
        let address = ptr as usize;
        address >= self.base && address < self.end() && len <= self.end() - address""",
     """        let address = ptr as usize;
        address >= self.base && address <= self.end() && len <= self.end() - address""",
     PLATFORM),

    # ---- the loader ------------------------------------------------------------------------------
    ("elf-A1", "A", "parsed bytes and mapped file no longer tied together", LOADER,
     """    if backing.len() != elf.data().len() as u64 {""",
     """    if false && backing.len() != elf.data().len() as u64 {""",
     ELF),

    ("elf-A2", "A", "the plan's anonymous memory is unbounded again", LOADER,
     """    if anonymous > config.max_anonymous_bytes {""",
     """    if false && anonymous > config.max_anonymous_bytes {""",
     ELF),

    # ---- omni-apk --------------------------------------------------------------------------------
    ("apk-A1", "A", "a local header naming a different entry is accepted", ZIP,
     """    if local_name != record.name {""",
     """    if false && local_name != record.name {""",
     APK),

    # ---- D4: identity mapping, and the assertion that is its entire defence -----------------------
    ("cpu-A15", "A", "the D4 startup assertion cannot refuse anything", CPU_FASTMEM,
     """    let refuse = |setting, expected: u64, actual: u64, consequence| {
        Err(CpuError::MisconfiguredMemoryPath { setting, expected, actual, consequence })
    };""",
     """    let refuse = |_setting, _expected: u64, _actual: u64, _consequence| Ok(());""",
     CPU),

    ("cpu-A16", "A", "the fastmem width is no longer required to be 64", CPU_FASTMEM,
     """    if observed.address_bits != 64 {""",
     """    if false && observed.address_bits != 64 {""",
     CPU),

    ("cpu-A17", "A", "a wild guest address may be mirrored into range again", CPU_FASTMEM,
     """    if observed.mirrors_out_of_range {""",
     """    if false && observed.mirrors_out_of_range {""",
     CPU),

    # ---- the run loop: the watchdog and the signed-comparison footgun -----------------------------
    ("cpu-A18", "A", "a budget reaches the backend unclamped, so u64::MAX reads as negative",
     CPU_RUN,
     """    if wanted == 0 {
        1
    } else if wanted > MAX_SLICE_INSTRUCTIONS {
        MAX_SLICE_INSTRUCTIONS
    } else {
        wanted
    }""",
     """    wanted""",
     CPU),

    ("cpu-A19", "A", "a zero budget becomes run-forever instead of one instruction", CPU_RUN,
     """    if wanted == 0 {
        1
    } else if""",
     """    if wanted == 0 {
        0
    } else if""",
     CPU),

    # ---- D13: the bionic thread pointer ----------------------------------------------------------
    ("cpu-A20", "A", "the stack guard is never written into slot 5", CPU_TLS,
     """            ptr.add(TLS_SLOT_STACK_GUARD_OFFSET)
                .cast::<u64>()
                .write_unaligned(self.guard);""",
     """            let _ = TLS_SLOT_STACK_GUARD_OFFSET;""",
     CPU),

    ("cpu-A21", "A", "a recycled TLS block keeps the previous thread's contents", CPU_TLS,
     """            core::ptr::write_bytes(ptr, 0, self.block_bytes);""",
     """            if false { core::ptr::write_bytes(ptr, 0, self.block_bytes); }""",
     CPU),

    ("cpu-A22", "A", "the stack guard may be zero, which equals a zeroed stack slot", CPU_TLS,
     """        let value = hasher.finish();
        if value != 0 {
            return value;
        }""",
     """        let value = hasher.finish();
        if value != 0 {
            return value & 0;
        }""",
     CPU),

    # ---- the backend's own bookkeeping -----------------------------------------------------------
    ("cpu-A23", "A", "a processor id is never recycled, so threads exhaust the monitor", CPU_DYN,
     """        self.shared.release_processor_id(self.processor_id);""",
     """        let _ = self.processor_id;""",
     CPU),

    ("cpu-A24", "A", "a guest access is served without checking the region's protection", CPU_DYN,
     """        if !allowed {""",
     """        if false && !allowed {""",
     CPU),

    # ---- direction B: over-corrections that read as more careful ----------------------------------
    ("cpu-B2", "B", "TLS blocks committed eagerly so no guest thread ever faults", CPU_TLS,
     """                CommitPolicy::Lazy,""",
     """                CommitPolicy::Eager,""",
     CPU),

    ("cpu-B3", "B", "block linking turned off as well, for a second escape D16 prices at 7x",
     CPU_DYN,
     """        if self.interruptible {
            optimization::INTERRUPTIBLE
        } else {""",
     """        if self.interruptible {
            optimization::INTERRUPTIBLE & !optimization::BLOCK_LINKING
        } else {""",
     CPU),

    ("cpu-B4", "B", "code invalidation refuses a range outside the guest address space", CPU_DYN,
     """    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        self.with_ctx(|ctx| ctx.executable_cache = None);""",
     """    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        if !self.shared.extent.contains(range.start()) {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "invalidate code outside the guest address space",
                reason: "over-correction: the trait says such a range is not an error",
            });
        }
        self.with_ctx(|ctx| ctx.executable_cache = None);""",
     CPU),

    # ---- I3: the guest's architectural counter ---------------------------------------------------
    # There is deliberately no row for leaving `cntfrq_el0` at 0 rather than programming
    # `clock::CNTFRQ_HZ`. dynarmic's default for 0 is the same 600 MHz, so the mutation is a no-op on
    # this pin and would MISS -- and a row that cannot fail is worse than no row. What the explicit
    # programming buys is that the frequency and the scale are one constant; if either moves, the
    # guest-side `the_guest_reads_the_frequency_the_backend_advertises` fails on the exact value.
    ("cpu-A27", "A", "CNTPCT_EL0 goes back to the per-slice instruction counter", CPU_CALLBACKS,
     """    unsafe { with(ctx, 0, |_| crate::clock::cntpct()) }""",
     """    unsafe { with(ctx, 0, |c| c.ticks_used) }""",
     CPU),

    ("cpu-A34", "A", "the counter returns nanoseconds, so its units are not the advertised CNTFRQ",
     CPU_CLOCK,
     """    let ticks = nanos.saturating_mul(u128::from(CNTFRQ_HZ)) / NANOS_PER_SECOND;""",
     """    let ticks = nanos;""",
     CPU),

    ("cpu-B8", "B", "the counter epoch made per host thread, so two guest threads disagree",
     CPU_CLOCK,
     """    let epoch = *EPOCH.get_or_init(Instant::now);""",
     """    thread_local! {
        static THREAD_EPOCH: Instant = Instant::now();
    }
    let epoch = THREAD_EPOCH.with(|e| *e);""",
     CPU),

    # ---- teardown: the three defects with no symptom where they happen ---------------------------
    ("cpu-A25", "A", "GuestTls no longer frees itself, so a failed construction leaks a block",
     CPU_TLS,
     """    fn drop(&mut self) {
        self.free.lock().push(self.base);
    }""",
     """    fn drop(&mut self) {
        let _ = self.base;
    }""",
     CPU),

    ("cpu-B7", "B", "a block is returned twice, so two live guest threads share one stack guard",
     CPU_TLS,
     """        self.free.lock().push(self.base);""",
     """        self.free.lock().push(self.base);
        self.free.lock().push(self.base);""",
     CPU),

    ("cpu-A26", "A", "the processor id is recycled before the jit that holds its monitor entry",
     CPU_DYN,
     """        unsafe { od_jit_free(self.jit) };
        // Nulled so that `self.jit.is_null()` below *is* the statement "the jit is gone" rather
        // than a comment claiming it, and so a use-after-free of this field would be a null
        // dereference rather than a dangling one.
        self.jit = core::ptr::null_mut();
        // Only now: no jit can reference this processor's monitor entry any more.
        self.shared.release_processor_id(self.processor_id, self.jit.is_null());""",
     """        self.shared.release_processor_id(self.processor_id, self.jit.is_null());
        unsafe { od_jit_free(self.jit) };
        self.jit = core::ptr::null_mut();""",
     CPU),

    # There is no row for "run clears a stale halt bit on entry", because there is no such line to
    # revert. The review's M2 asked for one; the emitted dispatcher already does it
    # (`block_of_code.cpp:403-405` ends every return path with `lock xchg` on `halt_reason`), so an
    # entry clear changed nothing and was removed. What replaced it is a row against the pin itself,
    # in `shim-A*`: if dynarmic ever stopped reading-and-clearing, M2 would become real, and that is
    # the thing worth detecting.

    ("cpu-A33", "A", "the pager refusal loses its line continuation again (M1's defect class)",
     CPU_DYN,
     """                        "{e}. D10 requires Omnidroid to take guest faults ahead of dynarmic's own \\
                         handler; without that every guest fault recompiles its block onto the \\
                         callback path, measured 30-49x slower with correct results""",
     """                        "{e}. D10 requires Omnidroid to take guest faults ahead of dynarmic's own                          handler; without that every guest fault recompiles its block onto the                          callback path, measured 30-49x slower with correct results""",
     CPU),

    # ---- the demand pager ------------------------------------------------------------------------
    ("mem-A15", "A", "the pager commits a page the guest may not write", PAGER,
     """        FaultAccess::Write => protection.is_writable(),""",
     """        FaultAccess::Write => protection.is_readable(),""",
     MEM),

    ("mem-A16", "A", "the pager claims faults from outside its own address space", PAGER,
     """    if fault.address < inner.base || fault.address >= inner.end {
        return FaultOutcome::NotOurs;
    }""",
     """    if false {
        return FaultOutcome::NotOurs;
    }""",
     MEM_AND_CPU),

    ("mem-A17", "A", "a zero-byte commit is declined again, so a concurrent fault leaves fastmem",
     PAGER,
     """    let anonymous = matches!(region.kind, RegionKind::Anonymous);
    let repeated = LAST_ZERO_COMMIT.with(|cell| cell.replace(fault.address)) == fault.address;
    if anonymous && !repeated {""",
     """    let anonymous = matches!(region.kind, RegionKind::Anonymous);
    let repeated = LAST_ZERO_COMMIT.with(|cell| cell.replace(fault.address)) == fault.address;
    if false && anonymous && !repeated {""",
     MEM_AND_CPU),

    ("mem-B6", "B", "the pager commits the whole mapping so a page never faults twice", PAGER,
     """    match inner.space.ensure_committed(fault.address, 1) {""",
     """    match inner.space.ensure_committed(region.start, region.len) {""",
     MEM_AND_CPU),
    # ---- .eh_frame: the function map M2's whole choice of code rests on -------------------------
    # ---- C1: the handler slot is drained, not merely cleared -------------------------------------
    # Two rows on the quiescence protocol and two on what it must NOT cost. There is deliberately no
    # row weakening the `SeqCst` accesses to acquire/release: the pair is the Dekker shape, so the
    # weakening is genuinely wrong, but x86-64 is TSO and the only reordering that exposes it is one
    # the hardware does not perform. A row that cannot fail is worse than no row (Task 1), and the
    # argument lives in the module docs instead. There is likewise no row for taking the in-flight
    # reference *after* the handler load rather than before: the window it opens is between two
    # instructions, and no deterministic test can land in it.
    ("plat-A3", "A", "the drain removed, so release returns with a dispatch still in the handler",
     FAULT,
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);""",
     """    if false && slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);""",
     PLATFORM),

    ("plat-A4", "A", "the in-flight reference dropped before the handler call instead of after",
     FAULT,
     """        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        let outcome = handler(context, fault);
        drop(guard);""",
     """        let handler: FaultHandler = unsafe { core::mem::transmute::<usize, FaultHandler>(handler) };
        drop(guard);
        let outcome = handler(context, fault);""",
     PLATFORM),

    ("plat-B7", "B", "the drain made a whole-table barrier, so one space waits on another's fault",
     FAULT,
     """    if slot.active.load(Ordering::SeqCst) != 0 {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while slot.active.load(Ordering::SeqCst) != 0 {""",
     """    if SLOTS.iter().any(|s| s.active.load(Ordering::SeqCst) != 0) {
        DRAINED.fetch_add(1, Ordering::Relaxed);
        let mut spins: u32 = 0;
        while SLOTS.iter().any(|s| s.active.load(Ordering::SeqCst) != 0) {""",
     PLATFORM),

    ("plat-B8", "B", "slots retired rather than reused, the cheaper C1 fix the review offered",
     FAULT,
     """    slot.handler.store(0, Ordering::SeqCst);""",
     """    slot.handler.store(CLAIMING, Ordering::SeqCst);""",
     PLATFORM),

    ("elf-A20", "A", "the table's datarel base dropped, so every function start is wrong",
     EH_FRAME,
     """        Apply::DataRelative => hdr_vaddr,""",
     """        Apply::DataRelative => 0,""",
     ELF_SCAN),

    ("elf-A21", "A", "pc_range read with the pointer's base applied, so lengths become addresses",
     EH_FRAME,
     """            fde_encoding & 0x0F,
            0,
            "FDE pc_range",""",
     """            fde_encoding,
            0,
            "FDE pc_range",""",
     ELF_SCAN),

    ("elf-A22", "A", "the table's initial_location is no longer checked against the FDE's pc_begin",
     EH_FRAME,
     """            if bounds.start != initial_location {""",
     """            if false && bounds.start != initial_location {""",
     ELF_SCAN),

    ("elf-A23", "A", "fde_count trusted rather than bounded by the bytes present", EH_FRAME,
     """    if entry_bytes == 0 || fde_count > available / entry_bytes {""",
     """    if false {""",
     ELF_SCAN),

    ("elf-A24", "A", "an unimplemented DWARF pointer encoding is read as udata4 instead of refused",
     EH_FRAME,
     """        _ => {
            return Err(ElfError::UnsupportedEhFrameEncoding { what, encoding });
        }""",
     """        _ => Format { bytes: 4, signed: false },""",
     ELF_SCAN),

    ("elf-A25", "A", "a LEB128 with no terminator is walked without a bound", EH_FRAME,
     """        if used >= 10 {""",
     """        if false {""",
     ELF_SCAN),

    # Direction B: over-corrections that read as more careful and destroy the map.
    ("elf-B10", "B", "a zero-length FDE refused again, so one entry rejects all 245,117", EH_FRAME,
     """        if start.checked_add(len).is_none() {""",
     """        if len == 0 || start.checked_add(len).is_none() {""",
     ELF_SCAN),

    # ---- the leaf classifier ---------------------------------------------------------------------
    ("elf-A26", "A", "a BL is no longer recorded, so a function that calls out grades as a leaf",
     LEAF,
     """        if w & 0xFC00_0000 == 0x9400_0000 {
            facts.direct_calls.insert(branch_target(at, imm26(w)));""",
     """        if w & 0xFC00_0000 == 0x9400_0000 {
            let _ = branch_target(at, imm26(w));""",
     ELF_SCAN),

    ("elf-A27", "A", "a call before the last RET is counted as if it were in the failure tail",
     LEAF,
     """            if last_return.is_none_or(|last| i < last) {
                facts.calls_before_last_return += 1;
            }""",
     """            if false {
                facts.calls_before_last_return += 1;
            }""",
     ELF_SCAN),

    ("elf-A28", "A", "a memory base that is neither SP nor a thread pointer is not recorded", LEAF,
     """                if rn != 31 && !thread_pointer_regs[rn as usize] {
                    facts.foreign_memory_bases.insert(rn);
                }""",
     """                if false {
                    facts.foreign_memory_bases.insert(rn);
                }""",
     ELF_SCAN),

    ("elf-A29", "A", "a thread-pointer register stays one after being redefined", LEAF,
     """            0b1000 | 0b1001 | 0b0101 | 0b1101 => {
                thread_pointer_regs[(w & 0x1F) as usize] = false;
            }""",
     """            0b1000 | 0b1001 | 0b0101 | 0b1101 => {}""",
     ELF_SCAN),

    ("elf-A30", "A", "an undecodable word no longer disqualifies a body", LEAF,
     """        if !self.fully_decoded()""",
     """        if false""",
     ELF_SCAN),

    ("elf-A31", "A", "a function map claiming more code than the object holds is scanned anyway",
     LEAF,
     """    if decoded > executable_bytes {""",
     """    if false {""",
     ELF_SCAN),

    ("elf-B11", "B", "the hint space refused again, losing every padded candidate", LEAF,
     """        if w & 0xFFFF_F01F == 0xD503_201F {
            facts.hints += 1;
            continue;
        }""",
     """        if w & 0xFFFF_F01F == 0xD503_201F {
            facts.system_instructions += 1;
            continue;
        }""",
     ELF_SCAN),

    ("elf-B12", "B", "a stack-guard tail past the last RET refused, so only unprotected code runs",
     LEAF,
     """        if !self.direct_calls.is_empty() {""",
     """        if true {""",
     ELF_SCAN),

    # ---- the per-slice callback invariant ---------------------------------------------------------
    ("cpu-A32", "A", "fastmem_exclusive_access lost, so every LDXR leaves the fast path", CPU_DYN,
     """            fastmem_exclusive_access: 1,""",
     """            fastmem_exclusive_access: 0,""",
     CPU),

    ("cpu-A28", "A", "the per-slice callback delta is no longer checked", CPU_DYN,
     """                let delta = self.slow_path_entries().saturating_sub(before);
                if delta != 0 {""",
     """                let delta = self.slow_path_entries().saturating_sub(before);
                if false && delta != 0 {""",
     CPU),

    ("cpu-A29", "A", "the invariant is never armed, so it can only ever pass", CPU_DYN,
     """        let armed = options.assert_callback_free_slices && shared.owns_guest_paging;""",
     """        let armed = false;""",
     CPU),

    ("cpu-A30", "A", "the exemption widened to every exit, so only a budget expiry can violate it",
     CPU_DYN,
     """                        Some(PendingExit::Returned { .. }) => Some("the guest returned"),""",
     """                        Some(PendingExit::Returned { .. }) => None,""",
     CPU),

    ("cpu-A31", "A", "a failed demand-pager install is swallowed again", CPU_DYN,
     """            Err(e) if e.is_unsupported() => None,""",
     """            Err(e) if true || e.is_unsupported() => None,""",
     CPU),

    ("cpu-B6", "B", "the memory-fault exemption removed, so every real guest fault is a violation",
     CPU_DYN,
     """                        Some(PendingExit::Fault { .. }) => None,""",
     """                        Some(PendingExit::Fault { .. }) => Some("a memory fault"),""",
     CPU),

    # There is deliberately no row for dropping the `&& owns_guest_paging` term from the arming
    # condition. Since `DynarmicBackend::new` now *refuses* a platform that has a vectored handler
    # and could not give us one, `owns_guest_paging` is false only where there is no handler
    # implementation at all -- Linux and macOS -- so on this host the term is hard to make differ
    # and a row written today would MISS. A row that cannot fail is worse than no row (Task 1).
    #
    # It is *closable*, though, and saying otherwise would overstate the obstacle: the precedent is
    # `create_misconfigured_thread`, a `test-support`-gated constructor that builds a context the
    # production path refuses precisely so an unreachable check can be shown to fire. A test-only
    # option that declines to install the pager would do the same here. It is not done because the
    # check is a platform guard rather than a defect anyone has hit, and a new bypass of a safety
    # property is not free -- a judgement about priority, not about possibility.
]


def read_exactly(path):
    """Read a file without touching its line endings.

    `open(path)` in text mode is universal-newlines: it turns CRLF into LF on the way in, and on
    Windows turns LF back into CRLF on the way out. So a mutation applied to an LF file used to
    restore it as CRLF -- every line of it reported as changed by `git diff`, and the "always
    restored" promise in this module's docstring quietly untrue. `newline=""` on both halves makes
    the round trip exact, which is the only version of that promise worth making.
    """
    with open(path, "r", encoding="utf-8", newline="") as handle:
        return handle.read()


def write_exactly(path, text):
    with open(path, "w", encoding="utf-8", newline="") as handle:
        handle.write(text)


def as_written(pattern, text):
    """Re-express a table pattern in the line ending the target file actually uses.

    The two halves of the restore fix have to be done together, and doing only the first is a trap I
    walked straight into. Reading with `newline=""` stops the harness rewriting a file's line
    endings -- but it also means a CRLF file now contains CRLF, while every `old`/`new` string in the
    table above is written with LF, because it lives in a Python source file. Six multi-line patterns
    silently stopped matching and were reported as MISS.

    They were reported, though, which is the only reason this was caught: a MISS is never a pass.
    That is worth more than the bug cost.
    """
    crlf = '\r\n' in text
    normalised = pattern.replace('\r\n', '\n')
    return normalised.replace('\n', '\r\n') if crlf else normalised


def run(command):
    started = time.time()
    proc = subprocess.run(command, capture_output=True, text=True, encoding="utf-8",
                          errors="replace")
    out = (proc.stdout or "") + (proc.stderr or "")
    return proc.returncode, out, time.time() - started


def failing_tests(output):
    names = []
    for line in output.splitlines():
        line = line.strip()
        if line.startswith("test ") and line.endswith(" ... FAILED"):
            names.append(line[len("test "):-len(" ... FAILED")])
    return names


def aborted_test(output):
    """The test that was running when a test binary died, from a `--test-threads=1` run.

    Some mutations are caught by a **crash** rather than by an assertion -- removing a bounds check
    that guest code reaches, for instance, turns a checked refusal into a wild dereference. libtest
    never prints a result line for those, so the harness could only say "the suite failed without
    naming a test", which is caught but useless: it does not say *what* noticed, and the whole value
    of this table is the mapping from a fix to the test that pins it.

    With `--test-threads=1` libtest prints `test <name> ... ` **before** running each one and
    completes the line afterwards, so an *orphan* -- a start with no matching completion -- is a test
    that died. Matching by name matters: `--no-fail-fast` means later binaries keep running and
    completing their own tests, and a naive "last dangling line" is cleared by the first of them.
    """
    orphans = []
    pending = None
    for line in output.splitlines():
        stripped = line.strip()
        if not stripped.startswith("test "):
            continue
        body = stripped[len("test "):]
        if body.endswith("..."):
            # A start. Anything still pending never completed.
            if pending is not None:
                orphans.append(pending)
            pending = body[: -len("...")].strip()
        elif " ... " in body:
            name = body.split(" ... ", 1)[0].strip()
            if pending == name:
                pending = None
            elif pending is not None:
                orphans.append(pending)
                pending = None
    if pending is not None:
        orphans.append(pending)
    return orphans[0] if orphans else None


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--only", default=None, help="run mutations whose id starts with this")
    parser.add_argument("--list", action="store_true")
    args = parser.parse_args()

    selected = [m for m in MUTATIONS if args.only is None or m[0].startswith(args.only)]
    if args.list:
        for mid, direction, description, path, _, _, _ in selected:
            print(f"{mid:<9} {direction}  {description}  [{path}]")
        return 0

    # Pre-flight: every selected pattern must match its file exactly once *before* anything is
    # mutated or any `cargo` is run.
    #
    # This exists because the Task 1 report claimed it existed when it did not -- the check was
    # per-row, inside the loop, so a stale pattern surfaced as a MISS forty minutes into a run,
    # mixed in with real results. Per-row checking is still there and still needed (a row can go
    # stale between this pass and its turn); this is the cheap pass that says so in one second.
    stale = []
    for mid, _, _, path, old, _, _ in selected:
        text = read_exactly(path)
        found = text.count(as_written(old, text))
        if found != 1:
            stale.append(f"  {mid}: pattern matches {found} times in {path}")
    if stale:
        print(f"pre-flight failed: {len(stale)} of {len(selected)} patterns do not match "
              f"exactly once. Nothing was mutated and nothing was run.")
        print(chr(10).join(stale))
        return 2
    print(f"pre-flight: {len(selected)}/{len(selected)} patterns match exactly once")

    print(f"{len(selected)} mutations\n")
    results = []
    for mid, direction, description, path, old, new, command in selected:
        original = read_exactly(path)
        old = as_written(old, original)
        new = as_written(new, original)
        if old not in original:
            print(f"{mid:<9} MISS  pattern not found in {path}")
            results.append((mid, direction, description, "MISS", "pattern not found"))
            continue
        if original.count(old) != 1:
            print(f"{mid:<9} MISS  pattern is not unique in {path}")
            results.append((mid, direction, description, "MISS", "pattern not unique"))
            continue
        try:
            write_exactly(path, original.replace(old, new))
            code, output, seconds = run(command)
            caught = failing_tests(output)
            if code == 0:
                status, detail = "NOT CAUGHT", "every test still passed"
            # Parenthesised. Without them this read as
            # `(not caught and "error[" in output) or ("could not compile" in output)`, so any run
            # whose output happened to contain "could not compile" -- including one where a mutation
            # was genuinely caught by a failing test -- was filed as MISS. A harness that
            # misclassifies its own results is worse than no harness.
            elif not caught and ("error[" in output or "could not compile" in output):
                status, detail = "MISS", "did not compile"
            elif caught:
                status = "caught"
                detail = f"{len(caught)} test(s): " + ", ".join(caught[:3])
                if len(caught) > 3:
                    detail += f", +{len(caught) - 3} more"
            else:
                # Caught by a crash. Re-run serially to find out which test was on the stack --
                # see `aborted_test`.
                serial_code, serial_output, serial_seconds = run(
                    list(command) + ["--", "--test-threads=1"]
                )
                seconds += serial_seconds
                named = aborted_test(serial_output) if serial_code != 0 else None
                if named:
                    status = "caught"
                    detail = f"1 test(s), by abort: {named}"
                else:
                    status = "caught"
                    detail = "the suite failed without naming a test, in parallel and serially"
            print(f"{mid:<9} {status:<10} {description} -> {detail} ({seconds:.0f}s)")
            results.append((mid, direction, description, status, detail))
        finally:
            write_exactly(path, original)
            if read_exactly(path) != original:
                # The restore is the one thing this harness must never get wrong: a file left
                # mutated silently poisons every later row and, worse, the repository.
                print(f"{mid:<9} FATAL restoring {path} did not reproduce the original")
                return 2

    print()
    caught = sum(1 for r in results if r[3] == "caught")
    print(f"{caught}/{len(results)} caught")
    for mid, direction, description, status, detail in results:
        if status != "caught":
            print(f"  {status}: {mid} {direction} {description} ({detail})")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
