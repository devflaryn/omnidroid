//! The loader fixture: a guest address space, the real extraction-cache file open for execute, and
//! the parsed image, all from the same bytes.
//!
//! Shared by the M1 suite and the commit-charge suite, which have to be separate test binaries
//! because commit charge is a per-process quantity — see `loader_commit.rs`.

#![allow(dead_code)]

use std::sync::Arc;

use omni_elf::loader::{self, LoaderConfig, ProviderRegistry};
use omni_elf::{ElfImage, LoadedObject};
use omni_mem::{Backing, GuestSpace, MapExecutability};

/// Measured from the real binary; see `docs/DECISIONS.md` D9 and `research/apk-analysis.md`.
pub const SPAN: u64 = 0x0733_3c3c;
pub const RELRO_VADDR: u64 = 0x062d_c1c0;
pub const RELRO_BYTES: u64 = 5_205_568;
pub const APS2_TOTAL: usize = 568_272;
pub const PLT_TOTAL: usize = 534;
pub const GRAND_TOTAL: usize = 568_806;
pub const N_RELATIVE: usize = 568_194;
pub const N_GLOB_DAT: usize = 56;
pub const N_ABS64: usize = 22;
pub const IMPORTS: usize = 565;
pub const IMPORTS_FUNC: usize = 539;
pub const IMPORTS_OBJECT: usize = 23;
pub const IMPORTS_NOTYPE: usize = 3;
pub const INIT_ARRAY_ENTRIES: usize = 3_594;
pub const FINI_ARRAY_ENTRIES: usize = 3;

pub const R_RELATIVE: u32 = 1027;
pub const R_GLOB_DAT: u32 = 1025;
pub const R_JUMP_SLOT: u32 = 1026;
pub const R_ABS64: u32 = 257;

pub const MIB: f64 = 1024.0 * 1024.0;

#[must_use]
pub fn mib(bytes: i64) -> f64 {
    bytes as f64 / MIB
}

pub struct Fixture {
    pub space: GuestSpace,
    pub backing: Arc<Backing>,
    pub elf: ElfImage<'static>,
}

/// `None` when the APK is absent, in which case the caller skips.
pub fn fixture() -> Option<Fixture> {
    let path = super::cached_main_lib()?;
    let bytes = super::cached_main_lib_bytes()?;
    // Opened `Executable` here and nowhere else: the section protection caps every view's
    // protection for the life of the mapping, so `.text` can never be made executable later (D11).
    let backing = Backing::open(&path, MapExecutability::Executable).expect("open the cache entry");
    assert_eq!(
        backing.len() as usize,
        bytes.len(),
        "the mapped file and the parsed bytes must be the same file"
    );
    Some(Fixture {
        space: GuestSpace::new().expect("reserve a guest address space"),
        backing,
        elf: ElfImage::parse(bytes).expect("parse libroblox.so"),
    })
}

/// The default configuration, with commit-charge sampling turned on.
#[must_use]
pub fn measuring() -> LoaderConfig {
    LoaderConfig { measure_commit: true, ..LoaderConfig::default() }
}

/// Load with a provider that supplies nothing, which is what M1 asks for.
pub fn load(f: &Fixture, config: &LoaderConfig) -> LoadedObject {
    loader::load(
        &f.space,
        &f.backing,
        &f.elf,
        &ProviderRegistry::empty_provider(),
        config,
    )
    .expect("libroblox.so must load")
}
