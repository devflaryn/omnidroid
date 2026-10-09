//! **Native fast paths for hot `libc.so` functions** (`OMNI_JIT_HLE=1`, lever `jit_hle=1`).
//!
//! In-world guest profiling (`OMNI_GUEST_PROF`) put `__memcpy_aarch64_simd` at ~1.4% of the two
//! frame-critical threads' samples, with `memset` and `memmove` below it. omni-cpu can serve a
//! guest call to a known function entry with a host implementation
//! ([`omni_cpu::dynarmic::DynarmicBackend::add_hle`]): the entry is planted, and the native code
//! runs with the faulting context in hand, so a copy off the end of a mapping becomes the same
//! `SIGSEGV` the guest's own routine would raise.
//!
//! **The entries are the implementations the ifunc resolver can pick**, resolved by name from the
//! mapped `libc.so`'s symbol table (never a hardcoded address): on arm64 `memcpy` and `memmove`
//! both resolve to `__memmove_aarch64_simd` (bionic shares the code), so the entries are the
//! `_simd`, base and `_nt` variants, planted as overlap-safe moves (what that code already is), and
//! the two `memset` variants. Whichever the guest's GOT holds is the one its calls reach; the
//! others stay inert. `pthread_getspecific` is left to the guest for now (its `key_map` global and
//! per-key generation make it not trivially safe).

use std::collections::HashMap;
use std::path::PathBuf;

use omni_cpu::dynarmic::Hle;

use crate::guestprof::{host_file, Elf};
use crate::process::Process;

/// The `libc.so` function entries to serve natively: a local implementation symbol, and which
/// native routine serves it. `memcpy`/`memmove` share `__memmove_aarch64_simd`, so every copy
/// variant is an overlap-safe move (the guest code at each already is one).
/// Each entry lists the names it goes by: a `memcpy` and a `memmove` variant share an address, and
/// the symbol table keeps one name per address (`Elf`), so either may be the one found.
pub const ENTRIES: &[(&[&str], Hle)] = &[
    (&["__memcpy_aarch64_simd", "__memmove_aarch64_simd"], Hle::Memmove),
    (&["__memcpy_aarch64", "__memmove_aarch64"], Hle::Memmove),
    (&["__memcpy_aarch64_nt", "__memmove_aarch64_nt"], Hle::Memmove),
    (&["__memset_aarch64"], Hle::Memset),
    (&["__memset_aarch64_nt"], Hle::Memset),
];

/// The virtual address of the first of `names` the library has.
pub fn find(elf: &Elf, names: &[&str]) -> Option<u64> {
    names.iter().find_map(|n| elf.symbol(n))
}

/// Resolve and register this process's `libc.so` HLE entries, if it has `libc.so` mapped and a
/// backend. Idempotent: `add_hle` skips an address already registered, so a second call after more
/// of `libc` is mapped only adds what is new. Returns how many entries are registered now.
pub fn register(p: &Process) -> usize {
    let Some(backend) = p.backend() else { return 0 };
    // (guest start, len, path, file offset) of every file mapping; the inverse of `name_at`.
    let maps = p.mm.file_mappings();
    let mut elves: HashMap<PathBuf, Option<Elf>> = HashMap::new();
    let mut registered = 0;
    for (start, len, guest, file_off) in &maps {
        if guest.rsplit(|&c| c == b'/').next() != Some(b"libc.so".as_slice()) {
            continue;
        }
        let Some(host) = host_file(p, guest) else { continue };
        let elf = elves
            .entry(host.clone())
            .or_insert_with(|| std::fs::File::open(&host).ok().and_then(|mut f| Elf::parse(&mut f, 0)));
        let Some(elf) = elf else { continue };
        for (names, kind) in ENTRIES {
            let Some(vaddr) = find(elf, names) else { continue };
            let Some(fo) = elf.file_of(vaddr) else { continue };
            // The symbol's file offset falls in this mapping: its guest entry is the mapping's
            // guest start plus the distance into the file.
            if fo >= *file_off && fo < file_off + len {
                let entry = start + (fo - file_off);
                backend.add_hle(entry as usize, *kind);
                registered += 1;
            }
        }
    }
    registered
}

/// Register for every live process (the `jit_hle` lever, after it turns the switch on).
pub fn register_all() -> usize {
    crate::process::all_live().iter().map(|p| register(p)).sum()
}
