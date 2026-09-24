"""macOS rows: dynarmic patches 0014 (the store-exclusive is a fastmem patch location too) and 0021
(the inline exclusives honour `Unsafe_IgnoreGlobalMonitor`, D31 amendment 1). Pure data;
see `__init__.py`. Its own module so the cpu, memory and native-backend tables merge untouched.

Vendored: the command touches `vendor/PIN.txt` first (see `cpu.py`'s note), and after a run the tree
needs another touch before any test binary is trusted.
"""

MEM = "crates/dynarmic-sys/vendor/dynarmic/src/dynarmic/backend/arm64/emit_arm64_memory.cpp"
TESTS = ["sh", "-c",
         "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test --release "
         "-p dynarmic-sys --test host_fault -p omni-cpu --test exclusive_store_fault --no-fail-fast "
         "-- --test-threads=1"]
LOCK_TEST = ["sh", "-c",
             "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test --release "
             "-p dynarmic-sys --test exclusive --no-fail-fast -- --test-threads=1 --exact "
             "value_compare_inline_exclusives_neither_take_nor_release_the_monitor_lock"]
VALUE_COMPARE_TESTS = ["sh", "-c",
                       "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test --release "
                       "-p dynarmic-sys --test exclusive -p omni-cpu --test exclusive --no-fail-fast "
                       "-- --test-threads=1"]

ROWS = [
    ("mac-cpu-E1", "A", "the store-release of the inline store-exclusive is not a patch location "
     "(patch 0014 reverted): a write fault after a successful load aborts the process",
     MEM,
     """        // Patch 0014: the store-release, with the same entry -- at it the lock is held and, for 128
        // bits, the borrowed registers are on the stack, exactly as at the load-acquire.
        ctx.ebi.fastmem_patch_info.emplace(
            store_location - ctx.ebi.entry_point,""",
     """        // Patch 0014 reverted (mutation mac-cpu-E1).
        (void)store_location;
        if (false) ctx.ebi.fastmem_patch_info.emplace(
            store_location - ctx.ebi.entry_point,""",
     TESTS),
    # --- patch 0021: value-compare honoured on arm64 (D31 amendment 1) -------------------------------
    # V1 and V2 run only the lock test, `--exact`: with V1 a value-compare exclusive takes the lock
    # and never gives it back, so every other test that runs two exclusives would spin for ever (this
    # harness has no timeout); the lock test bounds its own wait and fails.
    ("mac-cpu-V1", "A", "0021's lock check removed: a value-compare exclusive takes the monitor lock "
     "(and, its unlock still skipped, keeps it)",
     MEM,
     """void EmitMonitorLock(oaknut::CodeGenerator& code, EmitContext& ctx) {
    if (IgnoresGlobalMonitor(ctx)) {
        return;
    }
""",
     """void EmitMonitorLock(oaknut::CodeGenerator& code, EmitContext& ctx) {
""",
     LOCK_TEST),
    ("mac-cpu-V2", "A", "0021's unlock check removed: a value-compare exclusive releases a lock it "
     "never took (another holder's, a callback-path thread's)",
     MEM,
     """void EmitMonitorUnlock(oaknut::CodeGenerator& code, EmitContext& ctx) {
    if (IgnoresGlobalMonitor(ctx)) {
        return;
    }
""",
     """void EmitMonitorUnlock(oaknut::CodeGenerator& code, EmitContext& ctx) {
""",
     LOCK_TEST),
    ("mac-cpu-V3", "A", "0021's scan check removed: a value-compare store-exclusive still clears every "
     "processor's reservation (the ABA case fails, as on the pin)",
     MEM,
     """    if (!IgnoresGlobalMonitor(ctx)) {
        const u64 first""",
     """    {
        const u64 first""",
     VALUE_COMPARE_TESTS),
]
