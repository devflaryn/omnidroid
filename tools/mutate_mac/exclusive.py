"""macOS rows: dynarmic patch 0014 (the store-exclusive is a fastmem patch location too). Pure data;
see `__init__.py`. Its own module so the cpu, memory and native-backend tables merge untouched.

Vendored: the command touches `vendor/PIN.txt` first (see `cpu.py`'s note), and after a run the tree
needs another touch before any test binary is trusted.
"""

MEM = "crates/dynarmic-sys/vendor/dynarmic/src/dynarmic/backend/arm64/emit_arm64_memory.cpp"
TESTS = ["sh", "-c",
         "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test --release "
         "-p dynarmic-sys --test host_fault -p omni-cpu --test exclusive_store_fault --no-fail-fast "
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
]
