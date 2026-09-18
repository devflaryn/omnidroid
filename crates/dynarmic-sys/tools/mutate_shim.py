"""Mutation testing for the dynarmic C shim and its callback discipline.

    python crates/dynarmic-sys/tools/mutate_shim.py          # from the repository root
    python crates/dynarmic-sys/tools/mutate_shim.py --list

A sibling of `tools/mutate.py` rather than rows in it, for two reasons. The
mutated files are C++ and test-harness Rust, not workspace crates, so the
rebuild path is different; and several of these mutations make the process
**abort** or **hang** rather than fail a test, which needs a timeout the
workspace runner does not have.

Global Constraint 12: a test that does not fail when the logic it covers is
reverted is not evidence. Both directions are here, and the second is the point:

* **A** reverts a guard. Something must fail.
* **B** over-corrects -- substitutes a safer-looking default, clamps instead of
  refusing, invalidates more than asked. These read as improvements and destroy
  a measured property. D4's 13.2x fastmem cliff is exactly this shape: correct
  results, 13x slower, invisible to every functional test.

Statuses: `caught` (a test failed, the process aborted, or the suite hung),
`NOT CAUGHT` (everything still passed -- the fix has no test behind it), and
`MISS` (the pattern did not match, or the mutant did not compile).
"""

import argparse
import os
import subprocess
import sys
import tempfile
import time

SHIM = "crates/dynarmic-sys/shim/od_dynarmic.cpp"
HARNESS = "crates/dynarmic-sys/tests/harness/mod.rs"

# `--no-fail-fast` is not optional: without it cargo stops after the first
# failing binary and attributes every mutation to whichever ran first.
#
# `--target-dir` is not optional either. The workspace's `target/` may be held
# by another cargo, and a mutation run that spends its time blocked on a build
# lock takes hours instead of minutes. The extra nesting costs path budget, so
# the CMake build directory moves somewhere short at the same time -- which is
# what OMNIDROID_DYNARMIC_BUILD_DIR is for.
SYS = ["cargo", "test", "-p", "dynarmic-sys", "--no-fail-fast",
       "--target-dir", "target/mut"]
SYS_ENV = {"OMNIDROID_DYNARMIC_BUILD_DIR":
           os.path.join(tempfile.gettempdir(), "od-dyn")}

# A mutant that wedges a host thread is caught, not hung forever. The suite
# takes about 15 s clean, and the C++ shim rebuild adds a few more.
TIMEOUT_SECONDS = 240

# Suffix for the pristine copy kept on disk while a mutation is applied, so a
# run that is killed outright can be recovered from rather than leaving a
# mutant in the tree.
BACKUP_SUFFIX = ".mutate-orig"

# How long to let a previous run's dying processes release the test binary
# before retrying a build that failed to link.
SETTLE_SECONDS = 8

# (id, direction, description, file, old, new, command)
MUTATIONS = [
    # ---- the re-entrancy guard: guest code reaching a callback that calls back in ----------------
    ("sys-A1", "A", "od_jit_run re-entry guard removed (dynarmic asserts, which terminates)", SHIM,
     """    if (self->jit->IsExecuting()) {
        return OD_HALT_SHIM_REENTERED;
    }
    try {
        return static_cast<uint32_t>(self->jit->Run());""",
     """    try {
        return static_cast<uint32_t>(self->jit->Run());""",
     SYS),

    ("sys-A2", "A", "od_jit_step re-entry guard removed", SHIM,
     """    if (self->jit->IsExecuting()) {
        return OD_HALT_SHIM_REENTERED;
    }
    try {
        return static_cast<uint32_t>(self->jit->Step());""",
     """    try {
        return static_cast<uint32_t>(self->jit->Step());""",
     SYS),

    # ---- configuration that dynarmic asserts on rather than refusing ----------------------------
    ("sys-A3", "A", "ABI version no longer checked", SHIM,
     """    if (config == nullptr || config->abi_version != OD_DYNARMIC_ABI_VERSION) {""",
     """    if (config == nullptr || false) {""",
     SYS),

    ("sys-A4", "A", "a callback table with a null slot is accepted again", SHIM,
     """    if (config->callbacks == nullptr || !callbacks_complete(config->callbacks)) {""",
     """    if (config->callbacks == nullptr) {""",
     SYS),

    ("sys-A5", "A", "fastmem_address_space_bits range check removed", SHIM,
     """    if (config->fastmem_enabled
        && (config->fastmem_address_space_bits < 12 || config->fastmem_address_space_bits > 64)) {
        return nullptr;
    }""",
     """    if (false) {
        return nullptr;
    }""",
     SYS),

    ("sys-A6", "A", "code_cache_size range check removed", SHIM,
     """        if (config->code_cache_size < min_cache || config->code_cache_size > max_cache) {
            return nullptr;
        }""",
     """        if (false) {
            return nullptr;
        }""",
     SYS),

    ("sys-A7", "A", "processor_id no longer checked against the monitor's size", SHIM,
     """        if (static_cast<std::size_t>(config->processor_id) >= mon->GetProcessorCount()) {
            return nullptr;
        }""",
     """        if (false) {
            return nullptr;
        }""",
     SYS),

    ("sys-A8", "A", "monitor processor_count bound removed", SHIM,
     """    if (processor_count == 0 || processor_count > 4096) {""",
     """    if (false) {""",
     SYS),

    # ---- degenerate arguments dynarmic builds inverted intervals from --------------------------
    ("sys-A9", "A", "zero-length invalidation halts the guest for an empty range", SHIM,
     """    if (len == 0) {""",
     """    if (false) {""",
     SYS),

    ("sys-A10", "A", "invalidation length overflow no longer clamped (invalidates nothing)", SHIM,
     """    if (len - 1 > room) {
        len = room + 1;
    }""",
     """    if (false) {
        len = room + 1;
    }""",
     SYS),

    # `sys-A10` proves the clamp is load-bearing. It does not prove the clamp is
    # correct, and it was not: this is the arithmetic that shipped, and it turns
    # a four-byte invalidation at guest address 0 into a whole-cache flush.
    ("sys-A17", "A", "the clamp written so it overflows at addr 0 (the original defect)", SHIM,
     """    const uint64_t room = std::numeric_limits<uint64_t>::max() - addr;
    if (len - 1 > room) {
        len = room + 1;
    }""",
     """    const uint64_t max_len = std::numeric_limits<uint64_t>::max() - addr + 1;
    if (len > max_len) {
        len = max_len;
    }""",
     SYS),

    # ---- register access, which dynarmic indexes unchecked --------------------------------------
    ("sys-A11", "A", "od_jit_get_reg index bound removed", SHIM,
     """uint64_t od_jit_get_reg(void* p, uint32_t index) {
    if (index > 30) {
        return 0;
    }""",
     """uint64_t od_jit_get_reg(void* p, uint32_t index) {
    if (false) {
        return 0;
    }""",
     SYS),

    ("sys-A12", "A", "od_jit_get_vec index bound removed", SHIM,
     """    if (index > 31) {
        out[0] = 0;
        out[1] = 0;
        return;
    }""",
     """    if (false) {
        out[0] = 0;
        out[1] = 0;
        return;
    }""",
     SYS),

    # ---- the counters P2 exists for ------------------------------------------------------------
    ("sys-A13", "A", "slow-path reads no longer counted", SHIM,
     """    u64 MemoryRead64(u64 v) override {
        stats.slow_path_reads++;
        stats.slow_path_total++;""",
     """    u64 MemoryRead64(u64 v) override {
        stats.slow_path_total++;""",
     SYS),

    ("sys-A14", "A", "slow_path_total never incremented by writes", SHIM,
     """    void MemoryWrite64(u64 v, u64 x) override {
        stats.slow_path_writes++;
        stats.slow_path_total++;""",
     """    void MemoryWrite64(u64 v, u64 x) override {
        stats.slow_path_writes++;""",
     SYS),

    # ---- the panic discipline -------------------------------------------------------------------
    ("sys-A15", "A", "callbacks no longer catch panics (an unwind through JIT frames aborts)",
     HARNESS,
     """    let result = catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *raw })));""",
     """    let result: Result<R, Box<dyn std::any::Any + Send>> =
        Ok(f(unsafe { &mut *raw }));""",
     SYS),

    ("sys-A16", "A", "a panicking callback no longer halts the guest", HARNESS,
     """                    od_jit_halt(jit, HALT_PANIC);""",
     """                    let _ = HALT_PANIC;""",
     SYS),

    # ---- direction B: over-corrections that read as improvements ---------------------------------
    ("sys-B1", "B", "fastmem_address_space_bits forced to dynarmic's default of 36", SHIM,
     """            uc.fastmem_address_space_bits = static_cast<std::size_t>(config->fastmem_address_space_bits);""",
     """            uc.fastmem_address_space_bits = 36;""",
     SYS),

    ("sys-B2", "B", "fastmem disabled unconditionally, because callbacks are 'safer'", SHIM,
     """        if (config->fastmem_enabled) {""",
     """        if (false) {""",
     SYS),

    ("sys-B3", "B", "code_cache_size clamped into range instead of refused", SHIM,
     """        if (config->code_cache_size < min_cache || config->code_cache_size > max_cache) {
            return nullptr;
        }""",
     """        if (config->code_cache_size < min_cache || config->code_cache_size > max_cache) {
            const_cast<od_config*>(config)->code_cache_size =
                config->code_cache_size < min_cache ? min_cache : max_cache;
        }""",
     SYS),

    ("sys-B4", "B", "a range invalidation throws away every translation, to be safe", SHIM,
     """    as_jit(p)->jit->InvalidateCacheRange(addr, static_cast<std::size_t>(len));""",
     """    (void)addr;
    (void)len;
    as_jit(p)->jit->ClearCache();""",
     SYS),

    ("sys-B5", "B", "an out-of-range register index is clamped rather than refused", SHIM,
     """uint64_t od_jit_get_reg(void* p, uint32_t index) {
    if (index > 30) {
        return 0;
    }""",
     """uint64_t od_jit_get_reg(void* p, uint32_t index) {
    if (index > 30) {
        index = 30;
    }""",
     SYS),

    # Not `= 64` or any other literal: a wrong constant is a different and much
    # less interesting mutation. What is worth catching is the effective config
    # reporting something other than the state that was actually installed --
    # here by never recording it, so it answers from the struct's defaults
    # (fastmem off, 36 bits, 128 MiB) while dynarmic runs with what was asked
    # for. That is D4's silent-substitution shape pointed at the instrument
    # rather than at the engine.
    #
    # Note what this cannot reach: a shim that echoed the caller's own
    # `od_config` would be indistinguishable by any test, because that struct
    # and the saved `UserConfig` hold the same values by construction. The
    # equivalence rests on dynarmic's own copy being `const UserConfig conf`
    # (`a64_interface.cpp:317`) for the jit's whole life, which is stated where
    # the function is defined and would have to be rechecked on a re-pin.
    ("sys-B6", "B", "the effective config is never recorded, so it reports struct defaults",
     SHIM,
     """        self->conf = uc;
        self->jit = new A64::Jit{uc};""",
     """        self->jit = new A64::Jit{uc};""",
     SYS),
]


def kill_tree(proc):
    """Kill the whole tree, not just cargo.

    A mutant that wedges a host thread leaves a *test binary* spinning, and on
    Windows killing the parent leaves it orphaned -- holding the build lock,
    and holding up every mutation after it.
    """
    if os.name == "nt":
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                       capture_output=True, check=False)
    else:
        proc.kill()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        pass


def run(command):
    """Run `command`, capturing to a file rather than a pipe.

    A pipe would be inherited by every descendant, and a *wedged orphan* keeps
    it open after its parent has been killed -- so reading it would block
    forever, which is exactly the hang this exists to survive.
    """
    started = time.time()
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as sink:
        env = dict(os.environ)
        env.update(SYS_ENV)
        proc = subprocess.Popen(command, stdout=sink, stderr=subprocess.STDOUT, env=env)
        try:
            proc.wait(timeout=TIMEOUT_SECONDS)
            code = proc.returncode
        except subprocess.TimeoutExpired:
            kill_tree(proc)
            code = None
        sink.seek(0)
        return code, sink.read(), time.time() - started


def backup_path(path):
    return path + BACKUP_SUFFIX


def restore_stale_backups():
    """Put back anything a previous run was killed in the middle of.

    `try`/`finally` restores the file on a crash, but not when the interpreter
    is killed outright -- and a run that hangs gets killed outright. A mutant
    left behind in the tree is silent: it compiles, and it surfaces later as
    some unrelated test failing for no visible reason. So the original goes to
    disk before each mutation and is removed after, and a leftover is recovered
    here.
    """
    for _, _, _, path, _, _, _ in MUTATIONS:
        backup = backup_path(path)
        if os.path.exists(backup):
            with open(backup, encoding="utf-8") as handle:
                original = handle.read()
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(original)
            os.remove(backup)
            print(f"recovered {path} from an interrupted run")


def looks_like_a_build_failure(output):
    return "error[" in output or "could not compile" in output


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
            print(f"{mid:<8} {direction}  {description}  [{path}]")
        return 0

    restore_stale_backups()

    # Pre-flight: every selected pattern must match its file exactly once
    # *before* anything is mutated or any cargo is run. `tools/mutate.py` grew
    # this after a stale row surfaced as a MISS forty minutes into a run. The
    # same thing happened here twice -- once because a comment the pattern
    # quoted had been reworded, once because a run killed mid-flight left a
    # mutant behind. The per-row check below is still needed, because a row can
    # go stale between this pass and its turn; this is the one-second version
    # that says so before the wait rather than after it.
    stale = []
    for mid, _, _, path, old, _, _ in selected:
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        found = text.count(old)
        if found != 1:
            stale.append(f"  {mid}: pattern matches {found} times in {path}")
    if stale:
        print(f"pre-flight failed: {len(stale)} of {len(selected)} patterns do not "
              "match exactly once. Nothing was mutated and nothing was run.")
        print("\n".join(stale))
        return 2
    print(f"pre-flight: {len(selected)}/{len(selected)} patterns match exactly once")

    print(f"{len(selected)} mutations\n")
    results = []
    for mid, direction, description, path, old, new, command in selected:
        with open(path, encoding="utf-8") as handle:
            original = handle.read()
        if old not in original:
            print(f"{mid:<8} MISS       pattern not found in {path}")
            results.append((mid, direction, description, "MISS", "pattern not found"))
            continue
        if original.count(old) != 1:
            print(f"{mid:<8} MISS       pattern is not unique in {path}")
            results.append((mid, direction, description, "MISS", "pattern not unique"))
            continue
        try:
            with open(backup_path(path), "w", encoding="utf-8") as handle:
                handle.write(original)
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(original.replace(old, new))
            code, output, seconds = run(command)
            caught = failing_tests(output)
            if code not in (None, 0) and not caught and looks_like_a_build_failure(output):
                # A mutation whose predecessor aborted the test process can
                # find the previous binary still open: on Windows a dying
                # process's children keep the .exe locked for a moment, and the
                # link fails with a permission error rather than a compile
                # error. Give it room and try once more before calling it a
                # MISS, which would otherwise hide a perfectly good mutation.
                time.sleep(SETTLE_SECONDS)
                code, output, seconds = run(command)
                caught = failing_tests(output)
            if code is None:
                status = "caught"
                detail = f"the suite hung (>{TIMEOUT_SECONDS}s)"
            elif code == 0:
                status, detail = "NOT CAUGHT", "every test still passed"
            elif not caught and looks_like_a_build_failure(output):
                status, detail = "MISS", "did not compile"
            elif caught:
                status = "caught"
                detail = f"{len(caught)} test(s): " + ", ".join(caught[:3])
                if len(caught) > 3:
                    detail += f", +{len(caught) - 3} more"
            else:
                status = "caught"
                detail = "the suite failed without naming a test (abort or signal)"
            print(f"{mid:<8} {status:<10} {description} -> {detail} ({seconds:.0f}s)")
            results.append((mid, direction, description, status, detail))
        finally:
            with open(path, "w", encoding="utf-8") as handle:
                handle.write(original)
            if os.path.exists(backup_path(path)):
                os.remove(backup_path(path))

    print()
    caught = sum(1 for r in results if r[3] == "caught")
    print(f"{caught}/{len(results)} caught")
    for mid, direction, description, status, detail in results:
        if status != "caught":
            print(f"  {status}: {mid} {direction} {description} ({detail})")
    return 0 if caught == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
