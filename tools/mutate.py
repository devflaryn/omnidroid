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
BIONIC_RWLOCK = "crates/omni-bionic/src/rwlock.rs"
BIONIC_COND = "crates/omni-bionic/src/cond.rs"
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
ADAPTER_DATA = "crates/omni-android/src/bionic/data.rs"
ADAPTER_DL = "crates/omni-android/src/bionic/dl.rs"
ADAPTER_GUESTMEM = "crates/omni-android/src/bionic/guestmem.rs"
# M3 task 3 phase 3a: the OS surface. `omni-platform` grows past `vm` and `fault`, and the adapter
# grows the twenty-three guest symbols over it.
PLAT_CLOCK = "crates/omni-platform/src/clock.rs"
PLAT_LOG = "crates/omni-platform/src/log.rs"
PLAT_PROCESS = "crates/omni-platform/src/process/windows.rs"
PLAT_PROCESS_MOD = "crates/omni-platform/src/process/mod.rs"
BIONIC_TIME = "crates/omni-bionic/src/time.rs"
ADAPTER_CLOCKS = "crates/omni-android/src/bionic/clocks.rs"
ADAPTER_PROCENV = "crates/omni-android/src/bionic/procenv.rs"
ADAPTER_LOGGING = "crates/omni-android/src/bionic/logging.rs"
# M3 task 3 phase 3b: files and directories. `omni-platform` gains a ROOTED filesystem, the
# `FILE *` layer lands in `omni-bionic` over a trait, and the adapter binds the 29 file-io symbols.
PLAT_FS = "crates/omni-platform/src/fs/mod.rs"
PLAT_FS_PATH = "crates/omni-platform/src/fs/path.rs"
# M5: the pipe. An in-process byte queue with two ends, and the first descriptor kind whose
# readiness depends on another descriptor.
PLAT_FS_PIPE = "crates/omni-platform/src/fs/pipe.rs"
# M5: the NDK surface. `ALooper` is the first of the four families.
NDK_LOOPER = "crates/omni-android/src/ndk/looper.rs"
PLAT_FS_WINDOWS = "crates/omni-platform/src/fs/windows.rs"
BIONIC_STDIO = "crates/omni-bionic/src/stdio.rs"
ADAPTER_FILES = "crates/omni-android/src/bionic/files.rs"
ADAPTER_STDIO = "crates/omni-android/src/bionic/stdio.rs"

# Phase 3d/3e: the network group and the six nothing else claimed. `omni-platform` gained one
# primitive for this phase (process CPU time) and **no socket seam at all** -- `poll` and `select`
# answer over the descriptor table `fs` already had, which is why there is nothing to mutate on
# the platform side of them.
BIONIC_NET = "crates/omni-bionic/src/net.rs"
ADAPTER_NET = "crates/omni-android/src/bionic/net.rs"

# Phase 3c: threads and signals.
BIONIC_SIGNAL = "crates/omni-bionic/src/signal.rs"
BIONIC_LAYOUTS = "crates/omni-bionic/src/layouts.rs"
ADAPTER_SIGNALS = "crates/omni-android/src/bionic/signals.rs"
ADAPTER_THREADS = "crates/omni-android/src/bionic/threads.rs"
ADAPTER_RUNTIME = "crates/omni-android/src/bionic/runtime.rs"
# M4: JNI without a JVM. The JNI modules, and the two bionic files M4's gate corrected.
JNI_ENV = "crates/omni-android/src/jni/env.rs"
JNI_REFS = "crates/omni-android/src/jni/refs.rs"
JNI_CLASSES = "crates/omni-android/src/jni/classes.rs"
JNI_VALUES = "crates/omni-android/src/jni/values.rs"
JNI_POOL = "crates/omni-android/src/jni/pool.rs"
JNI_SLOTS = "crates/omni-android/src/jni/slots.rs"


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
    # The NDK surface's own target, added in M5 for exactly the same reason: every `NDK_*` row is
    # detected here and nowhere else.
    "--test", "ndk",
    "--no-fail-fast",
]

# The adapter's **library** targets only, with no guest in sight. One row needs this and says why:
# removing the sleep cap makes the end-to-end test sleep for the `i64::MAX` seconds it asked for,
# which HANGS rather than fails -- the failure mode this module's docstring already records from M3
# task 2. Its detector is the unit test on `clocks::capped`, which is why that predicate is a
# function rather than an inline comparison.
ANDROID_LIB = ["cargo", "test", "-p", "omni-android", "--lib", "--no-fail-fast"]

# M6 groundwork: runtime texture transcoding (`omni-texture`). Zero dependencies and `#![no_std]`,
# so its command builds in about a second.
TEXTURE_ETC1 = "crates/omni-texture/src/etc1.rs"
TEXTURE_LIB = "crates/omni-texture/src/lib.rs"
TEXTURE_FORMAT = "crates/omni-texture/src/format.rs"
# `tests/exhaustive.rs` is deliberately NOT named here. MEASURED: 27.4 s in a debug build against
# under a second for the rest of the crate, and every row below has a named detector in one of the
# three targets that ARE named -- the same reasoning, and the same precedent, as `BIONIC` leaving
# out `tests/stress.rs`. `real_assets` is named because two rows are only caught there.
TEXTURE = [
    "cargo", "test", "-p", "omni-texture", "--lib",
    "--test", "spec_vectors", "--test", "hostile", "--test", "real_assets",
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
     """    let sp = cpu.sp();
    if sp % 16 != 0 {""",
     """    let sp = cpu.sp();
    if false && sp % 16 != 0 {""",
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


    # ---- the rwlock's futex contract ------------------------------------------------------------
    # Both loops used to hand the futex a literal 0 while the word is non-zero by construction. The
    # crate's mock and the adapter both ignore `expected`, so the placeholder was invisible --
    # `mutex` and `once` pass real values, only `rwlock` and `sem` did not. A futex that DOES compare
    # would answer WouldBlock to every waiter and the `continue` would busy spin.
    ("bionic-A11", "A",
     "a blocking reader hands the futex a placeholder instead of the word it read",
     BIONIC_RWLOCK,
     """        match futex.wait(rwlock_addr, state, Some(remaining)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => {
                if deadline.is_none() {
                    continue; // protocol re-check (no lost wake under the policy)
                }""",
     """        match futex.wait(rwlock_addr, 0, Some(remaining)) {
            WaitResult::Woken => continue,
            WaitResult::TimedOut => {
                if deadline.is_none() {
                    continue; // protocol re-check (no lost wake under the policy)
                }""",
     BIONIC),

    # The over-correction is the ORIGINAL value: a net so wide it is itself the stall.
    ("bionic-B2", "B",
     "the self-heal slice widened back to a second, so a lost wake is a one-second stall",
     BIONIC_RWLOCK,
     """const SELF_HEAL_SLICE: Duration = Duration::from_millis(50);""",
     """const SELF_HEAL_SLICE: Duration = Duration::from_millis(1_000);""",
     BIONIC),

    # ---- M3 task 3 phase 2: the dl* family ------------------------------------------------------
    #
    # `dl_iterate_phdr` is the one import in this phase that cannot be a stub: the C++ runtime in
    # `libroblox.so` is statically linked, so the in-guest unwinder walks 11.5 MB of `.eh_frame`
    # through it and every `throw` depends on the answer.
    ("dl-A1", "A",
     "dl_iterate_phdr reports an empty process instead of refusing when nothing is registered",
     ADAPTER_DL,
     """    if images.is_empty() {""",
     """    if false && images.is_empty() {""",
     ANDROID),

    # The struct layout, which is the silently-wrong class here: a callback reading `dlpi_phdr` out
    # of where `dlpi_name` was written gets a pointer that is not a program header table.
    ("dl-A2", "A", "dl_phdr_info's dlpi_phdr written at +8, on top of dlpi_name", ADAPTER_DL,
     """    pub(super) const PHDR: usize = 16;""",
     """    pub(super) const PHDR: usize = 8;""",
     ANDROID),

    ("dl-A3", "A", "the walk does not stop when a callback answers non-zero", ADAPTER_DL,
     """        if returned != 0 {
            result = returned;
            break;
        }""",
     """        if returned != 0 {
            result = returned;
        }""",
     ANDROID),

    ("dl-A4", "A", "a null dl_iterate_phdr callback is called instead of refused", ADAPTER_DL,
     """    let target = usize::try_from(callback).ok().filter(|&t| t != 0).ok_or_else(|| {""",
     """    let target = usize::try_from(callback).ok().ok_or_else(|| {""",
     ANDROID),

    # The over-correction: a walk that always stops after one object. The unwinder would then never
    # see any library but the first, which is a *success* returning the first callback's answer.
    ("dl-B1", "B", "the walk stops after the first object whatever the callback answered",
     ADAPTER_DL,
     """        if returned != 0 {
            result = returned;
            break;
        }""",
     """        {
            result = returned;
            break;
        }""",
     ANDROID),

    # ---- M3 task 3 phase 2: the eighteen data objects -------------------------------------------
    # RE-TARGETED in phase 3b: the loop this mutated gained a `register_stream` call, so the
    # original pattern stopped matching and the pre-flight refused the whole run. That is the gate
    # working -- a stale row is otherwise a MISS that looks like a missing test.
    ("data-A1", "A", "stdin/stdout/stderr spaced by a pointer instead of by a whole FILE",
     ADAPTER_DATA,
     """        let stream = sf + index * FILE_BYTES;""",
     """        let stream = sf + index * 8;""",
     ANDROID),

    ("data-A2", "A", "in6addr_loopback is 1:: rather than ::1", ADAPTER_DATA,
     """    ones[15] = 1;""",
     """    ones[0] = 1;""",
     ANDROID),

    # A zero canary compares equal to a zeroed stack slot, so a stack overflow that wrote zeroes
    # passes every `__stack_chk_fail` check. `omni-cpu` refuses to generate one; this is the other
    # half of that refusal.
    ("data-A3", "A", "a zero stack canary is stored instead of refused", ADAPTER_DATA,
     """    if process.stack_guard == 0 {""",
     """    if false && process.stack_guard == 0 {""",
     ANDROID),

    ("data-A4", "A", "environ is null rather than pointing at an empty vector", ADAPTER_DATA,
     """    mem.write_u64(environ, empty_environ as u64, blame("environ", environ))?;""",
     """    mem.write_u64(environ, 0, blame("environ", environ))?;""",
     ANDROID),

    ("data-A5", "A", "__sF is one FILE wide, so stdout and stderr are somebody else's object",
     ADAPTER_DATA,
     """    DataObject { symbol: "__sF", len: 3 * FILE_BYTES, align: 8 },""",
     """    DataObject { symbol: "__sF", len: FILE_BYTES, align: 8 },""",
     ANDROID),

    # The over-correction: the static pool tightened until the data the phase actually places no
    # longer fits. A bound that refuses correct input is as wrong as no bound.
    ("data-B1", "B", "the static pool tightened below what the eighteen data objects need",
     ADAPTER_MOD,
     """pub const POOL_BYTES: usize = 4096;""",
     """pub const POOL_BYTES: usize = 64;""",
     ANDROID),

    # ---- M3 task 3 phase 2: the guest-memory group ----------------------------------------------
    #
    # Task 2 review F9: these five reach the whole `GuestSpace`, so they are on the exit path. That
    # is also the only path that can reach a CPU, which is what `invalidate` needs.
    ("guestmem-A1", "A",
     "translated code is not discarded when the memory it came from is unmapped or reprotected",
     ADAPTER_GUESTMEM,
     """    if len == 0 {
        return Ok(());
    }
    c.invalidate_code(address, len)""",
     """    if true || len == 0 {
        return Ok(());
    }
    c.invalidate_code(address, len)""",
     ANDROID),

    # **Re-aimed, not retired.** This row used to inject `MADV_DONTNEED` being *answered*, and
    # that is what it now does by design (D28 amendment 1): the guarantee is met by decommitting
    # rather than by writing zeroes, so the old form no longer describes a defect. What is left of
    # the original property is `MADV_REMOVE`, which punches a hole in an *underlying object* that
    # no mapping here has. `jni-A11` and `jni-B3` cover MADV_DONTNEED's own semantics from both
    # directions.
    ("guestmem-A2", "A", "MADV_REMOVE is answered instead of refused", ADAPTER_GUESTMEM,
     """    if advice == MADV_REMOVE {""",
     """    if false {""",
     ANDROID),

    # Re-anchored for M3's gate: the `fd != -1` half of this condition was a defect in its own
    # right (`guestmem-A10`), and removing it left this row's pattern unmatched. The statement is
    # unchanged -- serving a file-backed request as anonymous memory hands the guest zeroed pages
    # where it asked for a file's contents, and succeeds while doing it.
    ("guestmem-A3", "A", "a file-backed mmap is served as anonymous memory", ADAPTER_GUESTMEM,
     """    if flags & MAP_ANONYMOUS == 0 {""",
     """    if false {""",
     ANDROID),

    # Widening rather than refusing: the guest asked for write-only and is given read as well.
    ("guestmem-A4", "A", "PROT_WRITE alone widened to ReadWrite instead of refused",
     ADAPTER_GUESTMEM,
     """        p if p == PROT_READ | PROT_WRITE => Ok(Protection::ReadWrite),""",
     """        p if p == PROT_READ | PROT_WRITE || p == PROT_WRITE => Ok(Protection::ReadWrite),""",
     ANDROID),

    # Global Constraint 11's "saturating arithmetic on a limit turns hostile input into a larger
    # permission", in the one place in this phase where a guest length is rounded up.
    ("guestmem-A5", "A", "a length rounded up to a page saturates instead of being checked",
     ADAPTER_GUESTMEM,
     """    len.checked_add(mask).map(|n| n & !mask)""",
     """    Some(len.saturating_add(mask) & !mask)""",
     ANDROID),

    ("guestmem-A6", "A", "mlock answers 0, which is the believable wrong answer", ADAPTER_GUESTMEM,
     """    let call = Call::begin(c)?;
    call.refuse(format!(
        "the guest asked to lock {len} bytes at {addr:#x} into memory.""",
     """    let call = Call::begin(c)?;
    c.ret(|mut r| r.i32(0));
    return Ok(());
    #[allow(unreachable_code)]
    call.refuse(format!(
        "the guest asked to lock {len} bytes at {addr:#x} into memory.""",
     ANDROID),

    # The over-correction: refusing a request that is correct here. MAP_SHARED on anonymous memory
    # differs from MAP_PRIVATE only across a `fork`, and there is none.
    ("guestmem-B1", "B", "an anonymous MAP_SHARED refused although there is no fork to share with",
     ADAPTER_GUESTMEM,
     """    if !matches!(flags & MAP_TYPE, MAP_PRIVATE | MAP_SHARED) {""",
     """    if !matches!(flags & MAP_TYPE, MAP_PRIVATE) {""",
     ANDROID),

    # The over-correction that destroys the measured design property: D10's "never commit
    # speculatively", and the demand pager being the heap seam rather than `malloc`.
    ("guestmem-B2", "B", "a guest mmap commits eagerly, so the heap seam stops being the pager",
     ADAPTER_GUESTMEM,
     """    match space.map_anonymous(placement, len, protection, CommitPolicy::Lazy) {""",
     """    match space.map_anonymous(placement, len, protection, CommitPolicy::Eager) {""",
     ANDROID),

    ("guestmem-B3", "B", "the purely advisory madvise hints refused as well", ADAPTER_GUESTMEM,
     """    if ADVISORY.contains(&advice) {""",
     """    if false && ADVISORY.contains(&advice) {""",
     ANDROID),
    # ---- M3 task 3 phase 3a: the OS surface ------------------------------------------------------
    #
    # `omni-platform` grows past `vm` and `fault` for the first time. Two halves, and both are
    # mutated: the seam itself (clock, process, log) and the twenty-three guest symbols over it.

    # The monotonic clock re-anchored per call. Still non-decreasing, still plausible, and every
    # reading is ~0 -- so a guest measuring an interval measures nothing.
    ("seam-A1", "A", "the monotonic clock is re-anchored on every call instead of on one epoch",
     PLAT_CLOCK,
     """    let epoch = *EPOCH.get_or_init(Instant::now);""",
     """    let epoch = Instant::now();""",
     PLATFORM),

    # The worst available value out of an entropy source: a buffer of zeroes, reported as filled.
    ("seam-A2", "A", "random_bytes reports success without asking the OS for anything", PLAT_PROCESS,
     """        let status = unsafe {
            BCryptGenRandom(core::ptr::null_mut(), chunk.as_mut_ptr(), len, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };""",
     """        let _ = (&chunk, len);
        let status = 0;""",
     PLATFORM),

    ("seam-A3", "A", "sleep returns immediately whatever it was asked for", PLAT_CLOCK,
     """    if duration.is_zero() {
        return;
    }""",
     """    if true {
        return;
    }""",
     PLATFORM),

    ("seam-A4", "A", "an out-of-range android log priority is mapped to a neighbour", PLAT_LOG,
     """            8 => Priority::Silent,
            _ => return None,""",
     """            8 => Priority::Silent,
            _ => Priority::Unknown,""",
     PLATFORM),

    # The over-correction: an empty request is a no-op in C and must not become a failure.
    ("seam-B1", "B", "random_bytes fails an empty request instead of treating it as a no-op",
     PLAT_PROCESS_MOD,
     """    if out.is_empty() {
        return Ok(());
    }""",
     """    if out.is_empty() {
        return Err(ProcessError::Status {
            operation: "random_bytes",
            api: "BCryptGenRandom",
            status: -1,
        });
    }""",
     PLATFORM),

    # The over-correction: a severity that maps perfectly well is refused.
    ("seam-B2", "B", "the most severe syslog level stops mapping onto the android scale", PLAT_LOG,
     """            0..=2 => Priority::Fatal,""",
     """            1..=2 => Priority::Fatal,""",
     PLATFORM),

    # ---- the calendar ----------------------------------------------------------------------------
    #
    # Every row here is wrong only for part of the input range, which is what makes the conversion
    # worth mutating at all: a wrong answer for 1969 and a right one for 2026 is exactly the shape
    # that ships.

    ("time-A1", "A", "floor division becomes truncating, so every pre-1970 date is a day late",
     BIONIC_TIME,
     """    if numerator % denominator != 0 && ((numerator < 0) != (denominator < 0)) {
        quotient - 1
    } else {
        quotient
    }""",
     """    quotient""",
     BIONIC),

    ("time-A2", "A", "the Gregorian 400-year leap exception is dropped, so 2000 is not a leap year",
     BIONIC_TIME,
     """    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0""",
     """    year % 4 == 0 && year % 100 != 0""",
     BIONIC),

    ("time-A3", "A", "a year that will not fit int tm_year wraps instead of reporting EOVERFLOW",
     BIONIC_TIME,
     """    let Ok(tm_year) = i32::try_from(tm_year) else {
        return Err(GmtimeError::YearOutOfRange { year });
    };""",
     """    let tm_year = tm_year as i32;""",
     BIONIC),

    ("time-A4", "A", "the weekday uses % instead of rem_euclid, so pre-1970 days are negative",
     BIONIC_TIME,
     """    let wday = (days + UNIX_EPOCH_WEEKDAY).rem_euclid(7);""",
     """    let wday = (days + UNIX_EPOCH_WEEKDAY) % 7;""",
     BIONIC),

    ("time-A5", "A", "tm_yday loses the leap-day adjustment after February", BIONIC_TIME,
     """    let leap_day = i32::from(is_leap(year) && month > 2);""",
     """    let leap_day = 0;""",
     BIONIC),

    ("time-A6", "A", "tm_zone is left null, so guest code prints a const char * that is not there",
     BIONIC_TIME,
     """    bytes[TM_ZONE_OFFSET..TM_ZONE_OFFSET + 8].copy_from_slice(&zone.to_le_bytes());""",
     """    let _ = zone;""",
     BIONIC),

    # The over-correction: refusing input that is entirely legal. A negative time_t is a date
    # before 1970, not an error.
    ("time-B1", "B", "gmtime refuses every pre-1970 timestamp", BIONIC_TIME,
     """    let days = floor_div(timestamp, SECONDS_PER_DAY);""",
     """    if timestamp < 0 {
        return Err(GmtimeError::YearOutOfRange { year: 0 });
    }
    let days = floor_div(timestamp, SECONDS_PER_DAY);""",
     BIONIC),

    # The over-correction: the year bound tightened below what the guest's own `int` allows.
    ("time-B2", "B", "the tm_year bound is narrowed to 16 bits, refusing years an int holds",
     BIONIC_TIME,
     """    let Ok(tm_year) = i32::try_from(tm_year) else {""",
     """    let Ok(tm_year) = i16::try_from(tm_year).map(i32::from) else {""",
     BIONIC),

    # ---- the clock symbols -----------------------------------------------------------------------

    ("clocks-A1", "A", "CLOCK_MONOTONIC is served from the wall clock", ADAPTER_CLOCKS,
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
                omni_platform::clock::monotonic_now()
            }""",
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {
                omni_platform::clock::realtime_now()
            }""",
     ANDROID),

    # CLOCK_BOOTTIME counts time spent suspended and the host's monotonic clock does not, so
    # aliasing it is a wrong answer rather than a coarser right one.
    ("clocks-A2", "A", "CLOCK_BOOTTIME is aliased to the monotonic clock instead of refused",
     ADAPTER_CLOCKS,
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE => {""",
     """            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {""",
     ANDROID),

    ("clocks-A3", "A", "gettimeofday writes nanoseconds into a struct timeval's tv_usec",
     ADAPTER_CLOCKS,
     """            write_pair(&view, tv, seconds, nanos / 1_000, 0)?;""",
     """            write_pair(&view, tv, seconds, nanos, 0)?;""",
     ANDROID),

    ("clocks-A4", "A", "nanosleep accepts a malformed timespec instead of reporting EINVAL",
     ADAPTER_CLOCKS,
     """    if seconds < 0 || !(0..NANOS_PER_SECOND).contains(&nanos) {""",
     """    if false {""",
     ANDROID),

    # **Scoped to the library target on purpose.** Removing the cap makes the end-to-end test sleep
    # for the i64::MAX seconds it asks for, which hangs rather than fails -- the failure mode this
    # harness's own docstring records from M3 task 2. The detector is the unit test on `capped`,
    # which is why that predicate is a function.
    ("clocks-A5", "A", "the sleep cap is not applied, so a guest can block a host thread forever",
     ADAPTER_CLOCKS,
     """    duration.as_secs() > MAX_SLEEP_SECONDS""",
     """    false && duration.as_secs() > MAX_SLEEP_SECONDS""",
     ANDROID_LIB),

    ("clocks-A6", "A", "usleep reads all 64 bits of X0 although useconds_t is 32", ADAPTER_CLOCKS,
     """    let micros = u64::from(c.args().next_u64()? as u32);""",
     """    let micros = c.args().next_u64()?;""",
     ANDROID),

    # The pattern carries `gmtime_r`'s own `Ok` arm because M4 bound `gmtime` beside it and the
    # two share these two lines. Without the context it matched **twice**, and the pre-flight
    # refused the whole run rather than mutating whichever came first -- which is what that gate
    # is for. `clocks-A10` is the same property for the non-reentrant spelling.
    ("clocks-A7", "A", "gmtime_r returns its buffer after failing to fill it", ADAPTER_CLOCKS,
     """                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = guest_address(view.blaming(1), result)?;""",
     """                view.set_errno(EOVERFLOW);
                result
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = guest_address(view.blaming(1), result)?;""",
     ANDROID),

    # **`gmtime` returning its own buffer after failing to fill it**, which is `clocks-A7`'s
    # property for the spelling that owns the storage. A caller that tests the result against NULL
    # -- which is the whole of `gmtime`'s error reporting -- would read a `struct tm` nothing
    # wrote. Bound by M4's gate, so it had no row until now. (Numbered A10: A8 and A9 were both taken, and
    # the harness's duplicate-id gate is what said so before anything ran.)
    ("clocks-A10", "A", "gmtime returns its per-thread struct tm after failing to fill it",
     ADAPTER_CLOCKS,
     """                view.set_errno(EOVERFLOW);
                0u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = view.tm_address();""",
     """                view.set_errno(EOVERFLOW);
                view.tm_address() as u64
            }
            Ok(tm) => {
                let zone = state.bionic.utc_zone();
                let at = view.tm_address();""",
     ANDROID),

    # The over-correction: the cap applied to the sub-second part, so an ordinary 10 ms sleep is
    # refused. A bound that refuses correct input is as wrong as no bound.
    ("clocks-B1", "B", "the sleep cap is applied to the nanoseconds, refusing a 10 ms sleep",
     ADAPTER_CLOCKS,
     """    duration.as_secs() > MAX_SLEEP_SECONDS""",
     """    u64::from(duration.subsec_nanos()) > MAX_SLEEP_SECONDS""",
     ANDROID),

    # ---- process and environment -----------------------------------------------------------------
    #
    # The first row is the one that matters most in this phase: the open AT_HWCAP decision made by
    # defaulting, which is precisely what the policy type exists to prevent.
    ("procenv-A1", "A",
     "the open AT_HWCAP decision is made by defaulting an instance to Decline", ADAPTER_MOD,
     """            hwcap: Mutex::new(HwcapPolicy::Undecided),""",
     """            hwcap: Mutex::new(HwcapPolicy::Decline),""",
     ANDROID),

    # Re-anchored for M3's gate. `sysconf` answers the page size and the processor count now, so
    # the old form of this row -- answering an unverified constant -- is what the code does. What
    # is still worth injecting is `_SC_PHYS_PAGES`: a number IS available for it, and it is the
    # host's physical memory rather than the guest's budget, which is the same wrong answer
    # `sysinfo` refuses for.
    ("procenv-A2", "A", "sysconf answers _SC_PHYS_PAGES with the host's memory", ADAPTER_PROCENV,
     """        other => {
            let believed = believed_sysconf_name(other).map_or_else(""",
     """        other if other == 0x0062 => 1 << 19,
        other => {
            let believed = believed_sysconf_name(other).map_or_else(""",
     ANDROID),

    ("procenv-A3", "A", "prctl answers 0, which every option has available as a believable done",
     ADAPTER_PROCENV,
     """    if option != PR_SET_VMA {
        let named = prctl_option_name(option)""",
     """    if option != PR_SET_VMA {
        c.ret().i32(0);
        return Ok(());
        #[allow(unreachable_code)]
        let named = prctl_option_name(option)""",
     ANDROID),

    ("procenv-A4", "A", "syscall answers -1/ENOSYS, which callers route around silently",
     ADAPTER_PROCENV,
     """    let named = syscall_name(number).map_or_else(String::new, |name| format!(" (arm64 `{name}`)"));""",
     """    c.ret().i32(-1);
    return Ok(());
    #[allow(unreachable_code)]
    let named = syscall_name(number).map_or_else(String::new, |name| format!(" (arm64 `{name}`)"));""",
     ANDROID),

    # Half a buffer of real entropy and a reported failure: the caller cannot tell which half.
    ("procenv-A5", "A",
     "arc4random_buf validates one byte instead of the whole destination", ADAPTER_PROCENV,
     """            view.mem().checked_ptr(at, len, true, blame)?;""",
     """            view.mem().checked_ptr(at, 1, true, blame)?;""",
     ANDROID),

    ("procenv-A6", "A", "__system_property_get reports the length including its NUL",
     ADAPTER_PROCENV,
     """        i32::try_from(text.len()).map_err(|_| {""",
     """        i32::try_from(text.len() + 1).map_err(|_| {""",
     ANDROID),

    ("procenv-A7", "A", "abort returns to the guest instead of becoming a typed outcome",
     ADAPTER_PROCENV,
     """    let state = active(c.symbol(), c.address())?;
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the guest called abort()",""",
     """    let state = active(c.symbol(), c.address())?;
    c.ret().void();
    return Ok(());
    #[allow(unreachable_code)]
    Err(AbiError::GuestAborted {
        symbol: c.symbol().to_string(),
        address: c.address(),
        why: "the guest called abort()",""",
     ANDROID),

    ("procenv-A8", "A", "_exit loses the status the guest asked to exit with", ADAPTER_PROCENV,
     """    Err(AbiError::GuestExited {
        symbol: c.symbol().to_string(),
        address: c.address(),
        status,
    })""",
     """    let _ = status;
    Err(AbiError::GuestExited {
        symbol: c.symbol().to_string(),
        address: c.address(),
        status: 0,
    })""",
     ANDROID),

    ("procenv-A9", "A",
     "the abort message is dropped, so the only account of the crash is lost", ADAPTER_PROCENV,
     """        why: "the guest called abort()",
        message: state.bionic.abort_message(),""",
     """        why: "the guest called abort()",
        message: { let _ = &state; None },""",
     ANDROID),

    # The over-correction: getenv(NULL) is undefined in C, and NULL is the answer that cannot be
    # mistaken for a value. Refusing it fails a program that is merely careless.
    ("procenv-B1", "B", "getenv refuses a null name instead of answering NULL", ADAPTER_PROCENV,
     """        if name == 0 {
            0
        } else {""",
     """        if name == 0 {
            return Err(view.refusal("a null name"));
        } else {""",
     ANDROID),

    # The over-correction: an unset property is a fact, and 0 with an empty string is what bionic
    # answers. Refusing it turns "this host has no property service" into a halt.
    ("procenv-B2", "B", "an unset system property is refused instead of answered as unset",
     ADAPTER_PROCENV,
     """        let text = found.unwrap_or_default();""",
     """        let Some(text) = found else {
            return Err(view.refusal("no such property"));
        };""",
     ANDROID),

    # ---- the log sink ----------------------------------------------------------------------------

    ("logging-A1", "A", "syslog drops the facility instead of carrying it into the tag",
     ADAPTER_LOGGING,
     """            (Some(ident), facility) => format!("{ident}[facility {facility}]"),""",
     """            (Some(ident), _facility) => ident,""",
     ANDROID),

    ("logging-A2", "A", "closelog leaves openlog's ident in place", ADAPTER_LOGGING,
     """    let state = active(c.symbol(), c.address())?;
    state.bionic.set_syslog_ident(None);
    c.ret().void();""",
     """    let state = active(c.symbol(), c.address())?;
    let _ = &state;
    c.ret().void();""",
     ANDROID),

    ("logging-A3", "A", "the capture ring drops its newest records rather than its oldest",
     ADAPTER_MOD,
     """            ring.pop_front();""",
     """            ring.pop_back();""",
     ANDROID),

    # The over-correction: a ring too small to hold what one run produces is a bound that destroys
    # the thing it was bounding.
    ("logging-B1", "B", "the log capture ring is tightened to four records", ADAPTER_MOD,
     """pub const LOG_CAPTURE_MAX: usize = 256;""",
     """pub const LOG_CAPTURE_MAX: usize = 4;""",
     ANDROID),

    # ---- the cond test's hang guard must stay a hang guard ---------------------------------------
    # `signal_wakes_exactly_one` counts how many waiters finished after one signal. Each waiter's
    # timeout is a HANG GUARD; when it was 400 ms it could fire inside the observation window --
    # registration polls four threads at 10 ms a turn -- and a second thread finished on its own
    # timeout rather than on the signal. Seen three times, once laundering itself into a `wcslen`
    # mutation's catch list. The row restores the racing value; the in-test relation catches it
    # deterministically, with no timing dependence of its own.
    ("bionic-B3", "B",
     "the cond hang guard shrunk back to a value that can fire while the count is taken",
     BIONIC_COND,
     """    const WAITER_HANG_GUARD: Duration = Duration::from_secs(5);""",
     """    const WAITER_HANG_GUARD: Duration = Duration::from_millis(400);""",
     BIONIC),
    # ---- phase 3b: the confinement -------------------------------------------------------------
    # The rules in `fs::path` are the whole of what stops a guest opening an arbitrary host file,
    # and the APK under test is cheat-injected (D6). Each row removes one of them.

    ("fs-A1", "A", "the lexical `..` pop removed, so a traversal reaches the host", PLAT_FS_PATH,
     """                components.pop();""",
     """                components.push(String::from(".."));""",
     PLATFORM),

    ("fs-A2", "A", "component hygiene accepts everything: backslashes, drives, device names",
     PLAT_FS_PATH,
     """pub fn hostile_component(name: &str) -> Option<String> {
    if let Some(bad) = name.chars().find(|c| c.is_control()) {""",
     """pub fn hostile_component(name: &str) -> Option<String> {
    if true {
        return None;
    }
    if let Some(bad) = name.chars().find(|c| c.is_control()) {""",
     PLATFORM),

    # The over-correction, and it is the one a "be strict" instinct produces: refusing every path
    # that contains `..` rather than absorbing it. `/a/../b` is an ordinary path a compiler emits.
    ("fs-B1", "B", "a path containing `..` is refused outright rather than absorbed",
     PLAT_FS_PATH,
     """            ".." => {""",
     """            ".." => {
                return Err(FsError::confined(operation, &shown, "no dot-dot"));""",
     PLATFORM),

    # The other over-correction: a rule wide enough to refuse `libroblox.so`.
    ("fs-B2", "B", "component hygiene widened until an ordinary filename is refused",
     PLAT_FS_PATH,
     """    if name.ends_with('.') || name.ends_with(' ') {""",
     """    if name.contains('.') || name.ends_with(' ') {""",
     PLATFORM),

    # ---- phase 3b: the platform seam ------------------------------------------------------------

    # MEASURED and this row is the defect itself: `FileExt::seek_read` on Windows MOVES the file
    # pointer, so a `pread` built on it alone leaves the next sequential read at end of file --
    # every call `Ok`, nothing reported.
    ("fs-A3", "A", "pread stops restoring the descriptor's own offset", PLAT_FS_WINDOWS,
     """    let restored = handle.seek(SeekFrom::Start(saved));""",
     """    let restored = if true { Ok(0u64) } else { handle.seek(SeekFrom::Start(saved)) };""",
     PLATFORM),

    # `st_ino` zero makes every `(st_dev, st_ino)` identity test answer "the same file", which is
    # the worst answer a `stat` has available.
    ("fs-A4", "A", "st_ino becomes a constant, so every file is the same file", PLAT_FS,
     """    if hash == 0 {
        1
    } else {
        hash
    }""",
     """    let _ = hash;
    0""",
     PLATFORM),

    ("fs-A5", "A", "the descriptor ceiling removed, so a leaking guest holds host handles",
     PLAT_FS,
     """        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let mut table = self.table();
        if table.open.len() >= MAX_OPEN_FILES {""",
     """        let host = self.resolve(OP, guest_path, FinalLink::Refuse)?;
        let mut table = self.table();
        if false && table.open.len() >= MAX_OPEN_FILES {""",
     PLATFORM),

    ("fs-A6", "A", "unlink removes a directory, which is rmdir's job", PLAT_FS,
     """        if metadata.is_dir() {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::IsADirectory,
                "unlink does not remove directories; rmdir does",
            ));
        }""",
     """        if false {
            return Err(FsError::kinded(
                OP,
                host.display().to_string(),
                FsErrorKind::IsADirectory,
                "unlink does not remove directories; rmdir does",
            ));
        }""",
     PLATFORM),

    # The over-correction: a directory bound small enough to refuse an ordinary directory.
    ("fs-B3", "B", "the directory-entry ceiling tightened to two entries", PLAT_FS,
     """pub const MAX_DIR_ENTRIES: usize = 65_536;""",
     """pub const MAX_DIR_ENTRIES: usize = 2;""",
     PLATFORM),

    # ---- phase 3b: the `FILE *` layer in omni-bionic ---------------------------------------------

    # `size * nmemb` is two guest numbers. A release build WRAPS, and the wrapped value (zero)
    # still satisfies "fewer items than asked for" -- so the detector is the errno, not the count.
    ("stdio-A1", "A", "fread's size*nmemb multiplication wraps instead of being checked",
     BIONIC_STDIO,
     """    let Some(total) = size.checked_mul(nmemb) else {
        stream.error = true;
        ctx.set_errno(consts::EINVAL);
        return Ok(0);
    };
    if total == 0 {
        // C: zero items, and the stream is untouched. Not an error, and `size == 0` is the case
        // that would divide by zero below.
        return Ok(0);
    }""",
     """    let total = size.wrapping_mul(nmemb);
    if total == 0 {
        return Ok(0);
    }""",
     BIONIC),

    # The `\\n` is escaped because this pattern is Python source **and** Rust source: an unescaped
    # `\n` here is a newline in the pattern rather than the two characters the Rust file holds,
    # and it matches nothing. The pre-flight pattern gate caught it, which is what it is for.
    ("stdio-A2", "A", "fgets reads past its newline instead of stopping on it", BIONIC_STDIO,
     """                if byte[0] == b'\\n' {
                    break;
                }""",
     """                if false {
                    break;
                }""",
     BIONIC),

    ("stdio-A3", "A", "fgets turns an end of file into an empty line", BIONIC_STDIO,
     """    if written == 0 && stream.eof {""",
     """    if false && written == 0 && stream.eof {""",
     BIONIC),

    ("stdio-A4", "A", "fputc returns the argument, so writing 0xff reads as EOF", BIONIC_STDIO,
     """        Ok(1) => i32::from(byte),""",
     """        Ok(1) => c,""",
     BIONIC),

    ("stdio-A5", "A", "a short read no longer sets the end-of-file flag", BIONIC_STDIO,
     """            Ok(0) => {
                stream.eof = true;
                break;
            }
            Ok(got) => {""",
     """            Ok(0) => {
                break;
            }
            Ok(got) => {""",
     BIONIC),

    # The over-correction: bounding `fgets` by the transfer chunk rather than chunking through it,
    # which silently truncates any line longer than 4 KiB.
    ("stdio-B4", "B", "fgets truncates at one transfer chunk instead of chunking through it",
     BIONIC_STDIO,
     """    let capacity = (size as u32 - 1) as u64;""",
     """    let capacity = ((size as u32 - 1) as u64).min(TRANSFER_CHUNK as u64);""",
     BIONIC),

    # And the other one: refusing `fflush` because this layer cannot promise durability. It can
    # not promise durability, and `fflush` does not ask it to -- that is `fsync`.
    ("stdio-B5", "B", "fflush refuses rather than reporting the contract it does meet",
     BIONIC_STDIO,
     """    match descriptors.flush(stream.fd) {
        Ok(()) => 0,""",
     """    match descriptors.flush(stream.fd) {
        Ok(()) => EOF,""",
     BIONIC),

    # ---- phase 3b: the adapter -------------------------------------------------------------------

    # An unclassified host failure given a specific errno is the plausible-wrong-answer class:
    # guest code retries EIO and moves on, and nothing anywhere says what really happened.
    ("files-A1", "A", "an unclassified host failure is given EIO instead of refusing by name",
     ADAPTER_FILES,
     """        _ => return None,
    })
}""",
     """        _ => consts::EIO,
    })
}""",
     ANDROID),

    ("files-A2", "A", "struct stat's st_size moves onto __pad1", ADAPTER_FILES,
     """    put64(48, stat.size, &mut out);""",
     """    put64(40, stat.size, &mut out);""",
     ANDROID),

    ("files-A3", "A", "access(X_OK) answers 0 instead of refusing", ADAPTER_FILES,
     """        if mode & X_OK != 0 {
            return Err(view.refusal(""",
     """        if false {
            return Err(view.refusal(""",
     ANDROID),

    ("files-A4", "A", "the open flags whose guarantees cannot be met are accepted silently",
     ADAPTER_FILES,
     """        if flags & bit == bit {
            return Err(view.refusal(format!(""",
     """        if false && flags & bit == bit {
            return Err(view.refusal(format!(""",
     ANDROID),

    ("files-A5", "A", "__open_2 stops refusing O_CREAT, which bionic's FORTIFY build aborts on",
     ADAPTER_FILES,
     """        if flags & O_CREAT != 0 || flags & O_TMPFILE == O_TMPFILE {""",
     """        if false {""",
     ANDROID),

    ("files-A6", "A", "readdir answers NULL for a wild DIR pointer, which reads as an empty \
directory", ADAPTER_FILES,
     """        let Some(id) = state.bionic.dir_for(dirp) else {""",
     """        let Some(id) = state.bionic.dir_for(dirp).or(Some(-1)) else {""",
     ANDROID),

    # The over-correction: refusing W_OK as well as X_OK, on the same "Windows has no POSIX
    # permissions" argument. It is the same argument and it is wrong there, because a write probe
    # is an exact answer where an execute probe has none.
    ("files-B6", "B", "access refuses W_OK as well as X_OK", ADAPTER_FILES,
     """        if mode & X_OK != 0 {""",
     """        if mode & (X_OK | W_OK) != 0 {""",
     ANDROID),

    # And the one a "mode is not applied" worry produces: refusing every mkdir that asks for
    # permissions this layer cannot set, which stops the engine creating any directory.
    ("files-B7", "B", "mkdir refuses a mode it cannot apply instead of recording that it cannot",
     ADAPTER_FILES,
     """    let (path, _mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.mkdir(&bytes))? {""",
     """    let (path, mode) = {
        let mut a = c.args();
        (a.next_u64()?, a.next_u64()?)
    };
    let state = active(c.symbol(), c.address())?;
    let result = {
        let mut view = enter(c, &state);
        if mode != 0o777 {
            return Err(view.refusal("a mode this layer cannot apply"));
        }
        let bytes = path_for(view.blaming(0), path, 0)?;
        let fs = filesystem(&view)?;
        match settle(&view, fs.mkdir(&bytes))? {""",
     ANDROID),

    # A `FILE *` this instance never handed out is a wild pointer, a use-after-fclose, or the
    # `&__sF[n]` arithmetic an unverified `sizeof(FILE)` would get wrong. Answering it as an
    # ordinary invalid stream lets guest code route around all three.
    ("stdio-A6", "A", "an unknown FILE pointer is answered instead of refused", ADAPTER_STDIO,
     """    view.active.bionic.stream_of(file).ok_or_else(|| {""",
     """    view.active.bionic.stream_of(file).or(Some(Stream::new(-1))).ok_or_else(|| {""",
     ANDROID),

    # The stream's flags live host-side and have to be written back, or `feof` answers false
    # forever after an end of file. The failure is invisible to anything that does not read the
    # flag after an operation changed it.
    ("stdio-A7", "A", "a stream's flags are never written back, so feof never becomes true",
     ADAPTER_MOD,
     """            if let Some(slot) = self.streams.lock().get_mut(&at) {
                *slot = stream;
            }""",
     """            if let Some(slot) = self.streams.lock().get_mut(&at) {
                let _ = (slot, stream);
            }""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: signals

    # `sigfillset` is the one signal symbol that can be answered exactly, and what it answers is
    # the bits. A set of zeroes is an EMPTY set: every range check, every return value and every
    # errno stays exactly as it was, and the guest is told the opposite of what it asked for.
    ("signals-A1", "A", "sigfillset produces an empty set instead of a full one", BIONIC_SIGNAL,
     """pub const FILLED_BYTE: u8 = 0xFF;""",
     """pub const FILLED_BYTE: u8 = 0x00;""",
     BIONIC),

    # The size is the other half. bionic's LP64 `sigset_t` is one `unsigned long`; a four-byte
    # write leaves signals 33-64 clear in a set the guest was told was full.
    ("signals-A2", "A", "sigfillset fills half the set, leaving signals 33-64 clear",
     BIONIC_LAYOUTS,
     """    pub const SIGSET_T: u64 = 8;""",
     """    pub const SIGSET_T: u64 = 4;""",
     BIONIC),

    # The plausible stub this phase exists to refuse: `sigaction` returning 0 tells the guest a
    # handler is installed, and nothing is observable until the fault it registered for happens.
    ("signals-A3", "A", "sigaction reports that a handler was installed", ADAPTER_SIGNALS,
     """    let shape = if act == 0 {""",
     """    if act != u64::MAX {
        c.ret().i32(0);
        return Ok(());
    }
    let shape = if act == 0 {""",
     ANDROID),

    # The over-correction: refusing `sigfillset` too, on the same "there is no signal delivery
    # here" argument. It is the same argument and it is wrong there, because `sigfillset` is a
    # total function of its one argument and needs no signal state at all.
    ("signals-B1", "B", "sigfillset is refused along with the rest of the family",
     ADAPTER_SIGNALS,
     """    let set = c.args().next_u64()?;
    let state = active(c.symbol(), c.address())?;""",
     """    let set = c.args().next_u64()?;
    if set != u64::MAX {
        return Err(refuse(c, "there is no signal delivery here".to_string()));
    }
    let state = active(c.symbol(), c.address())?;""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: thread lifecycle

    # An instance with no thread host was never configured. EAGAIN says a resource ran out, which
    # is a condition a correct guest retries -- for ever, because nothing will ever free it.
    ("threads-A1", "A", "pthread_create with no thread host reports EAGAIN instead of refusing",
     ADAPTER_THREADS,
     """    let Some(host) = bionic.thread_host() else {
        return call.refuse(""",
     """    let Some(host) = bionic.thread_host() else {
        c.ret(|mut r| r.i32(consts::EAGAIN));
        return Ok(());
        #[allow(unreachable_code)]
        return call.refuse(""",
     ANDROID),

    # **The defect thread lifecycle created and nothing else would have noticed.** Taking the
    # block index from the table's LENGTH is exact only while nothing is ever removed, and this
    # phase removes one at every thread exit: remove the entry holding index 1 from a table of
    # three and the next thread is handed index 2, which is live. Two guest threads then share
    # one `errno` cell, and the symptom is an occasional wrong error number in a thread that did
    # nothing wrong.
    ("threads-A2", "A", "an exited thread's block is handed to a thread that is still using one",
     ADAPTER_RUNTIME,
     """        if let Some(index) = self.free.pop() {
            return Some(index);
        }
        if self.high_water >= capacity {
            return None;
        }
        let index = self.high_water;
        self.high_water += 1;
        Some(index)""",
     """        let index = self.slots.len();
        if index >= capacity {
            return None;
        }
        self.high_water = self.high_water.max(index + 1);
        Some(index)""",
     ANDROID),

    # A thread that stopped without returning produced no `void *`. Reporting 0 with an untouched
    # `retval` is indistinguishable from a thread that returned NULL, which is the one answer the
    # guest cannot tell apart from success.
    ("threads-A3", "A", "joining a thread that faulted reports success", ADAPTER_MOD,
     """            GuestThreadState::Failed(why) => Err(format!(""",
     """            GuestThreadState::Failed(_why) if start_routine != usize::MAX => {
                Ok(JoinOutcome::Returned(0))
            }
            GuestThreadState::Failed(why) => Err(format!(""",
     ANDROID),

    # `pthread_attr_setstacksize(attr, SIZE_MAX)` is two guest numbers meeting a page size. The
    # masked round-up wraps to ZERO, silently in release, and the thread gets a stack made
    # entirely of its guard page.
    ("threads-A4", "A", "a SIZE_MAX stack request wraps to a zero-byte stack", ADAPTER_THREADS,
     """    value.checked_add(to - remainder)""",
     """    Some(value.wrapping_add(to - remainder))""",
     ANDROID),

    # Without the thunk table on the new context, the new thread's first imported call branches
    # into a region that is not executable. The thread dies rather than calling anything, which
    # nothing about `pthread_create`'s own return value would show.
    ("threads-A5", "A", "the thunk table is not installed on the new thread's context",
     ADAPTER_THREADS,
     """    if let Err(error) = boundary.install(&mut *cpu) {""",
     """    if let Err(error) = (if entry == usize::MAX { boundary.install(&mut *cpu) } else { Ok(()) }) {""",
     ANDROID),

    # The start routine's own `RET` is what ends a guest thread, and it ends it by landing on the
    # boundary's sentinel. Without it the thread returns to whatever `X30` held and runs off.
    ("threads-A6", "A", "the new thread's X30 is not the boundary's sentinel", ADAPTER_THREADS,
     """        cpu.set_x(XReg::new(30).expect("X30 exists"), boundary.sentinel() as u64);""",
     """        cpu.set_x(XReg::new(30).expect("X30 exists"), 0);""",
     ANDROID),

    # A thread that exits without giving its block back leaks one per thread, so a guest that
    # creates and joins in a loop stops being able to create threads after 64 of them -- with a
    # refusal that names the thread count and points at the guest rather than at this line.
    ("threads-A7", "A", "an exited guest thread never gives its arena block back",
     ADAPTER_THREADS,
     """    let _ = bionic.threads_table().detach_current();""",
     """    let _ = ();""",
     ANDROID),

    # The over-correction on `pthread_detach`: treating the FIRST detach as the one that is not
    # joinable. A guest that detaches once and never joins then leaks a record for every thread.
    ("threads-B1", "B", "the first pthread_detach is refused as well as the second", ADAPTER_MOD,
     """        if record.detached {
            // POSIX: "the value specified by thread does not refer to a joinable thread". A""",
     """        if !record.detached {
            // POSIX: "the value specified by thread does not refer to a joinable thread". A""",
     ANDROID),

    # The over-correction on `pthread_getschedparam`: refusing it along with the signal family,
    # on a "this runtime does not model scheduling" argument. Nothing in the reachable 188 can
    # SET a policy, so the default is forced rather than approximated, and refusing it stops a
    # correct guest over a field it is only reading.
    ("threads-B2", "B", "pthread_getschedparam refuses instead of answering the forced default",
     ADAPTER_THREADS,
     """    if !known {
        c.ret().i32(consts::ESRCH);
        return Ok(());
    }""",
     """    if !known || thread != u64::MAX {
        return call.refuse("this runtime does not model scheduling policy");
    }""",
     ANDROID),

    # ------------------------------------------------ phase 3c: cross-context code invalidation

    # The state phase 2 left: `invalidate_code` reaching ONE context, so a second guest thread
    # that had translated the same range keeps executing bytes that are no longer mapped.
    ("watch-A1", "A", "an unmap reaches only the calling thread's context", BOUNDARY,
     """        let me = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(token, _)| *token));
        self.boundary.code_watch.broadcast(me.unwrap_or(u64::MAX), (address, len));""",
     """        let me = CONTEXT.with(|cell| cell.borrow().as_ref().map(|(token, _)| *token));
        let _ = (me, address, len);""",
     ANDROID),

    # The over-correction: collapsing to the whole address space on the first range rather than
    # on a full queue. It is always *safe* -- over-invalidating costs translation and nothing
    # else -- which is exactly why nothing but a counter can see it.
    ("watch-B1", "B", "every cross-context invalidation collapses to the whole address space",
     BOUNDARY,
     """            if inner.ranges.len() >= MAX_PENDING_INVALIDATIONS {""",
     """            if inner.ranges.len() < MAX_PENDING_INVALIDATIONS {""",
     ANDROID),

    # ---------------------------------------------------------------- phase 3c: the arena's bases

    # The accessor/layout disagreement an independent review found: the arena test restated
    # `ARENA_BYTES`'s own definition, which cannot fail, so nothing checked that the four
    # accessors agreed with it. With this applied, `fopen` hands out `FILE` objects on top of the
    # pool's interned strings.
    ("arena-A1", "A", "the FILE table starts on top of the pool", ADAPTER_MOD,
     """    pub fn files_base(&self) -> GuestAddr {
        self.pool() + POOL_BYTES
    }""",
     """    pub fn files_base(&self) -> GuestAddr {
        self.pool()
    }""",
     ANDROID_LIB),

    # ---- the gmtime wrap: a defect only a DEBUG build can see -----------------------------------
    # `days * SECONDS_PER_DAY` exceeds i64 only near i64::MIN, and every such timestamp is in a year
    # around -2.9e11, which `i32::try_from(year - 1900)` refuses whatever `second_of_day` holds. So
    # in release the product wraps, the garbage is discarded by that refusal, and the behaviour is
    # correct BY ACCIDENT -- which is why the whole workspace suite stayed green while the Critical
    # was live, and why its regression test could assert nothing and still look like a guard.
    # This row is the detector: `mutate.py` runs debug, where the multiplication panics.
    ("time-A7", "A",
     "the day remainder goes back to a subtraction that overflows near i64::MIN",
     BIONIC_TIME,
     """    let second_of_day = timestamp.rem_euclid(SECONDS_PER_DAY);""",
     """    let second_of_day = timestamp - floor_div(timestamp, SECONDS_PER_DAY) * SECONDS_PER_DAY;""",
     BIONIC),

    # ---- the confinement's last two rules, which had no row at all ------------------------------
    # A review found rules 5 and 6 covered by exactly one test, which SILENTLY SKIPPED on this host:
    # an unelevated Windows session cannot create a symbolic link (MEASURED: WinError 1314). So on
    # the machine whose green suite was the evidence for them, neither rule had ever executed, and
    # fs-A1..A4/B1..B3 all sit in rules 1-4. The test now falls back to a directory junction, which
    # needs no privilege, and FAILS LOUDLY rather than skipping if it can make neither.
    ("fs-A7", "A",
     "the symlink refusal never fires, so a link inside the root is followed out of it",
     PLAT_FS_PATH,
     """            Ok(metadata) if metadata.file_type().is_symlink() => {""",
     """            Ok(metadata) if false && metadata.file_type().is_symlink() => {""",
     PLATFORM),

    # Rule 6 guards `PathBuf::push` with an ABSOLUTE component, which replaces the path rather than
    # appending. `starts_with` is lexical, so a `..` component would never reach this -- which is
    # why the test builds its `Resolved` by hand.
    ("fs-A8", "A",
     "the containment check never fires, so an absolute component escapes the root",
     PLAT_FS_PATH,
     """    if !host.starts_with(root) {""",
     """    if false && !host.starts_with(root) {""",
     PLATFORM),

    # ================================================================ phase 3d: the network group
    #
    # `omni-platform` did not grow for this phase, so there is nothing to mutate on that side.
    # `poll` and `select` are mutated where their answer is decided, which is the adapter, and
    # `inet_ntop`'s formatting is mutated in `omni-bionic`, which is where BIND's rules live.

    # BIND formats into a local buffer and only then compares against `size`. Without the compare,
    # a destination the guest said was eight bytes long receives nine -- and the nine are a
    # truncated address, which is still a printable string naming a different host.
    ("net-A1", "A", "inet_ntop writes a truncated address instead of reporting ENOSPC",
     BIONIC_NET,
     """    if size as usize <= text.len() {""",
     """    if false && size as usize <= text.len() {""",
     BIONIC),

    # `best.len > 1`. A naive "compress the longest run" produces `1::2:3:4:5:6:7` for an address
    # with one zero group: a different, shorter, plausible spelling, and not bionic's.
    ("net-A2", "A", "a single zero group is compressed, which is not what BIND does",
     BIONIC_NET,
     """    let best = best.filter(|(_, len)| *len > 1);""",
     """    let best = best.filter(|(_, len)| *len > 0);""",
     BIONIC),

    # The encapsulated-IPv4 tail. Without `len == 6`, `::1.2.3.4` prints as `::102:304` -- the same
    # address, spelled the way the modern standard library spells it and not the way bionic does.
    # The 200,000-address differential run found that divergence rather than assuming it, and this
    # row is what keeps the finding.
    ("net-A3", "A", "the IPv4-compatible form loses its dotted tail",
     BIONIC_NET,
     """                base == 0 && (len == 6 || (len == 5 && words[5] == 0xFFFF))""",
     """                base == 0 && (len == 5 && words[5] == 0xFFFF)""",
     BIONIC),

    # The pooled `gai_strerror` table. A bound of one leaves every code but `Success` falling
    # through to "Unknown error", which is a string, prints, and says nothing.
    ("net-A4", "A", "gai_strerror answers Unknown error for codes that are in its table",
     ADAPTER_MOD,
     """            Ok(index) if index < known => index,""",
     """            Ok(index) if index < 1 => index,""",
     ANDROID),

    # The fallback row is interned last and `gai_message` indexes it by `known`. Without it, a code
    # outside the table indexes past the end and gets the pool's base, which holds `"UTC"`.
    ("net-A5", "A", "the gai_strerror fallback row is never interned",
     ADAPTER_MOD,
     """        for code in 0..=omni_bionic::net::GAI_MESSAGES as i32 {""",
     """        for code in 0..omni_bionic::net::GAI_MESSAGES as i32 {""",
     ANDROID),

    # POSIX: a negative descriptor is ignored with a zeroed `revents`. It is the idiom for a slot a
    # program has stopped using, so `POLLNVAL` there makes every such program see an error it has
    # no cause for -- and makes `poll` return a non-zero count for an array of disabled slots.
    ("net-A6", "A", "a negative pollfd is reported POLLNVAL instead of being ignored",
     ADAPTER_NET,
     """        let revents = if fd < 0 {""",
     """        let revents = if false && fd < 0 {""",
     ANDROID),

    # A descriptor nothing opened reported as ready. The guest then reads it and gets EBADF from a
    # call `poll` had just said would not block.
    # **RE-ANCHORED in M5**, when `poll` stopped answering "open, therefore ready" and started
    # asking `Filesystem::readiness`. The property is unchanged and so is the detector; what moved
    # is the line that holds it. The pre-flight is what found the staleness, which is what it is
    # for — six rows went stale the same way once before.
    ("net-A7", "A", "a descriptor that is not open is reported ready rather than POLLNVAL",
     ADAPTER_NET,
     """                _ => POLLNVAL,""",
     """                _ => events & READY_MASK,""",
     ANDROID),

    # **Review finding M1's shape, in this group.** Read the whole array, decide, write it once --
    # against reading and writing entry by entry, which leaves a half-updated `revents` array
    # behind a reported failure. One row rather than two, because each half alone is invisible: a
    # per-entry read fails before the whole-array write is reached, and a per-entry write is never
    # reached after a whole-array read has failed. VERIFIED as a detector before the row was
    # written -- the first entry's sentinel is overwritten and
    # `a_poll_array_that_runs_off_its_mapping_leaves_the_first_entry_untouched` fails.
    ("net-A8", "A", "poll answers the guest's array entry by entry instead of all at once",
     ADAPTER_NET,
     """    let mut entries = view.mem().read_bytes(at, bytes, blame)?;""",
     """    let mut entries = vec![0u8; bytes];
    for (i, chunk) in entries.chunks_exact_mut(POLLFD_BYTES).enumerate() {
        chunk.copy_from_slice(&view.mem().read_bytes(at + i * POLLFD_BYTES, POLLFD_BYTES, blame)?);
        let mut answer = [chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], 1, 0];
        if i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) < 0 {
            answer[6] = 0;
        }
        view.mem().write_bytes(at + i * POLLFD_BYTES, &answer, blame)?;
    }""",
     ANDROID),

    # `nfds` is an `nfds_t`, which is 64 bits and is the guest's. Without the cap, `poll(p, 1025,
    # 0)` reads an array eight kilobytes long out of whatever follows the guest's own, and
    # `poll(p, SIZE_MAX, 0)` asks this layer for 147 exabytes -- which in a debug build is an
    # arithmetic panic reachable from guest input, the shape Global Constraint 11 calls Critical.
    ("net-A10", "A", "poll's nfds cap removed, so a guest-chosen count is honoured",
     ADAPTER_NET,
     """        if nfds > MAX_POLL_FDS {""",
     """        if false && nfds > MAX_POLL_FDS {""",
     ANDROID),

    # `select` reports `EBADF` for the CALL, not for one bit. Without the check, a descriptor
    # nothing opened is reported ready in whichever set named it.
    ("net-A11", "A", "select reports a descriptor nothing opened as ready",
     ADAPTER_NET,
     """        if named.iter().any(|fd| !fs.is_open(*fd)) {""",
     """        if named.iter().any(|fd| !fs.is_open(*fd)) && false {""",
     ANDROID),

    # POSIX: `select` returns the total number of bits set across all the masks, so one descriptor
    # ready in two sets is two. Counting descriptors instead returns one -- a smaller, entirely
    # reasonable-looking number.
    # **RE-ANCHORED in M5.** `sets[2].clear()` moved into `answer_sets` and the count moved into
    # `ready_bits`, which the first evaluation and every pass of the wait now share — so the row
    # anchors on the function rather than on one of two identical expressions.
    ("net-A12", "A", "select counts ready descriptors instead of ready bits",
     ADAPTER_NET,
     """    sets[0].count(nfds) + sets[1].count(nfds)""",
     """    sets[0].count(nfds).max(sets[1].count(nfds))""",
     ANDROID),

    # Nothing in this runtime can raise an exception condition, so the exception set comes back
    # empty. Leaving the guest's own bits in it says every descriptor it asked about has one.
    # **RE-ANCHORED in M5**: the clear moved into `answer_sets`, which is now the one place any
    # set is answered.
    ("net-A13", "A", "select leaves the guest's bits in the exception set",
     ADAPTER_NET,
     """    sets[2].clear();
}""",
     """}""",
     ANDROID),

    # Bionic converts the `timeval` before the syscall and reports a `tv_usec` outside [0, 1e6) as
    # EINVAL itself. Without the check, `{i64::MIN, i64::MIN}` reaches the duration arithmetic.
    ("net-A14", "A", "select accepts a malformed struct timeval",
     ADAPTER_NET,
     """        if !(0..MICROS_PER_SECOND).contains(&micros) || seconds < 0 {""",
     """        if false && (!(0..MICROS_PER_SECOND).contains(&micros) || seconds < 0) {""",
     ANDROID),

    # `socket` answering -1/EAFNOSUPPORT is the most believable wrong answer this phase had: a
    # legitimate POSIX outcome that a networked program branches on quietly, so the engine disables
    # its own networking during initialisation and nothing records that this layer, rather than the
    # device, decided that.
    ("net-A15", "A", "socket answers -1/EAFNOSUPPORT instead of refusing",
     ADAPTER_NET,
     """    let family = match domain {""",
     """    if domain != i32::MIN {
        let state = active(c.symbol(), c.address())?;
        {
            let mut view = enter(c, &state);
            view.set_errno(consts::EAFNOSUPPORT);
        }
        c.ret().i32(-1);
        return Ok(());
    }
    let family = match domain {""",
     ANDROID),

    # `freeaddrinfo` returns `void`, which is what makes a silent no-op the dangerous answer: there
    # is no value to be wrong, so nothing distinguishes it from a correct free.
    ("net-A16", "A", "freeaddrinfo quietly does nothing instead of refusing",
     ADAPTER_NET,
     """    let res = c.args().next_u64()?;""",
     """    let res = c.args().next_u64()?;
    if res != u64::MAX {
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),


    # POSIX: "on failure, the objects pointed to by the readfds, writefds, and errorfds arguments
    # are not modified". With the sets rewritten before the timeout is read, a `tv_usec` of
    # 1,000,000 answers -1/EINVAL **and takes the guest's sets with it**, so a caller that retried
    # the call would retry it with nothing. This was a real defect in the first version of this
    # module, found by re-reading the code rather than by a failing test; `net-A9` is the row that
    # keeps it found.
    # **RE-ANCHORED in M5**, when the sleep became a wait that re-asks the question. The mutation
    # is the same one — zero and write back the guest's sets *before* the timeout is validated —
    # expressed against the new shape: the `timeout == 0` branch is where the parse begins, so
    # clearing and writing there puts the whole rest of the validation after the damage.
    ("net-A9", "A", "select zeroes the guest's sets before it validates the timeout",
     ADAPTER_NET,
     """    let duration = if timeout == 0 {""",
     """    for set in &mut sets {
        set.clear();
    }
    for set in &sets {
        set.write_back(view)?;
    }
    let duration = if timeout == 0 {""",
     ANDROID),

    # ---- the over-corrections ------------------------------------------------------------------

    # The cap on a wait applied to every wait, so a `poll` with a thirty-millisecond timeout is
    # refused. A guest polling with a short timeout is the ordinary case, and refusing it stops a
    # correct guest over a bound that exists for a hostile one.
    ("net-B1", "B", "every bounded wait is refused, not only one past the cap",
     ADAPTER_NET,
     """    if duration.as_secs() > MAX_SLEEP_SECONDS {""",
     """    if duration.as_millis() > 0 {""",
     ANDROID),

    # `nfds == FD_SETSIZE` is the last legal value: an `fd_set` holds descriptors 0..FD_SETSIZE, so
    # `select(FD_SETSIZE, ..)` names all of them. Excluding it refuses a correct call.
    ("net-B2", "B", "select refuses an nfds of exactly FD_SETSIZE",
     ADAPTER_NET,
     """    if !(0..=FD_SETSIZE).contains(&nfds) {""",
     """    if !(0..FD_SETSIZE).contains(&nfds) {""",
     ANDROID),

    # The descriptor table consulted whether or not any entry names a descriptor, so a `poll` over
    # an array of disabled slots refuses on an instance with no filesystem root. `poll` needs a
    # descriptor table only when it is asked about a descriptor.
    ("net-B3", "B", "poll needs a filesystem even when it is asked about no descriptors",
     ADAPTER_NET,
     """    let fs = if names_a_descriptor { Some(filesystem(view)?) } else { None };""",
     """    let _ = names_a_descriptor;
    let fs = Some(filesystem(view)?);""",
     ANDROID),

    # ================================================================ phase 3e: the last six

    # The two `__gcov_*` imports resolve to nothing only because the reference is WEAK. Without
    # that check every declared-absent symbol resolves to nothing however it is referenced, and a
    # strong reference becomes a branch to address zero with no symbol attached -- the failure the
    # whole thunk region exists to replace.
    ("gcov-B1", "B", "a strong reference to an absent symbol also resolves to nothing",
     BOUNDARY,
     """                if request.weak && self.is_absent(request.name) {""",
     """                if self.is_absent(request.name) {""",
     ANDROID),

    # And the revert: the absent list ignored, so `__gcov_dump` gets a thunk address, the guest's
    # own `CBZ` falls through, and it calls a symbol no Android device supplies -- followed, four
    # bytes later, by `BL abort`.
    ("gcov-A1", "A", "the absent list is ignored, so a weak import gets an address after all",
     BOUNDARY,
     """                if request.weak && self.is_absent(request.name) {""",
     """                if false && self.is_absent(request.name) {""",
     ANDROID),

    # `time(tloc)` stores the value as well as returning it. A handler that returns the right
    # number and writes nothing is invisible to any test that only reads the return value.
    ("clocks-A8", "A", "time returns the right value and does not store it",
     ADAPTER_CLOCKS,
     """        if tloc != 0 {""",
     """        if false && tloc != 0 {""",
     ANDROID),

    # `CLOCKS_PER_SEC` is a million, fixed by POSIX. Reporting milliseconds is a constant
    # thousand-fold error in every ratio `clock()` is used to compute, and the value still rises.
    ("clocks-A9", "A", "clock reports milliseconds where CLOCKS_PER_SEC says microseconds",
     ADAPTER_CLOCKS,
     """        i64::try_from(cpu.as_micros()).map_err(|_| {""",
     """        i64::try_from(cpu.as_millis()).map_err(|_| {""",
     ANDROID),

    # The process CPU clock answered from the wall clock: monotonic, a plausible number of seconds,
    # and not what was asked for -- the exact failure `clocks-A1` records for `CLOCK_MONOTONIC`,
    # one clock along. Its detector is the one assertion a wall clock cannot satisfy: several
    # threads burning one interval of wall time advance a process CPU clock by more than it.
    ("plat-A9", "A", "process CPU time is served from the monotonic clock",
     PLAT_PROCESS_MOD,
     """pub fn cpu_time() -> ProcessResult<Duration> {
    backend::cpu_time()
}""",
     """pub fn cpu_time() -> ProcessResult<Duration> {
    let _ = backend::cpu_time();
    Ok(crate::clock::monotonic_now())
}""",
     PLATFORM),

    # `mallinfo` answering eighty zeroed bytes is the believable wrong answer precisely because it
    # is arithmetically TRUE of a libc heap nothing has allocated from -- and `libroblox.so`
    # imports no allocator at all, so there is no heap for it to describe.
    ("guestmem-A9", "A", "mallinfo writes eighty zeroed bytes instead of refusing",
     ADAPTER_GUESTMEM,
     """    let out = c.args().indirect_result();""",
     """    let out = c.args().indirect_result();
    if out != usize::MAX {
        c.mem().write_bytes(
            out,
            &[0u8; MALLINFO_BYTES],
            crate::mem::Blame::new(c.symbol(), c.address(), 0),
        )?;
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),

    # `longjmp` is declared `noreturn`. A handler that quietly returns resumes the guest in the
    # frame it was trying to escape, carrying whatever condition made it jump -- the same failure
    # `raise` declines, one frame further in.
    ("signals-A4", "A", "longjmp returns normally instead of refusing",
     ADAPTER_SIGNALS,
     """    let delivered = if val == 0 { 1 } else { val };""",
     """    let delivered = if val == 0 { 1 } else { val };
    if env != u64::MAX {
        c.ret().void();
        return Ok(());
    }""",
     ANDROID),
    # ------------------------------------------------------------------ M3 task 4: the gate
    #
    # Running all 3,594 initializers is what found every defect below, and every row's detector is
    # an ordinary fast test rather than the gate itself: the gate loads 109 MB and executes 91.6 M
    # guest instructions, so a row scoped to it would multiply this harness's cost by that.

    # **The defect the gate cost the most to find.** Linux ignores `fd` entirely when
    # MAP_ANONYMOUS is set -- `mmap(2)` says so -- and this clause refused every anonymous
    # mapping made with the `0` the engine's own allocator passes. `libroblox.so` imports no
    # allocator, so guest `mmap` IS the heap seam: the gate went from 188 initializers to 3,096
    # on this one clause, and no existing test could see it because every one of them passes -1.
    ("guestmem-A10", "A", "an anonymous mmap with a non-negative fd is refused as file-backed",
     ADAPTER_GUESTMEM,
     """    if flags & MAP_ANONYMOUS == 0 {""",
     """    if fd != -1 || flags & MAP_ANONYMOUS == 0 {""",
     ANDROID),

    # `dlsym` answering with an `Unbound` slot's address. That address exists so a DIRECT call can
    # name the symbol; handing it back through `dlsym` converts a lookup the guest is prepared to
    # see fail into a pointer it will call thousands of initializers later -- which is exactly the
    # argument phase 2 refused all three `dl*` calls on, and the half of it that survives.
    ("boundary-A13", "A", "dlsym answers with an Unbound slot instead of missing",
     BOUNDARY,
     """        if matches!(slot.binding, Binding::Unbound) {
            return None;
        }""",
     """""",
     ANDROID),

    # The over-correction on the other side of the same function: a handle for one library
    # answering for every symbol this layer has. The guest's own `.gnu.version_r` says which
    # library each import comes from, and ignoring it makes `dlsym(libc_handle, "eglGetProcAddress")`
    # succeed where a device fails.
    ("boundary-B4", "B", "a library handle resolves symbols from every library",
     BOUNDARY,
     """            Some(name) if slot.library.as_deref() == Some(name) => Some(slot),
            Some(_) => None,""",
     """            Some(_) => Some(slot),""",
     ANDROID),

    # `dlopen` issuing a handle for a library this runtime does not have. NULL is the true answer
    # and the one every caller has a branch for; a handle makes the guest `dlsym` it and carry
    # what came back.
    ("dl-A5", "A", "dlopen issues a handle for a library this runtime does not supply",
     ADAPTER_DL,
     """                        ));
                        0
                    }""",
     """                        ));
                        HANDLE_TAG | (libraries.len() as u64 + 1)
                    }""",
     ANDROID),

    # `_SC_PAGESIZE` off by one. This is the shape D22 refused to risk for three phases: the real
    # page-size query then arrives as an unmodelled number and is refused LOUDLY, while some other
    # `_SC_` name silently receives a page size. The decode of the guest's own call sites is what
    # licensed the constant, so a row that moves it is a row about that evidence.
    ("procenv-A10", "A", "_SC_PAGESIZE is off by one",
     ADAPTER_PROCENV,
     """const SC_PAGESIZE: i32 = 0x0027;""",
     """const SC_PAGESIZE: i32 = 0x0026;""",
     ANDROID),

    # `PR_GET_THP_DISABLE` answering 0 -- "huge pages are available and not disabled" -- instead
    # of the EINVAL a kernel without CONFIG_TRANSPARENT_HUGEPAGE gives. Zero is the believable
    # wrong answer: it is a success, and the allocator then believes a feature exists.
    ("procenv-A11", "A", "the transparent-huge-page prctl options answer 0 instead of EINVAL",
     ADAPTER_PROCENV,
     """        view.set_errno(omni_bionic::errno::consts::EINVAL);
        drop(view);
        c.ret().i32(-1);
        return Ok(());""",
     """        drop(view);
        c.ret().i32(0);
        return Ok(());""",
     ANDROID),

    # `PR_SET_VMA` answering 0 without keeping the label. Keeping it is the ENTIRE observable
    # effect of that call on a device -- the text beside the range in /proc/self/maps -- so a
    # handler that returns 0 and stores nothing is the plausible stub, not an implementation.
    ("procenv-A12", "A", "PR_SET_VMA succeeds without recording the label",
     ADAPTER_PROCENV,
     """                Ok(text) => {
                    state.bionic.set_vma_name(addr, len, text);
                    0
                }""",
     """                Ok(_) => 0,""",
     ANDROID),

    # `gettid` answering the process id. It is the believable wrong answer precisely because it is
    # RIGHT for a single-threaded process -- on Linux the main thread's tid equals the pid -- and
    # it is what a naive implementation reaches for. Every thread would then share one identity.
    ("procenv-A13", "A", "gettid answers the process id instead of the thread identity",
     ADAPTER_PROCENV,
     """        let Ok(narrowed) = i32::try_from(thread.0) else {""",
     """        let thread = omni_bionic::threads::GuestThreadId(u64::from(
            omni_platform::process::pid(),
        ));
        let Ok(narrowed) = i32::try_from(thread.0) else {""",
     ANDROID),

    # `rt_sigprocmask` validating `how` before the `set` pointer. The engine passes an invalid
    # `how` ON PURPOSE and reads the errno to decide whether an address is readable; checking
    # `how` first answers EINVAL for every address, so the probe reports unmapped memory as
    # readable and the guest dereferences it.
    ("procenv-A14", "A", "rt_sigprocmask checks `how` before the pointer, breaking the probe",
     ADAPTER_PROCENV,
     """            if (set != 0 && !readable(&view, set, false))
                || (oldset != 0 && !readable(&view, oldset, true))
            {""",
     """            if false {""",
     ANDROID),

    # The over-correction: answering EFAULT for a readable `set` too. The probe then reports every
    # address as unreadable, and the guest routes around memory it could have used -- a wrong
    # answer with no failure anywhere.
    ("procenv-B5", "B", "rt_sigprocmask answers EFAULT for a readable set as well",
     ADAPTER_PROCENV,
     """            if (set != 0 && !readable(&view, set, false))
                || (oldset != 0 && !readable(&view, oldset, true))
            {""",
     """            if set != 0 || oldset != 0 {""",
     ANDROID),

    # `/dev/urandom` reading end-of-file. Zero is `/dev/null`'s answer, one entry along, and it is
    # what `std::random_device` gets when the file is not there -- the guest's C++ runtime then
    # throws `system_error` and terminates, which is how the gate found the device was needed.
    # A **short read** from `/dev/urandom`. A modern one always fills the buffer, and
    # `std::random_device` reads four bytes at a time with no loop -- so a partial fill leaves the
    # rest of the caller's buffer holding whatever was there, which is the believable wrong
    # answer: the bytes did change, and some of them are not entropy.
    ("plat-A11", "A", "/dev/urandom fills only half the buffer",
     PLAT_FS,
     """                    })?;
                    Ok(buf.len())
                }
                Device::Null => Ok(0),""",
     """                    })?;
                    Ok(buf.len() / 2)
                }
                Device::Null => Ok(0),""",
     PLATFORM),

    # The over-correction: every path under `/dev` becomes a device. A guest opening
    # `/dev/watchdog` would get a readable, writable character device instead of the ENOENT that
    # says this runtime does not have one, and the confinement rule would stop meaning anything
    # for that prefix.
    ("plat-B6", "B", "any path under /dev is treated as a device",
     PLAT_FS,
     """    DEVICES.iter().find(|(name, _)| *name == spelled).map(|(_, device)| *device)""",
     """    if spelled.starts_with("/dev/") {
        return Some(Device::Random);
    }
    DEVICES.iter().find(|(name, _)| *name == spelled).map(|(_, device)| *device)""",
     PLATFORM),

    # `mbtowc` reporting an illegal sequence as a successful zero-length decode. `0` is the value
    # for a NUL character, so the caller reads "end of string" and stops -- silently truncating
    # every string with a byte it could not decode, where a device answers -1 and EILSEQ.
    ("bionic-A12", "A", "mbtowc reports an illegal sequence as a NUL character",
     BIONIC_WIDE,
     """        Decode::Invalid | Decode::Incomplete => {
            ctx.set_errno(EILSEQ);
            Ok(-1)
        }""",
     """        Decode::Invalid | Decode::Incomplete => Ok(0),""",
     BIONIC),

    # POSIX `strerror_r` returning the buffer pointer, which is the GNU form's return value.
    # `libroblox.so` imports both spellings; a caller of the POSIX one tests the result against 0
    # and would read every success as a failure -- or, with a buffer at a low address, the other
    # way round.
    ("bionic-A13", "A", "POSIX strerror_r returns the buffer pointer like the GNU form",
     BIONIC_STRING,
     """    if message.len() >= len as usize {
        return Ok(crate::errno::consts::ERANGE);
    }
    Ok(0)""",
     """    Ok(b as i32)""",
     BIONIC),

    # =============================================================================================
    # M6 groundwork -- `omni-texture`, the ETC1 decoder. Added by the texture-transcoding task.
    #
    # A wrong decoder does not crash: it produces a plausible, silently wrong texture thousands of
    # frames before anyone looks at it. Every row below is a mutation that a casual test suite
    # would not notice, which is the whole reason the vectors in `tests/spec_vectors.rs` are
    # derived from the specification rather than from another decoder.
    # =============================================================================================

    # ---- the ETC2 escape: the one place an ETC1 decoder produces believable wrong pixels --------
    ("texture-A1", "A", "an ETC2 T/H/planar block is decoded as a differential block",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {
            return Err(modes[channel]);
        }""",
     """        if false && !(0..=31).contains(&sum) {
            return Err(modes[channel]);
        }""",
     TEXTURE),

    ("texture-A2", "A", "only the overflow end of the ETC2 escape is tested, not the underflow",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {""",
     """        if sum > 31 {""",
     TEXTURE),

    # The over-correction of the same check. Base 0 and base 31 are perfectly legal ETC1 endpoints
    # -- the real water-normal and skybox blocks use both -- and narrowing the range to refuse them
    # reads as "be strict about the boundary" while rejecting ordinary content.
    ("texture-B1", "B", "the ETC2 escape range is narrowed, refusing legitimate 0 and 31 endpoints",
     TEXTURE_ETC1,
     """        if !(0..=31).contains(&sum) {""",
     """        if !(1..=30).contains(&sum) {""",
     TEXTURE),

    # ---- the pixel index layout: the classic transposition ---------------------------------------
    ("texture-A3", "A", "pixel numbering is row-first, so every block is transposed",
     TEXTURE_ETC1,
     """            let i = x * BLOCK_EXTENT + y;""",
     """            let i = y * BLOCK_EXTENT + x;""",
     TEXTURE),

    ("texture-A4", "A", "the msb and lsb index bit planes are swapped",
     TEXTURE_ETC1,
     """            let lsb = (indices >> i) & 1;
            let msb = (indices >> (i + 16)) & 1;""",
     """            let lsb = (indices >> (i + 16)) & 1;
            let msb = (indices >> i) & 1;""",
     TEXTURE),

    # Specification table 8.16 maps 00 -> a, 01 -> b, 10 -> -a, 11 -> -b, which over the ascending
    # set {-b, -a, a, b} is elements 2, 3, 1, 0. The identity mapping is what a flattened table
    # copied in the wrong order gives, and it is a plausible-looking image.
    ("texture-A5", "A", "the pixel-index-to-modifier mapping is the identity",
     TEXTURE_ETC1,
     """const PIXEL_INDEX_TO_SET_ELEMENT: [usize; 4] = [2, 3, 1, 0];""",
     """const PIXEL_INDEX_TO_SET_ELEMENT: [usize; 4] = [0, 1, 2, 3];""",
     TEXTURE),

    # ---- base colour reconstruction ---------------------------------------------------------------
    ("texture-A6", "A", "5-to-8 bit extension shifts without replicating the high bits",
     TEXTURE_ETC1,
     """    (value << 3) | (value >> 2)""",
     """    value << 3""",
     TEXTURE),

    ("texture-A7", "A", "4-to-8 bit extension shifts without replicating the nibble",
     TEXTURE_ETC1,
     """    (value << 4) | value""",
     """    value << 4""",
     TEXTURE),

    ("texture-A8", "A", "the modifier wraps instead of saturating",
     TEXTURE_ETC1,
     """    let value = base as i32 + modifier;
    if value < 0 {
        0
    } else if value > 255 {
        255
    } else {
        value as u8
    }""",
     """    let value = base as i32 + modifier;
    value as u8""",
     TEXTURE),

    ("texture-A9", "A", "the diffbit is ignored, so every block decodes as differential",
     TEXTURE_ETC1,
     """    if (block[3] >> 1) & 1 == 0 {""",
     """    if false {""",
     TEXTURE),

    ("texture-A10", "A", "the flipbit is ignored, so the sub-block split is always left/right",
     TEXTURE_ETC1,
     """            let sub = usize::from(if flip { y >= 2 } else { x >= 2 });""",
     """            let sub = usize::from(x >= 2);""",
     TEXTURE),

    ("texture-A11", "A", "decoded alpha is transparent where an RGB format must read opaque",
     TEXTURE_ETC1,
     """            out[at + 3] = 0xFF;""",
     """            out[at + 3] = 0x00;""",
     TEXTURE),

    # ---- the format gate --------------------------------------------------------------------------
    # The believable one: ETC2's RGB8 form is a superset of ETC1 at the container level, so
    # accepting it here "obviously works" -- right up to the first block that uses a mode this
    # decoder does not have, which is content-dependent and therefore intermittent.
    ("texture-A12", "A", "GL_COMPRESSED_RGB8_ETC2 is accepted as though it were ETC1",
     TEXTURE_FORMAT,
     """        if gl_internal_format == 0x8D64 {""",
     """        if gl_internal_format == 0x8D64 || gl_internal_format == 0x9274 {""",
     TEXTURE),

    # ---- sizes, extents and the block grid --------------------------------------------------------
    ("texture-A13", "A", "the block grid truncates instead of rounding up",
     TEXTURE_LIB,
     """    let blocks_x = width / bw + u32::from(width % bw != 0);""",
     """    let blocks_x = width / bw;""",
     TEXTURE),

    ("texture-A14", "A", "an undersized destination is written into instead of refused",
     TEXTURE_LIB,
     """    if out.len() < needed_out {
        return Err(TextureError::OutputTooSmall {""",
     """    if false && out.len() < needed_out {
        return Err(TextureError::OutputTooSmall {""",
     TEXTURE),

    ("texture-A15", "A", "a zero extent is accepted and reports a zero-byte image",
     TEXTURE_LIB,
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 {""",
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if false && (width == 0 || height == 0) {""",
     TEXTURE),

    # ---- direction B: the three over-corrections ---------------------------------------------------
    # Each of these reads as "be stricter", and each destroys a property the design depends on.

    # A KTX mip level is padded to a four-byte boundary and a caller may hand over the rest of the
    # chain; GL's own `imageSize` is a lower bound, not an equality. Requiring an exact length
    # refuses the real APK's own files.
    ("texture-B2", "B", "a payload longer than the block grid is refused as truncated",
     TEXTURE_LIB,
     """    if data.len() < needed_in {
        return Err(TextureError::TruncatedBlockData {""",
     """    if data.len() != needed_in {
        return Err(TextureError::TruncatedBlockData {""",
     TEXTURE),

    ("texture-B3", "B", "a destination larger than the image is refused as too small",
     TEXTURE_LIB,
     """    let needed_out = decoded_len(width, height)?;
    if out.len() < needed_out {""",
     """    let needed_out = decoded_len(width, height)?;
    if out.len() != needed_out {""",
     TEXTURE),

    # The device limit that does not belong here. `maxImageDimension2D` is 32,768 on the
    # development host, but that is a property of a device and this crate has no device in it: the
    # only bound that belongs here is arithmetic. A limit invented at this layer silently caps the
    # renderer on hardware that could go higher.
    ("texture-B4", "B", "a maximum dimension is invented inside pure computation",
     TEXTURE_LIB,
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 {""",
     """pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 || width > 4096 || height > 4096 {""",
     TEXTURE),

    # GL permits a compressed image whose dimensions are not multiples of the block size; the
    # texels past the edge are discarded. Refusing them outright is what the engine does for DXT
    # (`ERROR: DXT texture dimension {}x{} not divisible by 4.` is in `libroblox.so`), which is
    # exactly what makes it a believable over-correction here.
    ("texture-B5", "B", "a non-multiple-of-four extent is refused instead of clipped",
     TEXTURE_LIB,
     """    let (bw, bh) = format.block_extent();""",
     """    let (bw, bh) = format.block_extent();
    if width % bw != 0 || height % bh != 0 {
        return Err(TextureError::ZeroExtent { width, height });
    }""",
     TEXTURE),


    # ======================================================================= M4: JNI without a JVM
    #
    # Every row below is a property jni-surface.md or this milestone's own measurements
    # established. The A rows revert a fix and something must fail; the B rows over-correct --
    # they read as more careful and destroy a property the design depends on.

    # **The defect M4's gate found.** A `jclass` is an instance of `java.lang.Class`, and
    # `JvmClassLoaderHelper` takes the class of a class and asks *that* for `getClassLoader`.
    # Answering the class itself makes the lookup ask `NativeGLJavaInterface.getClassLoader`,
    # which does not exist, and the null `jmethodID` goes straight into `CallObjectMethodV`.
    ("jni-A1", "A", "GetObjectClass on a jclass answers the class itself",
     JNI_ENV,
     """        Object::Class(_) => registry.find("java/lang/Class"),""",
     """        Object::Class(class) => Some(*class),""",
     ANDROID_LIB),

    # The handle check word ignored. A `jobject` the guest deleted and used again resolves to
    # whatever took its slot -- and `DeleteLocalRef` has 45 call sites, so that is a shape the
    # engine produces in the ordinary course of running.
    ("jni-A2", "A", "a stale or forged jobject is not caught by its check word",
     JNI_REFS,
     """        if (handle >> 28) & CHECK_MASK != self.check_word(slot.generation) {""",
     """        if false {""",
     ANDROID_LIB),

    # `DeleteLocalRef` on a global reference performed rather than reported. The two have
    # different lifetimes, and deleting the wrong one leaves a later call holding a handle it was
    # entitled to keep.
    ("jni-A3", "A", "a reference is deleted through the wrong kind of DeleteRef",
     JNI_REFS,
     """        if actual != expected {""",
     """        if false {""",
     ANDROID_LIB),

    # `Answer::Unanswered` evaluating to a value. This is Global Constraint 1's failure shape
    # exactly: a Java getter nobody decided the answer for returning a believable zero.
    ("jni-A4", "A", "a member nobody decided the answer for returns zero",
     JNI_CLASSES,
     """            Answer::Sink => Value::Void,""",
     """            Answer::Unanswered => Value::Int(0),
            Answer::Sink => Value::Void,""",
     ANDROID_LIB),

    # Modified UTF-8's first divergence from UTF-8: U+0000 is `C0 80`, never a zero byte. Writing
    # it as one byte terminates the string the guest is about to read at the first NUL character
    # in it.
    ("jni-A5", "A", "modified UTF-8 writes U+0000 as a single zero byte",
     JNI_VALUES,
     """                0x0000 | 0x0080..=0x07ff => {""",
     """                0x0000 => out.push(0),
                0x0080..=0x07ff => {""",
     ANDROID_LIB),

    # One entry of `JNINativeInterface` transposed. Every slot at or after it moves by one, so the
    # guest's `ldr Xt,[Xb,#imm]` reaches a different function than the one the offset names.
    ("jni-A6", "A", "two JNINativeInterface entries are transposed",
     JNI_SLOTS,
     """    "GetStringUTFLength",
    "GetStringUTFChars",""",
     """    "GetStringUTFChars",
    "GetStringUTFLength",""",
     ANDROID_LIB),

    # A `Release…` given a pointer that is not a live pin silently doing nothing. The buffer stays
    # pinned and the guest reads through a pointer it believes it has given back.
    ("jni-A7", "A", "releasing a pointer that was never pinned is a silent no-op",
     JNI_POOL,
     """        self.live.get(&at).copied().ok_or_else(|| AbiError::JniRefused {""",
     """        self.live.get(&at).copied().or(self.live.values().next().copied()).ok_or_else(|| AbiError::JniRefused {""",
     ANDROID_LIB),

    # The array-region bound removed. `GetByteArrayRegion` and `SetLongArrayRegion` take a
    # guest-chosen start and length, and without this the host reads or writes outside the
    # object's own storage.
    #
    # **This row was `wrapping_add` instead of `checked_add` and nothing caught it**, correctly:
    # both values have come through `usize::try_from` of an `i32`, so on a 64-bit host the sum
    # cannot overflow and the two are the same function. The comment on `region` says so now.
    ("jni-A8", "A", "an array region is not bounds-checked against its array",
     JNI_ENV,
     """    if end > len {""",
     """    if false {""",
     ANDROID_LIB),

    # The generated dex surface put **ahead** of the hand-written members instead of after them.
    # `Registry::method` takes the first match, so every decided answer is shadowed by the
    # generated `Unanswered` copy of the same member and the engine's first call to one refuses.
    #
    # The first attempt at this row made `extend_with` add a member it already had, and nothing
    # caught it -- correctly: a duplicate appended *after* the hand-written one is never reached.
    # Which is the property worth pinning, and it is the order and not the duplication.
    # **Precedence between the two class tables**: a hand-written decided answer must win over
    # the generated `Unanswered` copy of the same member. This replaces the guard *and* the
    # append with an unconditional front-insert, so the generated copy is what `Registry::method`
    # finds.
    #
    # It takes both halves because either one alone is behaviour-preserving, which two earlier
    # attempts at this row found the expensive way: a duplicate appended after the hand-written
    # member is never reached, and a front-insert alone never runs for a member that is already
    # there. The property is held by two independent things, so a row that breaks one of them is
    # not a detector -- and a row that looked like one would have been evidence for nothing.
    ("jni-A9", "A", "the generated surface wins over a decided answer",
     JNI_CLASSES,
     """            if self.method(id, member.name, member.descriptor, member.is_static).is_none() {
                let class = &mut self.classes[usize::from(id.0)];
                if class.methods.len() < usize::from(u16::MAX) {
                    class.methods.push(Member {""",
     """            {
                let class = &mut self.classes[usize::from(id.0)];
                if class.methods.len() < usize::from(u16::MAX) {
                    class.methods.insert(0, Member {""",
     ANDROID_LIB),

    # `__strncpy_chk2`'s source check back to `n > src_size`, which aborts
    # `strncpy(dst, src, sizeof dst)` with a shorter source -- the commonest FORTIFY shape there
    # is, and what stopped jni-surface.md section 8 step 6 on the real engine.
    ("jni-A10", "A", "__strncpy_chk2 fails whenever n exceeds the source object",
     BIONIC_STRING,
     """    let readable = n.min(src_size);""",
     """    if n > src_size {
        return Err(crate::error::BionicError::CheckFailed("__strncpy_chk2"));
    }
    let readable = n.min(src_size);""",
     BIONIC),

    # `MADV_DONTNEED` degraded to `MADV_FREE`: marked idle and never reclaimed, so the old
    # contents survive a guarantee that says a later read is zero.
    ("jni-A11", "A", "MADV_DONTNEED marks the range idle and never reclaims it",
     ADAPTER_GUESTMEM,
     """        if let Err(error) = space.reclaim_idle() {""",
     """        if let Ok(()) = Ok::<(), omni_mem::MemError>(()) {
            c.invalidate_code(at, len)?;
            c.ret(|mut r| r.i32(0));
            return Ok(());
        }
        if let Err(error) = space.reclaim_idle() {""",
     ANDROID),

    # ---- B: the over-corrections -----------------------------------------------------------

    # Every JNI slot on the exit path. It reads as safer -- a handler that *may* call guest code
    # cannot then be on a path that structurally cannot -- and it destroys D17's measured split,
    # putting all 943 call sites on the 80-105 ns path instead of the 33 ns one.
    ("jni-B1", "B", "every JNIEnv slot is serviced on the exit path",
     JNI_ENV,
     """    name.starts_with("Call")
        || matches!(""",
     """    let _ = name;
    true
        || matches!(""",
     ANDROID_LIB),

    # The pinned pool committed eagerly. Four megabytes of commit charge per instance for a pool
    # that is empty until the engine asks for a string -- and this runtime hosts three concurrent
    # instances (Global Constraint 6, D10).
    ("jni-B2", "B", "the pinned pool is committed when it is reserved",
     JNI_POOL,
     """            CommitPolicy::Lazy,""",
     """            CommitPolicy::Eager,""",
     ANDROID_LIB),

    # `MADV_DONTNEED` unmapping the range. It is the *immediate* semantics taken one step too far:
    # the contents really do read as zero afterwards, and the mapping the guest still owns is
    # gone, so its next write faults.
    ("jni-B3", "B", "MADV_DONTNEED releases the mapping and not only the contents",
     ADAPTER_GUESTMEM,
     """        if let Err(error) = space.advise_idle(at, len) {""",
     """        if let Err(error) = space.unmap(at, len) {""",
     ANDROID),

    # The miss log bounded to one entry. It reads as tighter and it turns the measurement M5 is
    # built on into a sample of size one: a run that asked for forty members nobody declared
    # reports the first.
    ("jni-B4", "B", "the miss log keeps one entry instead of a thousand",
     JNI_CLASSES,
     """        if self.misses.len() < MAX_MISSES && !self.misses.contains(&miss) {""",
     """        if self.misses.len() < 1 && !self.misses.contains(&miss) {""",
     ANDROID_LIB),

    # Array types refused by the descriptor grammar. It reads as stricter -- an array is not a
    # class type -- and `[B`, `[I` and `[Ljava/lang/Object;` are all over the measured surface,
    # `showKeyboard` and `nativePassInputBatch` among them.
    ("jni-B5", "B", "the descriptor grammar refuses array types",
     JNI_VALUES,
     """            Some(b'[') => {""",
     """            Some(b'[') if false => {""",
     ANDROID_LIB),

    # The pinned-pool cap dropped to nothing. It reads as the safest possible bound and it makes
    # every `GetStringUTFChars` refuse, which is 45 of them on the startup path alone.
    ("jni-B6", "B", "the pinned pool refuses every pin",
     JNI_POOL,
     """        if self.pinned_bytes + need > MAX_PINNED_BYTES {""",
     """        if true {""",
     ANDROID_LIB),

    # =============================================== M5: the pipe, and the descriptor space opening
    #
    # `jni-surface.md` §5.2 needs two pipes before `initializeNativeCode` can return, and binding
    # `pipe` is what ended the closed-descriptor-space argument `bionic/net.rs` used to make. The
    # rows below are over the seam (`omni-platform`), the readiness rules `poll` and `select` now
    # answer from, and the blocking wait the adapter owns because the seam deliberately does not.

    # **The clause end-of-file depends on.** A reader whose writers have all closed must report
    # itself readable, because end of file IS a read that returns immediately. Without it a poll
    # loop parks on a pipe that can never produce another byte -- and every count-based assertion
    # about it still passes, which is `VERIFICATION.md` entry 11's shape exactly.
    ("pipe-A1", "A", "a reader with no writers left is not readable",
     PLAT_FS_PIPE,
     """                    readable: !empty || state.writers == 0,""",
     """                    readable: !empty,""",
     PLATFORM),

    # End of file itself: a read from an emptied, writerless pipe answers `WouldBlock` instead of
    # zero. A guest draining a pipe until `read` returns 0 never stops.
    ("pipe-A2", "A", "an emptied pipe with no writers reports EAGAIN instead of end of file",
     PLAT_FS_PIPE,
     """            if state.writers == 0 {
                // End of file, and it stays end of file: every later read answers zero too.
                return Ok(0);
            }""",
     """            if false {
                return Ok(0);
            }""",
     PLATFORM),

    # A write past the free space refusing the whole request rather than taking what fits. It is
    # the believable wrong answer for this shape: POSIX guarantees atomicity only to `PIPE_BUF`,
    # and a guest writing a large buffer would spin against a pipe that was draining.
    ("pipe-A3", "A", "a write larger than the free space takes nothing",
     PLAT_FS_PIPE,
     """        let taken = buf.len().min(room);
        state.queue.extend(&buf[..taken]);""",
     """        if buf.len() > room {
            return Err(FsError::kinded("write", "a pipe", FsErrorKind::WouldBlock, "no room"));
        }
        let taken = buf.len();
        state.queue.extend(&buf[..taken]);""",
     PLATFORM),

    # Closing an end not waking the gate. The last writer going away is what makes a blocked
    # reader see end of file; a close that does not raise the generation leaves that reader
    # parked until its deadline, which is a stall rather than a wrong answer -- the hardest kind
    # to see.
    ("pipe-A4", "A", "closing an end of a pipe does not wake what is waiting on it",
     PLAT_FS_PIPE,
     """        self.pipe.gate.bump();
    }
}

/// Create a pipe""",
     """        let _ = &self.pipe;
    }
}

/// Create a pipe""",
     PLATFORM),

    # `POLLHUP` masked by what was asked for. POSIX reports it whether or not it was requested,
    # and the canonical drain loop asks only for `POLLIN`: without this the loop never learns the
    # writer is gone.
    ("pipe-A5", "A", "POLLHUP is only reported when it was asked for",
     ADAPTER_NET,
     """    if readiness.hangup {
        revents |= POLLHUP;
    }""",
     """    if readiness.hangup {
        revents |= events & POLLHUP;
    }""",
     ANDROID),

    # The blocking wait removed: a blocking descriptor is told `EAGAIN`, which only a
    # non-blocking one can be told. The plausible wrong answer this whole type exists to prevent.
    ("pipe-A6", "A", "a blocking read reports EAGAIN instead of waiting",
     ADAPTER_FILES,
     """        errno == consts::EAGAIN && fs.is_nonblocking(fd).is_ok_and(|nonblocking| !nonblocking)""",
     """        let _ = (fs, fd, errno);
        false""",
     ANDROID),

    # The all-or-nothing descriptor check for a pipe's two ends. With `+ 1` a pipe fits where only
    # one slot is free, and the instance ends up holding one descriptor past its own ceiling.
    ("pipe-A7", "A", "a pipe needs only one free descriptor slot",
     PLAT_FS,
     """        if table.open.len() + 2 > MAX_OPEN_FILES {""",
     """        if table.open.len() + 1 > MAX_OPEN_FILES {""",
     PLATFORM),

    # `F_SETFL` accepting any bit, which is what Linux does and what this layer must not: a guest
    # that set `O_ASYNC` and was told it worked waits for a signal this runtime never delivers.
    ("pipe-A8", "A", "fcntl(F_SETFL) silently ignores every bit but O_NONBLOCK",
     ADAPTER_FILES,
     """                if unhandled != 0 {""",
     """                if false {""",
     ANDROID),

    # ---- the over-corrections ----

    # A pipe given the always-ready answer the other four kinds get. It reads as restoring the
    # simple rule `poll` used to have, and it makes every `poll` on an empty pipe report data
    # that is not there.
    ("pipe-B1", "B", "a pipe answers always-ready like every other descriptor kind",
     PLAT_FS,
     """            Entry::Pipe(handle) => handle.readiness(),""",
     """            Entry::Pipe(_) => Readiness::ALWAYS,""",
     PLATFORM),

    # The blocking bound removed. It reads as more POSIX-faithful -- a blocking read really does
    # wait indefinitely on a device -- and it is a permanent hang of a host thread, which D16's
    # step budgets cannot end because a sleeping thread executes no guest instructions.
    #
    # **Its detector is a unit test on the bound**, not an end-to-end one: an unbounded blocking
    # read on a pipe nobody writes to does not fail, it never returns. Same precedent, and same
    # reason, as the `clocks::capped` row.
    ("pipe-B2", "B", "a blocking transfer waits for ever, as a device does",
     ADAPTER_FILES,
     """    Instant::now() + Duration::from_secs(MAX_SLEEP_SECONDS)""",
     """    Instant::now() + Duration::from_secs(60 * 60 * 24 * 365)""",
     ANDROID_LIB),

    # `F_GETFL` reporting an access mode as well. It reads as more complete -- a real `F_GETFL`
    # does return one -- and this seam does not record which mode a descriptor was opened for, so
    # the value would be a guess the guest branches on.
    ("pipe-B3", "B", "F_GETFL invents an access mode",
     ADAPTER_FILES,
     """                Settled::Done(false) => 0,""",
     """                Settled::Done(false) => O_ACCMODE,""",
     ANDROID),

    # Reading the write end answered as end of file instead of `EBADF`. It reads as the gentler
    # answer and it tells a guest that used the wrong end of its own pipe that the data is gone.
    ("pipe-B4", "B", "reading the write end of a pipe is end of file rather than EBADF",
     PLAT_FS_PIPE,
     """        if self.end != PipeEnd::Read {
            return Err(FsError::kinded(
                "read",""",
     """        if self.end != PipeEnd::Read {
            return Ok(0);
        }
        if false {
            return Err(FsError::kinded(
                "read",""",
     PLATFORM),

    # ================================================================== M5: the ALooper
    #
    # §8.1's **fourth** failure mode is that `ALooper_forThread()` returning NULL makes
    # `initializeNativeCode` return 0 and Java-side startup fail silently. Every row here is about
    # one of the ways this layer could produce that silently, or produce a looper that answers
    # something a device would not.

    # The null that IS the answer, turned into a non-null. §5.2 decodes the constructor logging
    # "Unable to retrieve native ALooper" and returning zero on exactly this, so a layer that
    # always produced a looper would make the host's own precondition untestable.
    ("looper-A1", "A", "ALooper_forThread answers something rather than NULL when there is none",
     NDK_LOOPER,
     """    c.ret().u64(found.unwrap_or(0) as u64);""",
     """    c.ret().u64(found.unwrap_or(1) as u64);""",
     ANDROID),

    # Callbacks run before an ident is reported. AOSP reports idents first, and the glue depends
    # on it: `android_app_entry` registers its command pipe with LOOPER_ID_MAIN and no callback,
    # and `GameLoop` switches on the return.
    ("looper-A2", "A", "a callback is run where an ident should have been reported",
     NDK_LOOPER,
     """            match ident {
                Some(found) => found,
                None if callbacks.is_empty() => Pass::Idle,
                None => Pass::Callbacks(callbacks),
            }""",
     """            match ident {
                _ if !callbacks.is_empty() => Pass::Callbacks(callbacks),
                Some(found) => found,
                None => Pass::Idle,
            }""",
     ANDROID),

    # A registration with a callback keeping the caller's ident. §5.2's constructor passes
    # `ident = 0` **and** a callback, so this makes `pollOnce` report ident 0 -- a legal-looking
    # answer the glue has no branch for.
    ("looper-A3", "A", "a callback registration keeps an ident that can never be reported",
     NDK_LOOPER,
     """        ident: if callback == 0 { ident } else { ALOOPER_POLL_CALLBACK },""",
     """        ident,""",
     ANDROID),

    # A callback returning zero no longer removes its registration, which is the NDK's documented
    # contract and the mechanism by which the glue detaches its pipe.
    ("looper-A4", "A", "a callback returning zero does not remove its registration",
     NDK_LOOPER,
     """                if returned == 0 {""",
     """                if false && returned == 0 {""",
     ANDROID),

    # The last release no longer destroys the looper, so the thread keeps one for ever and
    # `ALooper_forThread` can never answer NULL again -- which removes the very condition §8.1's
    # fourth failure mode is about.
    ("looper-A5", "A", "the last release leaves the looper alive",
     NDK_LOOPER,
     """    if references == 0 {
        // The last reference""",
     """    if false {
        // The last reference""",
     ANDROID),

    # A descriptor the instance does not hold accepted into a looper. Its readiness would then
    # have to be invented, which is the whole reason the check is there.
    ("looper-A6", "A", "a looper accepts a descriptor this runtime does not have",
     NDK_LOOPER,
     """        if !fs.is_open(fd) {""",
     """        if false {""",
     ANDROID),

    # `pollOnce` returns the right ident and writes no `outFd`. A caller that read the stale value
    # acts on whatever descriptor was there last.
    ("looper-A7", "A", "pollOnce reports an ident and writes no out-parameter",
     NDK_LOOPER,
     """            if out_fd != 0 {""",
     """            if false {""",
     ANDROID),

    # The wait removed: `pollOnce` answers POLL_TIMEOUT immediately for any timeout. A game loop
    # would spin at whatever rate the run budget allowed instead of waiting for its pipe.
    ("looper-A8", "A", "pollOnce never waits, and times out at once",
     NDK_LOOPER,
     """        let now = Instant::now();
        if now >= deadline {
            break Pass::Idle;
        }""",
     """        let now = Instant::now();
        if true {
            break Pass::Idle;
        }""",
     ANDROID),

    # ---- the over-corrections ----

    # `ALooper_prepare` taking a reference for its caller as well as the thread's. It reads as the
    # careful thing -- the caller has a pointer, so surely it holds a reference -- and it leaves
    # the count one too high for ever, so the looper outlives the thread that owns it.
    ("looper-B1", "B", "prepare takes a reference for the caller as well as the thread",
     NDK_LOOPER,
     """        Looper { thread, opts, references: 1, fds: Vec::new() }""",
     """        Looper { thread, opts, references: 2, fds: Vec::new() }""",
     ANDROID),

    # The indefinite-wait refusal widened to every non-positive timeout. It reads as stricter, and
    # `ALooper_pollOnce(0, ..)` is the ordinary non-blocking poll a game loop makes every frame.
    ("looper-B2", "B", "a zero timeout is refused along with an indefinite one",
     NDK_LOOPER,
     """    let budget = if timeout_millis < 0 {""",
     """    let budget = if timeout_millis <= 0 {""",
     ANDROID),

    # A second `addFd` for one descriptor keeping both registrations. It reads as losing nothing,
    # and it reports one descriptor twice -- so a `pollOnce` that should answer one ident answers
    # a callback as well.
    ("looper-B3", "B", "a second addFd for one descriptor keeps both registrations",
     NDK_LOOPER,
     """    entry.fds.retain(|held| held.fd != fd);
    if entry.fds.len() >= MAX_LOOPER_FDS {""",
     """    if entry.fds.len() >= MAX_LOOPER_FDS {""",
     ANDROID),

    # ---- the park witness, for §8 row 14 ----

    # The guard stops removing its entry. A stale park makes a run that finished look like the
    # deadlock the witness exists to find, which is worse than having no witness at all.
    ("park-A1", "A", "a thread that finished waiting stays recorded as parked",
     ADAPTER_MOD,
     """        if let Some(index) = parked.iter().position(|held| held.started == self.token) {
            parked.remove(index);
        }""",
     """        if let Some(index) = parked.iter().position(|held| held.started == self.token) {
            let _ = index;
        }""",
     ANDROID),

    # The witness records the mutex where the condition variable belongs. **A substitution, not a
    # count**: the number of parked threads is right, every total is right, and the object named
    # is the wrong one -- which is this project's first verification lesson, applied to its own
    # newest instrument.
    # ============================================ M5: the Win32 device set, measured rather than listed
    #
    # Review finding M6 said `WINDOWS_DEVICES` omitted `COM0`/`LPT0` and the superscript forms.
    # **Half of that was wrong**, and the measurement is what settled it: `COM0` and `LPT0` are
    # ordinary files on this build (`RtlIsDosDeviceName_U` answers zero for both, and
    # `CreateFileW("COM0")` created a file the directory listing then showed), while `CONIN$`,
    # `CONOUT$` and the six U+00B9/B2/B3 forms are real devices and really were missing. So there
    # is an A row for each half that was missing and a **B row for believing the finding as
    # filed** -- adding `COM0`/`LPT0` is the over-correction, and it now fails.

    # The console buffers, which the list did not have. `CONIN$` and `CONOUT$` are openable by
    # name and are how a guest reaches the console rather than a file under the root.
    ("confine-A1", "A", "the console-buffer device names are dropped from the list",
     PLAT_FS_PATH,
     """    // The console buffers, openable by name — the first half of M6.
    "CONIN$", "CONOUT$",""",
     """    // The console buffers, openable by name — the first half of M6.""",
     PLATFORM),

    # The six superscript forms. MEASURED: only U+00B9/B2/B3 match, out of twenty-six digit
    # look-alikes tried -- so this is a three-member special case rather than a "Unicode digit"
    # rule, and a list that drops them lets the superscript spellings through as ordinary names.
    ("confine-A2", "A", "the superscript COM/LPT device forms are dropped from the list",
     PLAT_FS_PATH,
     r"""    "COM\u{b9}", "COM\u{b2}", "COM\u{b3}", "LPT\u{b9}", "LPT\u{b2}", "LPT\u{b3}",
];""",
     """];""",
     PLATFORM),

    # The extension no longer stripped, so `NUL.txt` reaches the host. MEASURED as *not* a device
    # on this build -- and refused anyway, because the confinement property must not depend on a
    # Windows build number and an over-refusal cannot create an escape.
    ("confine-A3", "A", "a device name with an extension is no longer recognised",
     PLAT_FS_PATH,
     """    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');""",
     """    let stem = name;""",
     PLATFORM),

    # The trailing space no longer trimmed, so `NUL .txt` passes. One strip away from a device.
    ("confine-A4", "A", "a device name with a trailing space is no longer recognised",
     PLAT_FS_PATH,
     """    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');""",
     """    let stem = name.split('.').next().unwrap_or(name);""",
     PLATFORM),

    # **Believing review finding M6 as it was filed.** It asked for `COM0`/`LPT0`; both measured
    # as ordinary files. Adding them costs a guest two filenames it is entitled to, which is the
    # over-refusal direction -- safe for confinement and wrong as a statement about the host.
    ("confine-B1", "B", "COM0 and LPT0 are refused, which the measurement says are files",
     PLAT_FS_PATH,
     """const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",""",
     """const WINDOWS_DEVICES: [&str; 32] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL", "COM0", "LPT0",""",
     PLATFORM),

    # Generalising the three superscripts into an "any digit look-alike" rule, by adding the
    # fullwidth form. MEASURED not to be a device; twenty-six were tried and only three matched.
    ("confine-B2", "B", "the superscript special case is generalised to another digit look-alike",
     PLAT_FS_PATH,
     """const WINDOWS_DEVICES: [&str; 30] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL",""",
     r"""const WINDOWS_DEVICES: [&str; 31] = [
    // The four classic character devices.
    "CON", "PRN", "AUX", "NUL", "COM\u{ff11}",""",
     PLATFORM),

    # `cpu_count` back to substituting a believable `1`, which is review finding M7's own shape:
    # the pattern `random_bytes` forbids eleven lines below it in the same file.
    ("confine-A5", "A", "cpu_count substitutes a believable 1 instead of reporting failure",
     PLAT_PROCESS_MOD,
     """    std::thread::available_parallelism().map_err(|error| ProcessError::Indeterminate {
        operation: "cpu_count",
        detail: error.to_string(),
    })""",
     """    Ok(std::num::NonZeroUsize::new(1).expect("one is not zero"))""",
     PLATFORM),

    # And the over-correction in the other direction: a fabricated `Unsupported` for a primitive
    # that is one portable `std` call on all five targets, which `lib.rs` forbids by name.
    ("confine-B3", "B", "cpu_count claims to be unsupported where std answers on every target",
     PLAT_PROCESS_MOD,
     """    std::thread::available_parallelism().map_err(|error| ProcessError::Indeterminate {
        operation: "cpu_count",
        detail: error.to_string(),
    })""",
     """    Err(ProcessError::Unsupported {
        operation: "cpu_count",
        intended: "sysconf(_SC_NPROCESSORS_ONLN)",
        platform: "this target",
    })""",
     PLATFORM),

    ("park-A2", "A", "the park witness names the mutex as the condition variable",
     ADAPTER_HANDLERS,
     """    let _parked = state.bionic.park("pthread_cond_wait", state.thread, cond, mutex);""",
     """    let _parked = state.bionic.park("pthread_cond_wait", state.thread, mutex, mutex);""",
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

    # **Row ids must be unique, and nothing used to check.** Six rows added for M3 task 3 phase 3a
    # were filed under `plat-*`, and four of them collided with the fault handler's existing
    # `plat-A1`..`plat-A4`. Nothing complained: a full run still touched every row, so the totals
    # were right, but `--only plat-A1` selected two different mutations and a report naming a row
    # id no longer identified one. That is the count-cannot-see-a-substitution failure this project
    # has already been bitten by, in the harness that exists to catch it.
    seen = {}
    collisions = []
    for row in MUTATIONS:
        if row[0] in seen:
            collisions.append(f"  {row[0]}: {seen[row[0]]}  AND  {row[2]}")
        seen[row[0]] = row[2]
    if collisions:
        print(f"{len(collisions)} duplicate mutation id(s). Nothing was run.")
        print(chr(10).join(collisions))
        return 2

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

    # Pre-flight 2: **every command must PASS on the unmutated tree.**
    #
    # This exists because it did not, and the hole is the worst one a mutation harness can have: a
    # command that already fails reports every row that uses it as `caught`, because "the suite
    # failed" is the whole of what `caught` means here. Eight `time-*` rows were reported 8/8
    # caught that way -- `gmtime(i64::MIN)` panicked with an arithmetic overflow in a **debug**
    # build, which is what this harness runs, while the whole-workspace suite runs `--release` and
    # wrapped silently instead. The defect was real and is fixed; the eight "caught"s were worth
    # nothing until it was.
    #
    # One run per distinct command rather than per row, so a full table costs a handful of extra
    # runs rather than two hundred.
    commands = []
    for row in selected:
        if row[6] not in commands:
            commands.append(row[6])
    for command in commands:
        code, output, seconds = run(command)
        if code != 0:
            print(f"pre-flight failed: `{' '.join(command)}` does not pass on the unmutated tree "
                  f"({seconds}s). Nothing was mutated. Every row using this command would have "
                  f"been reported `caught` whatever its mutation did.")
            for name in failing_tests(output):
                print(f"  {name}")
            return 2
    print(f"pre-flight: {len(commands)}/{len(commands)} commands pass on the unmutated tree")

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
                # **Retried once, and the retry is the point.** MEASURED: one 298-row run produced
                # two of these and BOTH compiled fine afterwards -- `mem-A18` logged
                # "did not compile (2s)" where its real build and suite take 23s, and `varargs-A8`
                # the same. Two seconds is not a compile; it is a cargo lock or a filesystem race.
                #
                # A transient build failure is indistinguishable from a genuinely non-compiling
                # mutation at this point, and filing it as MISS sends somebody to investigate a row
                # that is fine -- the mirror of the misclassification the comment above records. A
                # mutation that truly does not compile fails twice; a race does not.
                code, output, retry_seconds = run(command)
                seconds += retry_seconds
                caught = failing_tests(output)
                if code == 0:
                    status, detail = "NOT CAUGHT", "every test still passed (build retried)"
                elif not caught and ("error[" in output or "could not compile" in output):
                    status, detail = "MISS", "did not compile, twice"
                elif caught:
                    status = "caught"
                    detail = f"{len(caught)} test(s), after a build retry: " + ", ".join(caught[:3])
                    if len(caught) > 3:
                        detail += f", +{len(caught) - 3} more"
                else:
                    status, detail = "caught", "the suite failed after a build retry"
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
