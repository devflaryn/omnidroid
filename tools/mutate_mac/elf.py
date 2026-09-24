"""macOS rows: the ELF loader on a 16 KiB host (omni-elf). Pure data; see `__init__.py`."""

LOADER = "crates/omni-elf/src/loader/mod.rs"
LOADER_TESTS = [
    "cargo", "test", "-p", "omni-elf", "--release", "--test", "loader_m1", "--test", "loader_hostile",
    "--no-fail-fast",
]

ROWS = [
    ("mac-elf-A1", "A", "the relro end is rounded down, not up as bionic's page_end does: on a 16 KiB "
     "host libroblox.so's .got/.got.plt page stays writable",
     LOADER,
     """    let end_page = plan::page_up(end_vaddr, page).ok_or(LoadError::RelroOutsideImage {""",
     """    let end_page = Some(plan::page_down(end_vaddr, page)).ok_or(LoadError::RelroOutsideImage {""",
     LOADER_TESTS),
]
