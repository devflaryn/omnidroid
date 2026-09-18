"""Mutation testing across the workspace, table-driven.

    python tools/mutate.py               # from the repository root
    python tools/mutate.py --only mem    # one prefix
    python tools/mutate.py --list

Global Constraint 12: a test that does not fail when the logic it covers is reverted is not
evidence. `crates/omni-elf/tools/mutate_loader.py` made that checkable for the loader; this is the
generalisation the whole-branch review asked for — it takes a table of (file, old, new, command)
rather than being loader-shaped, so a new fix anywhere in the workspace costs one table row.

Each mutation is applied on its own, the named test command is run, the result is recorded, and the
file is **always** restored, including on a crash, via `try`/`finally`. A mutation that does not
compile, does not match its pattern, or is caught by nothing is reported as `MISS`, never as a pass:
a mutation nothing notices means the fix has no test behind it.

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

# Commands, kept narrow so the whole run stays under a few minutes.
MEM = ["cargo", "test", "-p", "omni-mem", "--no-fail-fast"]
CPU = ["cargo", "test", "-p", "omni-cpu", "--no-fail-fast"]
PLATFORM = ["cargo", "test", "-p", "omni-platform", "--no-fail-fast"]
ELF = ["cargo", "test", "-p", "omni-elf", "--no-fail-fast"]
APK = ["cargo", "test", "-p", "omni-apk", "--no-fail-fast"]
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
     """        let _sealed_guard = if self.sealed_pages.load(Ordering::Acquire) != 0 {""",
     """        let _sealed_guard = if false && self.sealed_pages.load(Ordering::Acquire) != 0 {""",
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
                status, detail = "caught", "the suite failed without naming a test"
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
