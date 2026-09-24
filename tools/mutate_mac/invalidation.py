"""macOS rows: dynarmic patch 0015 (an invalidation walks only the chunks holding translated code).
Pure data; see `__init__.py`. Vendored: the command touches `vendor/PIN.txt` first."""

ASPACE = "crates/dynarmic-sys/vendor/dynarmic/src/dynarmic/backend/arm64/a64_address_space.cpp"
TESTS = ["sh", "-c",
         "touch crates/dynarmic-sys/vendor/PIN.txt && CARGO_BUILD_JOBS=4 cargo test --release "
         "-p dynarmic-sys --test invalidation_probes --test bookkeeping --no-fail-fast "
         "-- --test-threads=1"]

ROWS = [
    ("mac-cpu-I1", "A", "every chunk an invalidation covers is walked page by page (patch 0015's "
     "filter off): a heap munmap costs a lookup per 4 KiB page on every guest thread",
     ASPACE,
     """                if (guest_range_chunks.contains(chunk)) {
                    walk_chunk(chunk);
                }""",
     """                walk_chunk(chunk);""",
     TESTS),
    ("mac-cpu-I2", "A", "a translated page's chunk is not recorded: an invalidation over real code "
     "finds nothing and a stale translation keeps running",
     ASPACE,
     """        guest_range_chunks.insert(page >> (guest_chunk_bits - guest_page_bits));  // patch 0015""",
     """        (void)page;  // patch 0015 mutated""",
     TESTS),
]
