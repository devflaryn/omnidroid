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

**Do not stage or commit while this is running, and do not run two copies of it.** It mutates files
in the working tree in place, so `git add` during a run can capture a mutation, and the commit then
looks like ordinary work with every test passing — the mutation is restored before the suite next
runs. That happened once, in M3 task 1: `XReg::new`'s bound came back as `>` instead of `>=`, which
admits `X31`, and it was found by reading the commit rather than by running anything. The pre-flight
below catches a *killed* run on the next invocation; it cannot see a concurrent one.

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
ACCESS = "crates/omni-mem/src/access.rs"
BIONIC_ERRNO = "crates/omni-bionic/src/errno.rs"
BIONIC_LAYOUTS = "crates/omni-bionic/src/layouts.rs"
BIONIC_STRING = "crates/omni-bionic/src/string.rs"
BIONIC_WIDE = "crates/omni-bionic/src/wide.rs"
BIONIC_SEM = "crates/omni-bionic/src/sem.rs"
BIONIC_MUTEX = "crates/omni-bionic/src/mutex.rs"
BIONIC_NUMERICS = "crates/omni-bionic/src/numerics.rs"
BIONIC_PRINTF = "crates/omni-bionic/src/printf.rs"
FAULT = "crates/omni-platform/src/fault/windows.rs"
EH_FRAME = "crates/omni-elf/src/eh_frame.rs"
LEAF = "crates/omni-elf/src/leaf.rs"
ABI = "crates/omni-android/src/abi.rs"
VARARGS = "crates/omni-android/src/varargs.rs"
ANDROID_MEM = "crates/omni-android/src/mem.rs"
REGION = "crates/omni-android/src/region.rs"
BOUNDARY = "crates/omni-android/src/boundary.rs"
# The bionic adapter: `omni-bionic`'s functions bound onto the boundary.
ADAPTER_VIEW = "crates/omni-android/src/bionic/view.rs"
ADAPTER_MOD = "crates/omni-android/src/bionic/mod.rs"
ADAPTER_HANDLERS = "crates/omni-android/src/bionic/handlers.rs"
ADAPTER_FORMAT = "crates/omni-android/src/bionic/format.rs"

# Commands, kept narrow so the whole run stays under a few minutes.
MEM = ["cargo", "test", "-p", "omni-mem", "--no-fail-fast"]
# `omni-bionic` has no dependencies at all, so it builds in seconds. The targets are named rather
# than taking the whole package because `tests/stress.rs` runs 8 threads x 12,500 rounds and is
# minutes in a debug build, while asserting nothing these rows touch -- the same reasoning as
# `ANDROID` above. `sem_wakeup` IS named: the waiter-flag row is what it exists for.
BIONIC = [
    "cargo", "test", "-p", "omni-bionic", "--lib",
    "--test", "string_tests", "--test", "wide_tests", "--test", "numerics_tests",
    "--test", "mem_tests", "--test", "sem_wakeup",
    # `printf_tests` is named because the field-width and output caps live there, and they are
    # the only bound in this crate that a guest-chosen value can push against.
    "--test", "printf_tests",
    "--no-fail-fast",
]
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

# The thunk boundary. Scoped to the three fast targets rather than the whole package: the
# `libroblox` target loads 109 MB and applies 568,806 relocations, and it asserts about the
# *loader's* binding rather than about the marshalling these rows mutate, so including it would
# multiply every row's cost by that load for no extra detection.
ANDROID = [
    "cargo", "test", "-p", "omni-android", "--lib", "--test", "roundtrip", "--test", "hostile",
    # The bionic adapter's own target. Added when the adapter was written: every row below that
    # names an `ADAPTER_*` file is detected here and nowhere else, so leaving it out would have
    # turned each of them into a MISS that looked like a missing test rather than a missing
    # target.
    "--test", "bionic",
    "--no-fail-fast",
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
     """        self.shared.release_processor_id(self.processor_id, self.jit.is_null());""",
     """        let _ = self.processor_id;""",
     CPU),

    ("cpu-A24", "A", "a guest access is served without checking the region's protection", ACCESS,
     """    if !permits(region.protection, access) {
        return Err(Refusal::Protection);
    }""",
     """    if false && !permits(region.protection, access) {
        return Err(Refusal::Protection);
    }""",
     MEM_AND_CPU),

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

    ("cpu-A35", "A", "the fetch cache ignores whether the region is committed (M5)",
     CPU_CALLBACKS,
     """            .is_some_and(|(start, end, committed)| {
                committed && address >= start && address + 4 <= end
            });""",
     """            .is_some_and(|(start, end, committed)| {
                let _ = committed;
                address >= start && address + 4 <= end
            });""",
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
    # ---- the shared access policy (M4) -----------------------------------------------------------
    # These four rows are the ones the review asked for: before the policy was unified, no row could
    # flip a rule on one side and check that the other caught it, because there were two rules. Now
    # there is one, and each of these runs BOTH suites, so a row that only one crate notices is
    # visible as such in the "N test(s)" column.
    ("mem-A15", "A", "the shared policy commits a page the guest may not write", ACCESS,
     """        FaultAccess::Write => protection.is_writable(),""",
     """        FaultAccess::Write => protection.is_readable(),""",
     MEM_AND_CPU),

    ("mem-A18", "A", "the length check dropped, so an access may run off the end of its region",
     ACCESS,
     """    if access_end > region.end() {
        return Err(Refusal::NotMapped);
    }
    if !permits(region.protection, access) {""",
     """    if false && access_end > region.end() {
        return Err(Refusal::NotMapped);
    }
    if !permits(region.protection, access) {""",
     MEM_AND_CPU),

    ("mem-A19", "A", "free address space is admitted, so a wild guest address resolves", ACCESS,
     """) -> Result<(), Refusal> {
    if region.is_free() {""",
     """) -> Result<(), Refusal> {
    if false && region.is_free() {""",
     MEM_AND_CPU),

    # There is no row for dropping the `anonymous &&` from rule 4, and the reason is a finding
    # rather than an omission: it is **inert**. `Inner::commit_range` skips any entry whose OS state
    # is not a placeholder, and a file-backed view never is, so committing "for any kind" commits
    # nothing extra and returns the same 0. The check stays as an early-out and a statement of
    # intent, and it is now written down that the layer below is what enforces it. A row would MISS,
    # and a row that cannot fail is worse than no row.
    ("mem-B7", "B", "rule 4 commits the whole mapping rather than the granule that was touched",
     ACCESS,
     """        match space.ensure_committed(address, len.max(1)) {""",
     """        match space.ensure_committed(region.mapping_start, region.mapping_len) {""",
     MEM_AND_CPU),

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
     """    if anonymous && !exhausted {
        return FaultOutcome::Resolved;
    }""",
     """    if false && anonymous && !exhausted {
        return FaultOutcome::Resolved;
    }""",
     MEM_AND_CPU),

    ("mem-A20", "A", "the retry record goes back to one address, so two in a granule loop", PAGER,
     """    let granule = fault.address - fault.address % inner.granule.max(1);
    let repeated = LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.replace(granule)) == granule;""",
     """    let granule = fault.address;
    let repeated = LAST_ZERO_COMMIT_GRANULE.with(|cell| cell.replace(granule)) == granule;""",
     MEM_AND_CPU),

    ("mem-A21", "A", "the streak bound removed, so a cycle of distinct granules never terminates",
     PAGER,
     """    let exhausted = repeated || streak > MAX_ZERO_COMMIT_STREAK;""",
     """    let exhausted = repeated;""",
     MEM_AND_CPU),

    ("mem-A22", "A", "examined stops counting the outcome, so declined > examined again", PAGER,
     """        self.examined.fetch_add(1, Ordering::Relaxed);
        match outcome {""",
     """        if !matches!(outcome, FaultOutcome::NotOurs) {
            self.examined.fetch_add(1, Ordering::Relaxed);
        }
        match outcome {""",
     MEM_AND_CPU),

    ("mem-A23", "A", "the pager invents an access length it was never told", PAGER,
     """    match crate::access::admit(&inner.space, fault.address, 1, fault.access) {""",
     """    match crate::access::admit(&inner.space, fault.address, usize::MAX, fault.access) {""",
     MEM_AND_CPU),

    ("mem-B8", "B", "the retry bound tightened to one zero-commit per thread for all time", PAGER,
     """const MAX_ZERO_COMMIT_STREAK: u32 = 1024;""",
     """const MAX_ZERO_COMMIT_STREAK: u32 = 1;""",
     MEM_AND_CPU),
    # ---- .eh_frame: the function map M2's whole choice of code rests on -------------------------
    # ---- C1: the slot is drained, and not reclaimable until the drain finishes -------------------
    # Four rows on the two hazards -- calls in flight, and the slot itself -- and two on what the fix
    # must NOT cost.
    #
    # There is deliberately no row weakening the `SeqCst` accesses to acquire/release, and the reason
    # is NOT that the weakening is safe. It is unsound on this host: the pair is `W(active);R(handler)`
    # against `W(handler);R(active)`, the store-buffer shape, and **StoreLoad is precisely the one
    # reordering x86-64's TSO permits** -- `release`'s plain store to `handler` may sit in the store
    # buffer while its plain load of `active` executes, and a dispatch that has already read the live
    # handler is missed. (`dispatch`'s own side happens to be fenced regardless, because a locked
    # read-modify-write is a full barrier on x86, but that is an accident of the target.) The row is
    # absent because the defect is a *race*: reverting it does not make a test fail, it makes a test
    # fail sometimes, and a flaky row attributes a mutation to the wrong detector (Task 1). An earlier
    # version of this comment said the hardware does not perform that reordering, which was wrong in
    # the reassuring direction -- exactly how the next person weakens it with a clean conscience.
    #
    # There is likewise no row for taking the in-flight reference *after* the handler load rather than
    # before: the window it opens is between two instructions, and no deterministic test can land in
    # it.
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

    ("plat-A5", "A", "a draining slot is unpublished with zero, so install can take it mid-drain",
     FAULT,
     """    slot.handler.store(DRAINING, Ordering::SeqCst);""",
     """    slot.handler.store(0, Ordering::SeqCst);""",
     PLATFORM),

    ("plat-A6", "A", "the context is cleared before the drain rather than after it", FAULT,
     """    // 2. No call is still *running* after this loop.""",
     """    slot.context.store(0, Ordering::Relaxed);

    // 2. No call is still *running* after this loop.""",
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
     """    slot.handler.store(0, Ordering::Release);""",
     """    slot.handler.store(DRAINING, Ordering::Release);""",
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

    # ---- the inline thunk boundary (M3 task 1) ---------------------------------------------------
    ("cpu-A36", "A",
     "the dispatcher stops installing the host MXCSR under an inline thunk handler", CPU_DYN,
     """            let switched = guest != host;
            if switched {
                write(host);
            }""",
     """            let switched = guest != host;""",
     CPU),

    ("cpu-A37", "A",
     "the guard installs the host MXCSR but never puts the guest's back", CPU_DYN,
     """    impl Drop for Guard {
        fn drop(&mut self) {
            if self.switched {
                write(self.guest);
            }
        }
    }""",
     """    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.guest;
        }
    }""",
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

    # ---- AAPCS64 marshalling (M3 task 2) ---------------------------------------------------------
    # Every row here is a way to be *silently* wrong: each produces a plausible number rather than an
    # error, which is the failure shape 3,594 initializers hide (Global Constraint 1).
    ("abi-A1", "A",
     "the integer bank back-fills after spilling to the stack",
     ABI,
     """        let value = if self.ngrn < ARG_REGISTERS {""",
     """        let value = if self.ngrn <= ARG_REGISTERS {""",
     ANDROID),

    ("abi-A2", "A",
     "the two argument banks share one counter, so a double lands in an X register",
     ABI,
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn);
            self.nsrn += 1;""",
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = u128::from(self.call.x(self.nsrn));
            self.nsrn += 1;""",
     ANDROID),

    ("abi-A3", "A",
     "an int return zero-extended, so every libc -1 reads as success",
     ABI,
     """        self.call.set_x(0, i64::from(value) as u64);""",
     """        self.call.set_x(0, u64::from(value as u32));""",
     ANDROID),

    ("abi-A4", "A",
     "a float return written as a double's bit pattern",
     ABI,
     """    pub fn f32(&mut self, value: f32) {
        self.call.set_v(0, u128::from(value.to_bits()));
    }""",
     """    pub fn f32(&mut self, value: f32) {
        self.call.set_v(0, u128::from(f64::from(value).to_bits()));
    }""",
     ANDROID),

    ("abi-A5", "A",
     "a stack argument read without checking that the guest's stack is there",
     ABI,
     # STALE PATTERN REPAIRED. F1's fix made `align_nsaa` fallible, so the `;` in the original
     # pattern stopped matching the source and this row had been silently reporting a MISS for a
     # reason that had nothing to do with the test it names. Found by a pattern check over the
     # whole table; the row's intent is unchanged.
     """            let at = self.align_nsaa(8)?;
            let value = self.mem.read_u64(at, self.blame())?;""",
     """            let at = self.align_nsaa(8)?;
            let value = self.mem.read_u64(at, self.blame()).unwrap_or(0);""",
     ANDROID),

    # The over-correction: a boundary that refused a zero-length access would refuse
    # `memcpy(dst, src, 0)`, which is legal C and which the engine emits.
    ("abi-B1", "B",
     "a zero-length guest access refused instead of being a no-op",
     ANDROID_MEM,
     """        if len == 0 {""",
     """        if false && len == 0 {""",
     ANDROID),

    # ---- the variadic rules, which are not the fixed rules ---------------------------------------
    ("varargs-A1", "A",
     "the SIMD save area stepped by 8 bytes instead of 16",
     VARARGS,
     """pub const VR_SLOT: usize = 16;""",
     """pub const VR_SLOT: usize = 8;""",
     ANDROID),

    ("varargs-A2", "A",
     "variadic floating point read from the integer registers, which is Windows-on-ARM64's rule",
     VARARGS,
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.v(self.nsrn) as u64;
            self.nsrn += 1;""",
     """        let bits = if self.nsrn < ARG_REGISTERS {
            let value = self.call.x(self.nsrn);
            self.nsrn += 1;""",
     ANDROID),

    ("varargs-A3", "A",
     "the guest va_list's offsets no longer range-checked",
     VARARGS,
     """        if value < low || value > high {""",
     """        if false && (value < low || value > high) {""",
     ANDROID),

    ("varargs-A4", "A",
     "a save-area pointer plus a negative offset allowed to wrap into the top of the address space",
     VARARGS,
     """        let sum = i128::from(top as u64) + i128::from(offs);""",
     """        let sum = i128::from((top as u64).wrapping_add(offs as u64));""",
     ANDROID),

    # The over-correction: a positive offset is legal and means "the registers are spent", so
    # refusing one refuses a correct guest.
    ("varargs-B1", "B",
     "a positive va_list offset refused instead of normalised",
     VARARGS,
     """        let high = save_bytes as i64;""",
     """        let high = -1;""",
     ANDROID),

    # ---- guest memory, which is hostile by assumption --------------------------------------------
    ("android-mem-A1", "A",
     "a guest string walk no longer bounded by its region's end",
     ANDROID_MEM,
     """        let reach = region_end.saturating_sub(address).min(Self::STRING_LIMIT);""",
     """        let reach = Self::STRING_LIMIT;""",
     ANDROID),

    ("android-mem-A2", "A",
     "guest memory read without admitting the range at all",
     ANDROID_MEM,
     """        self.check(address, len, FaultAccess::Read, blame)?;
        let mut out = vec![0u8; len];""",
     """        let mut out = vec![0u8; len];""",
     ANDROID),

    # ---- the thunk region ------------------------------------------------------------------------
    ("region-A1", "A",
     "the function area made executable, so a mid-slot branch runs whatever is there",
     REGION,
     """            // Not executable, and lazily committed. See the module docs: this is what turns a branch
            // into the middle of a slot into a typed fault instead of four bytes of something.
            Protection::Read,""",
     """            Protection::ReadExecute,""",
     ANDROID),

    # The over-correction, against Global Constraint 6: the function area is never read on the path
    # that works, so committing it up front is commit charge paid for nothing.
    ("region-B1", "B",
     "the function area committed eagerly instead of lazily",
     REGION,
     """            Protection::Read,
            CommitPolicy::Lazy,""",
     """            Protection::Read,
            CommitPolicy::Eager,""",
     ANDROID),

    # ---- the boundary itself ---------------------------------------------------------------------
    ("boundary-A1", "A",
     "an unbound symbol returns quietly instead of naming itself, which is Constraint 1's shape",
     BOUNDARY,
     """            Binding::Unbound => Err(AbiError::Unbound {
                symbol: slot.symbol.clone(),
                address: slot.address,
            }),""",
     """            Binding::Unbound => Ok(resume),""",
     ANDROID),

    ("boundary-A2", "A",
     "the host-to-guest recursion depth no longer capped",
     BOUNDARY,
     """        if depth > MAX_GUEST_DEPTH {""",
     """        if false && depth > MAX_GUEST_DEPTH {""",
     ANDROID),

    ("boundary-A3", "A",
     "a callback restores only the caller-saved registers, leaving X19-X28 clobbered",
     BOUNDARY,
     """        for (index, &value) in self.x.iter().enumerate() {
            cpu.set_x(XReg::new(index as u8).expect("X0-X30 exist"), value);
        }""",
     """        for (index, &value) in self.x.iter().enumerate().take(19) {
            cpu.set_x(XReg::new(index as u8).expect("X0-X30 exist"), value);
        }""",
     ANDROID),

    ("boundary-A4", "A",
     "SP not restored after a call into guest code",
     BOUNDARY,
     """        cpu.set_sp(self.sp);
        cpu.set_pc(self.pc);""",
     """        cpu.set_pc(self.pc);""",
     ANDROID),

    ("boundary-A5", "A",
     "a failing inline handler records its error and lets the guest carry on anyway",
     BOUNDARY,
     """            record_pending(error);
            import.call.defer_to_caller();""",
     """            record_pending(error);""",
     ANDROID),

    ("boundary-A6", "A",
     "the exit path stops reading a deferred error, so it reports the symbol as unbound",
     BOUNDARY,
     """            if let Some(error) = take_pending() {
                return Err(error);
            }""",
     """            if let Some(error) = take_pending() {
                let _ = error;
            }""",
     ANDROID),

    ("boundary-A7", "A",
     "a callback entered on a stack pointer AArch64 forbids",
     BOUNDARY,
     """        if sp % 16 != 0 {""",
     """        if false && sp % 16 != 0 {""",
     ANDROID),

    ("boundary-A8", "A",
     "the exit-path crossing cap removed",
     BOUNDARY,
     """            if crossings >= self.exit_crossings {""",
     """            if false && crossings >= self.exit_crossings {""",
     ANDROID),

    ("boundary-A9", "A",
     "an execute fault inside the region no longer re-described, so a mid-slot branch loses its symbol",
     BOUNDARY,
     """                ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                    if self.region.holds_function(address) || self.region.holds_data(address) =>""",
     """                ExitReason::MemoryFault { address, access: AccessKind::Execute, .. }
                    if false && (self.region.holds_function(address)
                        || self.region.holds_data(address)) =>""",
     ANDROID),

    # The over-correction: the exact-address lookup removed, so every legitimate imported call falls
    # through into the mid-slot machinery and is refused.
    #
    # This replaced a row that was an **equivalent mutant** and correctly reported NOT CAUGHT: turning
    # `if offset != 0` into `if true` changes nothing, because a call to a slot's own address is
    # answered by the exact lookup above and never reaches the guard. The harness was right and the row
    # was wrong, which is the distinction Global Constraint 13 asks for.
    ("boundary-B1", "B",
     "the exact slot lookup removed, so a legitimate call is treated as a branch into a slot",
     BOUNDARY,
     """        if let Some(slot) = self.slots.get(&address) {
            return Ok(slot);
        }""",
     """        if let Some(slot) = self.slots.get(&address) {
            let _ = slot;
        }""",
     ANDROID),

    ("boundary-A10", "A",
     "the caller's budget handed afresh to every crossing instead of spent down",
     BOUNDARY,
     """if let RunLimit::Instructions(allowance) = remaining {""",
     """if let RunLimit::Instructions(allowance) = RunLimit::Unlimited {""",
     ANDROID),

    # The defect the first version of the budget accounting had: charging the allowance after every
    # `cpu.run` and pre-empting on any exit, which turns a `MemoryFault` -- not resumable -- into a
    # `StepLimitReached`, which is.
    ("boundary-A11", "A",
     "the budget pre-empts any exit that lands on its last instruction, not only a crossing",
     BOUNDARY,
     """                other => return Ok(other),""",
     """                other if matches!(remaining, RunLimit::Instructions(n)
                    if n <= cpu.last_run_instructions()) =>
                {
                    return Ok(ExitReason::StepLimitReached { pc: other.pc(), executed: spent })
                }
                other => return Ok(other),""",
     ANDROID),

    # ---- the seam the boundary rests on ----------------------------------------------------------
    ("cpu-A38", "A",
     "a deferred inline thunk resumes the guest anyway instead of exiting",
     CPU_CALLBACKS,
     """                if deferred {""",
     """                if false && deferred {""",
     ANDROID),

    ("cpu-A39", "A",
     "SP missing from the register file a thunk handler sees",
     CPU_DYN,
     """    fn sp(&self) -> GuestAddr {
        // SAFETY: as `x`. `SP` is a field of `JitState` like any other.
        unsafe { od_jit_get_sp(self.jit) as GuestAddr }
    }""",
     """    fn sp(&self) -> GuestAddr {
        0
    }""",
     ANDROID),
    # ---- Task 2 review: the address arithmetic the guest controls (F1) --------------------------
    # Each of these reverts a `checked_add` to the unchecked round-up. In a profile with overflow
    # checks OFF the unchecked form wraps rather than panicking, and the following read then fails
    # with `BadPointer` anyway — so every one of these tests asserts the refused POINTER, not just
    # the variant. A row that only removed the check would otherwise be a MISS in release.
    ("varargs-A5", "A",
     "the va_list __stack round-up is unchecked again, so a top-of-space __stack wraps",
     VARARGS,
     """    fn aligned_stack(&self, align: usize) -> AbiResult<GuestAddr> {
        self.stack
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.stack_out_of_space(self.stack, align))
    }""",
     """    fn aligned_stack(&self, align: usize) -> AbiResult<GuestAddr> {
        Ok((self.stack + align - 1) & !(align - 1))
    }""",
     ANDROID),

    ("varargs-A6", "A",
     "the VarArgs overflow-area round-up is unchecked again",
     VARARGS,
     """    fn aligned_overflow(&self, align: usize) -> AbiResult<GuestAddr> {
        self.overflow
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.overflow_out_of_space(self.overflow, align))
    }""",
     """    fn aligned_overflow(&self, align: usize) -> AbiResult<GuestAddr> {
        Ok((self.overflow + align - 1) & !(align - 1))
    }""",
     ANDROID),

    ("abi-A6", "A",
     "the NSAA round-up is unchecked again, so a top-of-space SP wraps",
     ABI,
     """        self.nsaa
            .checked_add(align - 1)
            .map(|sum| sum & !(align - 1))
            .ok_or_else(|| self.nsaa_out_of_space(self.nsaa, align))""",
     """        Ok((self.nsaa + align - 1) & !(align - 1))""",
     ANDROID),

    # ---- Task 2 review: the va_list bank names itself (F5) --------------------------------------
    # The original defect: `core::ptr::eq(&self.gr_top, &top)` with `top` by value is always false,
    # so every general-bank refusal was reported as `__vr_top` with the VR bound. Two rows, because
    # the name and the bound are two separable halves of the same fact.
    ("varargs-A7", "A",
     "every save-area refusal names __vr_top again, whichever bank it came from",
     VARARGS,
     """    fn field(self) -> &'static str {
        match self {
            SaveBank::General => "__gr_top",
            SaveBank::Simd => "__vr_top",
        }
    }""",
     """    fn field(self) -> &'static str {
        "__vr_top"
    }""",
     ANDROID),

    ("varargs-A8", "A",
     "every save-area refusal carries the SIMD bound again, whichever bank it came from",
     VARARGS,
     """    fn save_bytes(self) -> usize {
        match self {
            SaveBank::General => GR_SAVE_BYTES,
            SaveBank::Simd => VR_SAVE_BYTES,
        }
    }""",
     """    fn save_bytes(self) -> usize {
        VR_SAVE_BYTES
    }""",
     ANDROID),

    # ---- Task 2 review: a failed handler is not a step limit (F3) -------------------------------
    # Restores the ordering the late-budget fix left behind: the budget arm above the pending-error
    # check, so a handler that failed on the budget's last instruction is reported as a resumable
    # StepLimitReached and its typed error is dropped by the next run's `let _ = take_pending()`.
    ("boundary-A12", "A",
     "the counted budget is checked before the pending handler error, so a failure is lost",
     BOUNDARY,
     """            if let Some(error) = take_pending() {
                return Err(error);
            }
            // Only now, having decided to go round again, is the allowance spent down. A budget that
            // has run out stops the guest *at the thunk*, unserviced and resumable, which is the
            // honest stop: servicing the call and then refusing to resume would leave the caller
            // unable to say what happened.
            if let RunLimit::Instructions(allowance) = remaining {
                let left = allowance.saturating_sub(cpu.last_run_instructions());
                if left == 0 {
                    return Ok(ExitReason::StepLimitReached { pc: site, executed: spent });
                }
                remaining = RunLimit::Instructions(left);
            }
            crossings += 1;""",
     """            if let RunLimit::Instructions(allowance) = remaining {
                let left = allowance.saturating_sub(cpu.last_run_instructions());
                if left == 0 {
                    return Ok(ExitReason::StepLimitReached { pc: site, executed: spent });
                }
                remaining = RunLimit::Instructions(left);
            }
            crossings += 1;
            if let Some(error) = take_pending() {
                return Err(error);
            }""",
     ANDROID),

    # ---- an access may span entries of one mapping, and only of one mapping --------------------
    # A commit carves the map into entries that are each exactly one OS placeholder, and adjacent
    # committed granules are never coalesced -- so a lazily-committed mapping is a run of entries and
    # an ordinary access can straddle two of them. A1 restores the single-entry check that refused
    # every such access as NotMapped; B1 is the over-correction, letting the walk run out of its
    # mapping into whatever is next.
    ("access-A1", "A",
     "only the first entry is checked, so an access straddling a granule boundary is refused",
     ACCESS,
     """    admits_region(&region, address, access_end.min(covered_end) - address, access)?;""",
     """    admits_region(&region, address, len, access)?;""",
     MEM_AND_CPU),

    ("access-B1", "B",
     "the span walk crosses out of its mapping into whatever is mapped next",
     ACCESS,
     """        if next.start != covered_end || next.mapping.is_none() || next.mapping != region.mapping {""",
     """        if next.start != covered_end || next.is_free() {""",
     MEM_AND_CPU),

    # ---- omni-bionic ----------------------------------------------------------------------------
    # The crate had 126 rows' worth of workspace mutation coverage around it and NONE of its own,
    # across 12,543 lines. These rows target the claims that would be silently wrong rather than
    # loudly broken: the guest ABI's widths, the Linux errno numbering, and the two error-reporting
    # conventions that are opposites of each other.

    # The guest's errno numbers are LINUX numbers. The development host is Windows, whose numbering
    # is different, so a value quietly taken from the host is the classic silent-wrong-answer here.
    ("bionic-A1", "A",
     "ETIMEDOUT becomes Windows' ERROR_SEM_TIMEOUT instead of the Linux value",
     BIONIC_ERRNO,
     """    pub const ETIMEDOUT: i32 = 110;""",
     """    pub const ETIMEDOUT: i32 = 121;""",
     BIONIC),

    ("bionic-A2", "A",
     "EAGAIN renumbered off the kernel's value",
     BIONIC_ERRNO,
     """    pub const EAGAIN: i32 = 11;""",
     """    pub const EAGAIN: i32 = 35;""",
     BIONIC),

    # Guest struct widths. Over-declaring a size is how a write lands past the end of a guest object.
    ("bionic-A3", "A",
     "pthread_mutex_t declared 32 bytes, as if bionic used the glibc-shaped layout",
     BIONIC_LAYOUTS,
     """    pub const PTHREAD_MUTEX_T: u64 = 40;""",
     """    pub const PTHREAD_MUTEX_T: u64 = 32;""",
     BIONIC),

    ("bionic-A4", "A",
     "timespec declared 8 bytes, as if time_t were 32-bit",
     BIONIC_LAYOUTS,
     """    pub const TIMESPEC: u64 = 16;""",
     """    pub const TIMESPEC: u64 = 8;""",
     BIONIC),

    ("bionic-A5", "A",
     "pthread_t declared 32-bit, as if a host thread id could carry it",
     BIONIC_LAYOUTS,
     """    pub const PTHREAD_T: u64 = 8;""",
     """    pub const PTHREAD_T: u64 = 4;""",
     BIONIC),

    # wchar_t is 32-bit on Android, not the 16 bits a Windows-shaped assumption would give it.
    ("bionic-A6", "A",
     "the wide-string walk steps by 2, as if wchar_t were 16-bit",
     BIONIC_WIDE,
     """        count += 1;
        cursor = cursor.checked_add(4).ok_or(Fault(cursor))?;""",
     """        count += 1;
        cursor = cursor.checked_add(2).ok_or(Fault(cursor))?;""",
     BIONIC),

    # The FORTIFY check is an off-by-one away from accepting a string with no room for its NUL.
    ("bionic-A7", "A",
     "__strlen_chk accepts a string exactly filling its object, leaving no room for the NUL",
     BIONIC_STRING,
     """    if len >= size {
        return Err(crate::error::BionicError::CheckFailed("__strlen_chk"));""",
     """    if len > size {
        return Err(crate::error::BionicError::CheckFailed("__strlen_chk"));""",
     BIONIC),

    # The sem waiter-flag protocol. A1 restores the defect that stalled a blocked waiter for a full
    # second; the suite could not see it because every sem_wait loops on a bounded slice.
    ("bionic-A8", "A",
     "sem_post consumes the waiter flag other waiters still need",
     BIONIC_SEM,
     """        let next = word + 1;""",
     """        let next = (word & !sem_bits::WAITERS) + 1;""",
     BIONIC),

    # strtol's overflow reporting: the value clamps AND errno is set. Dropping either is silent.
    ("bionic-A9", "A",
     "strtol overflow clamps but does not report ERANGE",
     BIONIC_NUMERICS,
     """    if overflow {
        ctx.set_errno(crate::errno::consts::ERANGE);
        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     """    if overflow {
        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     BIONIC),

    ("bionic-A10", "A",
     "strtol overflow clamps to LONG_MAX regardless of sign",
     BIONIC_NUMERICS,
     """        return Ok(Ok(if negative { i64::MIN } else { i64::MAX }));""",
     """        return Ok(Ok(i64::MAX));""",
     BIONIC),

    # The over-correction: bionic's compare functions return the BYTE DIFFERENCE, not glibc's plus or
    # minus one. Only the sign is specified by C, so this reads as a harmless normalisation -- which
    # is exactly why it needs a row.
    ("bionic-B1", "B",
     "strcmp normalised to glibc's plus-or-minus one instead of bionic's byte difference",
     BIONIC_STRING,
     """        // Bionic's strcmp returns the byte difference (c - d), not glibc's ±1. The C
        // standard only fixes the SIGN; bionic fixes the magnitude. We match bionic.
        Some((ca, cb)) => Ok(ca as i32 - cb as i32),""",
     """        Some((ca, cb)) => Ok(if ca > cb { 1 } else { -1 }),""",
     BIONIC),
    # ---- the printf engine's two bounds on guest-chosen sizes ----------------------------------
    # A width is guest-controlled and `emit_padded` pads with `repeat_n`, so an unbounded one is an
    # allocation the guest picked. These four rows are the pair of bounds in both directions.
    ("printf-A1", "A", "the per-conversion field-width cap removed", BIONIC_PRINTF,
     """                if n > MAX_FIELD_WIDTH {""",
     """                if false && n > MAX_FIELD_WIDTH {""",
     BIONIC),

    ("printf-B1", "B", "the field-width cap tightened below a width real code uses", BIONIC_PRINTF,
     """pub const MAX_FIELD_WIDTH: usize = 64 * 1024;""",
     """pub const MAX_FIELD_WIDTH: usize = 64;""",
     BIONIC),

    ("printf-A2", "A", "the total-output cap removed, so a repeated wide field is unbounded",
     BIONIC_PRINTF,
     """        if out.len() - start_len > MAX_OUTPUT {""",
     """        if false && out.len() - start_len > MAX_OUTPUT {""",
     BIONIC),

    ("printf-B2", "B", "the total-output cap lowered below an ordinary formatted result",
     BIONIC_PRINTF,
     """pub const MAX_OUTPUT: usize = 1024 * 1024;""",
     """pub const MAX_OUTPUT: usize = 8;""",
     BIONIC),

    # ---- F6: the long double refusal, which must fire and must not over-fire -------------------
    ("printf-A3", "A", "the %Lf refusal removed, so a 128-bit quad is read as a double",
     BIONIC_PRINTF,
     """            if length == "L" {""",
     """            if false && length == "L" {""",
     BIONIC),

    ("printf-B3", "B", "the %Lf refusal widened to `l`, refusing the legal %lf", BIONIC_PRINTF,
     """            if length == "L" {""",
     """            if length == "L" || length == "l" {""",
     BIONIC),

    # ---- the one parser: plan and format must agree argument for argument ----------------------
    # `format` fetches a `*` width before it looks at the conversion character, `%%` included. A
    # planner that skipped it diverges by one argument and every later conversion prints the NEXT
    # argument -- a plausible wrong answer, not an error.
    ("printf-A4", "A", "plan skips a `*` width on %%, diverging from format by one argument",
     BIONIC_PRINTF,
     """        if spec.width == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.precision == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.conv == '%' {
            continue;
        }""",
     """        if spec.conv == '%' {
            continue;
        }
        if spec.width == Count::Star {
            kinds.push(ArgKind::Int);
        }
        if spec.precision == Count::Star {
            kinds.push(ArgKind::Int);
        }""",
     BIONIC),

    # ---- the adapter: the guest's atomics ------------------------------------------------------
    # A 32-bit atomic on an unaligned address is undefined behaviour in Rust, and AArch64's
    # LDXR/STXR fault there too -- so the refusal is what the guest would see on a real device.
    # Note what A1 does: with the check gone the mutated build performs an unaligned atomic, which
    # is exactly the undefined behaviour the check exists to prevent. It is detected by the
    # refusal test, not by the access misbehaving.
    ("adapter-A1", "A", "the compare-and-swap alignment refusal removed", ADAPTER_VIEW,
     """        if at % 4 != 0 {""",
     """        if false && at % 4 != 0 {""",
     ANDROID),

    ("adapter-B1", "B", "the CAS alignment tightened to 8, refusing a legal 4-aligned mutex",
     ADAPTER_VIEW,
     """        if at % 4 != 0 {""",
     """        if at % 8 != 0 {""",
     ANDROID),

    # ---- the adapter: the LP64 return traps ----------------------------------------------------
    ("adapter-A2", "A", "an int return zero-extended instead of sign-extended", ADAPTER_HANDLERS,
     """    ($c:ident, i32, $v:expr) => {
        $c.ret().i32($v)
    };""",
     """    ($c:ident, i32, $v:expr) => {
        $c.ret().u64($v as u32 as u64)
    };""",
     ANDROID),

    # The over-correction, and it is the one that reads as correct: the C standard fixes only the
    # SIGN of a comparison, so clamping to -1/0/1 looks defensible. bionic fixes the magnitude and
    # guest code can see it.
    ("adapter-B2", "B", "the compare result normalised to its sign, losing bionic's magnitude",
     ADAPTER_HANDLERS,
     """    fn strcmp(a: ptr, b: ptr) -> i32 = |v| omni_bionic::string::strcmp(&v, a, b);""",
     """    fn strcmp(a: ptr, b: ptr) -> i32 =
        |v| omni_bionic::string::strcmp(&v, a, b).map(|d| d.signum());""",
     ANDROID),

    # ---- the adapter: per-thread state ---------------------------------------------------------
    ("adapter-A3", "A", "__errno points at the scratch buffer instead of the errno cell",
     ADAPTER_VIEW,
     """    pub fn errno_address(&self) -> GuestAddr {
        self.active.block + ERRNO_OFFSET
    }""",
     """    pub fn errno_address(&self) -> GuestAddr {
        self.active.block + SCRATCH_OFFSET
    }""",
     ANDROID),

    ("adapter-B3", "B", "the thread arena tightened below a thread count the runtime uses",
     ADAPTER_MOD,
     """pub const MAX_GUEST_THREADS: usize = 64;""",
     """pub const MAX_GUEST_THREADS: usize = 4;""",
     ANDROID),

    # ---- the adapter: the printf family --------------------------------------------------------
    # snprintf returns what WOULD have been written. A handler returning the truncated length makes
    # every caller that grows its buffer on overflow loop forever, and every short result is
    # identical either way -- so the defect is invisible until a result is truncated.
    ("adapter-A4", "A", "snprintf returns the truncated length instead of the full one",
     ADAPTER_FORMAT,
     """    view.mem().write_bytes(at, &write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(full)""",
     """    view.mem().write_bytes(at, &write, Blame::new(view.symbol(), view.address(), argument))?;
    Ok(i32::try_from(room).unwrap_or(full))""",
     ANDROID),

    ("adapter-A5", "A", "a variadic int taken as 64 bits instead of narrowed to its own width",
     ADAPTER_FORMAT,
     """            ArgKind::Int => Owned::Int(source.next_u64()? as u32 as i32 as i64),""",
     """            ArgKind::Int => Owned::Int(source.next_u64()? as i64),""",
     ANDROID),

    ("adapter-A6", "A", "a null %s argument dereferenced instead of printing (null)",
     ADAPTER_FORMAT,
     """                let pointer = source.next_u64()?;
                if pointer == 0 {
                    Owned::NullStr
                } else {
                    Owned::Str(read_latin1(view.blaming(argument), pointer, argument)?)
                }""",
     """                let pointer = source.next_u64()?;
                Owned::Str(read_latin1(view.blaming(argument), pointer, argument)?)""",
     ANDROID),

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
