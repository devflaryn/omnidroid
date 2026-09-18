//! The bionic-compatible loader: map the `PT_LOAD` set, relocate it, account for its imports.
//!
//! This is the half of `omni-elf` that touches memory. It reaches it only through
//! [`omni_mem::GuestSpace`], which reaches the OS only through `omni-platform`, so there is still no
//! `cfg(target_os)` and no OS crate anywhere in this crate (Global Constraint 4).
//!
//! # What a load is, in order
//!
//! 1. **Plan** ([`LoadPlan`]) — page-round the `PT_LOAD` set, refuse the shapes that cannot be
//!    mapped, and decide which pieces come from the file and which are private zero pages. No memory
//!    is touched, so every hostile header shape is rejected before anything is reserved.
//! 2. **Reserve** the whole span as one inaccessible anonymous mapping, which costs **0 bytes** of
//!    commit charge (D10). Holes between segments stay that way, exactly as `bionic` leaves them
//!    `PROT_NONE`, so nothing else in the process can be placed inside the library's span.
//! 3. **Map** each piece at `base + p_vaddr`. Text goes down `ReadExecute` from the outset, because
//!    a view mapped read-only out of an executable section can **never** be promoted to executable
//!    (D11 correction, error 87). Writable segments go down `Read` and are raised only in windows.
//! 4. **Zero** the part of each segment's last file page that is `.bss` rather than file content.
//! 5. **Bind** every symbol: defined ones to `base + st_value`, undefined ones through the
//!    [`ProviderRegistry`]. With the [`EmptyProvider`] nothing resolves and all 565 imports of
//!    `libroblox.so` are recorded, which is what M1 asks for.
//! 6. **Relocate in windows** ([`relocate`]). This is the constraint that shapes everything: a
//!    copy-on-write page is charged at `protect` time, not at write time.
//! 7. **Collect** `DT_INIT_ARRAY` from *relocated memory*. Every one of `libroblox.so`'s 3,594 slots
//!    is **zero in the file** — the pointers are produced by `R_AARCH64_RELATIVE` relocations — so a
//!    loader that reads them from the file image collects 3,594 null pointers and cannot tell.
//! 8. **Protect** each piece at its final protection, with `PT_GNU_RELRO` already folded in so that
//!    the 5.2 MB relro region is never transiently writable as a whole.
//!
//! # What is deliberately not here
//!
//! The 3,594 initializers are collected and **not called**: calling them needs the CPU backend, and
//! it also needs `TPIDR_EL0` pointing at a bionic TLS block with a stack guard at `+0x28` before the
//! *first* of them runs (D13). `dl_iterate_phdr` is likewise not implemented; the state it needs is
//! recorded ([`LoadedObject::dl_phdr_info`]) because the in-guest C++ unwinder walks 11.5 MB of
//! `.eh_frame` through it and a stub would break every exception.

pub mod error;
mod plan;
pub mod provider;
pub mod relocate;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_mem::{Backing, CommitPolicy, GuestAddr, GuestSpace, Placement, Protection};

use crate::consts::*;
use crate::ElfImage;

pub use error::{LoadError, LoadResult};
pub use plan::{LoadPlan, Piece, PieceSource, ZeroFill, MAX_SEGMENT_ALIGN};
pub use provider::{
    Binding, EmptyProvider, ProviderRegistry, SymbolKind, SymbolProvider, SymbolRequest,
    SymbolValue,
};
pub use relocate::{RelocationStats, DEFAULT_RELOCATION_WINDOW};

/// What one symbol index resolves to. Built once per load; 1,109 entries for `libroblox.so`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolBinding {
    /// The reserved null symbol, or an unnamed undefined symbol: nothing can bind to it.
    Null,
    /// A concrete address, either `base + st_value` or whatever a provider supplied.
    Value(u64),
    /// Undefined, and no provider supplied it. Relocations referencing it write a null.
    Unresolved,
    /// `STT_GNU_IFUNC`. Refused only if something actually references it, so an unused ifunc symbol
    /// in a tampered file does not make an otherwise loadable object unloadable.
    Ifunc,
}

/// What to do about a strong undefined symbol nothing supplies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnresolvedPolicy {
    /// Record it, bind it to null, and carry on. This is what M1 needs: with a provider that
    /// supplies nothing, every one of `libroblox.so`'s 565 imports is unresolved, and the milestone
    /// asks for them to be *accounted for*, not bound.
    #[default]
    Record,
    /// Fail the load. What the runtime will want once `omni-android` is supposed to supply
    /// everything, since a missing import then means a genuine gap in the compatibility layer.
    Fail,
}

/// How to load an object.
#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// The name recorded for `dl_iterate_phdr` and `/proc/self/maps`. Defaults to the backing's
    /// name.
    pub name: Option<String>,
    /// Bytes made writable at once while relocating. See [`DEFAULT_RELOCATION_WINDOW`].
    pub relocation_window: usize,
    /// Whether `.bss` is committed up front or lazily.
    ///
    /// Defaults to [`CommitPolicy::Eager`], which costs `libroblox.so`'s 11.6 MB of `.bss` in commit
    /// charge immediately. Lazy costs nothing until something touches it, but nothing yet *drives*
    /// the commit — a guest store into an uncommitted granule faults, and D10 rejected fault-driven
    /// paging as a hot path at 2053 ns/fault against 3 ns/page for bulk commit. So the default is
    /// the one that cannot produce a latent crash, and the cheap option is available and measured.
    pub bss_commit: CommitPolicy,
    /// What to do about unresolved strong imports.
    pub unresolved: UnresolvedPolicy,
    /// Whether to make `PT_GNU_RELRO` read-only at the end of the load. Only ever false for a test
    /// that needs to write to the region afterwards.
    pub seal_relro: bool,
    /// Sample [`omni_mem::process_commit_charge`] at each window boundary and report the
    /// peak. Costs one kernel call per window, so it is off by default and on in the tests that
    /// assert the number.
    pub measure_commit: bool,
    /// The most private anonymous memory this loader will plan for: `.bss` plus any final page the
    /// file does not cover, summed over every `PT_LOAD`.
    ///
    /// Defaults to [`DEFAULT_MAX_ANONYMOUS_BYTES`]. Checked before the span is reserved, so a
    /// tampered library is refused without taking any address space; the bound that protects the
    /// scarce resource itself is
    /// [`GuestSpaceConfig::max_committed`](omni_mem::GuestSpaceConfig::max_committed), and this is
    /// the earlier, more specific refusal rather than a replacement for it.
    pub max_anonymous_bytes: usize,
}

/// The default ceiling on a plan's private anonymous memory: 256 MiB.
///
/// Chosen, not derived, and the margin is asserted rather than assumed. `libroblox.so` needs
/// **11,575,296 bytes** of private anonymous memory — its `.bss`, plus the partial final page of each
/// segment — which is the largest figure across all eleven libraries in the APK by two orders of
/// magnitude. That is a **23x** margin, and `loader_hostile.rs`'s `every_library_in_the_apk_loads`
/// pins it, so drift shows up long before a real object is rejected.
///
/// It bounds a quantity that comes straight from `p_memsz`, which is a file field: D6 records that
/// this project's own test APK is adversarially modified, and an eight-byte edit to that field was
/// measured to turn a 16.7 MiB load into a 3.4 GiB one. Note what the limit is *not* derived from:
/// the image span, which `omni-elf` already bounds at 4 GiB and which D10 measured as free. A bound
/// on the abundant resource is not a bound on the scarce one.
pub const DEFAULT_MAX_ANONYMOUS_BYTES: usize = 256 * 1024 * 1024;

impl Default for LoaderConfig {
    fn default() -> Self {
        Self {
            name: None,
            relocation_window: DEFAULT_RELOCATION_WINDOW,
            bss_commit: CommitPolicy::Eager,
            unresolved: UnresolvedPolicy::Record,
            seal_relro: true,
            measure_commit: false,
            max_anonymous_bytes: DEFAULT_MAX_ANONYMOUS_BYTES,
        }
    }
}

/// One mapped piece of a loaded object, at its guest addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedRange {
    /// Index of the `PT_LOAD` in the program-header table.
    pub segment: usize,
    /// Guest start address, page-aligned.
    pub start: GuestAddr,
    /// Guest end address, exclusive and page-aligned.
    pub end: GuestAddr,
    /// The protection this range rests at once the load is complete, `PT_GNU_RELRO` included.
    pub rest: Protection,
    /// Whether this range is private anonymous memory rather than a file view.
    pub anonymous: bool,
    /// File offset this range is mapped from, for `/proc/self/maps` and diagnostics.
    pub file_offset: Option<u64>,
}

impl MappedRange {
    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether the range is empty. Never true: a zero-length piece is not recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// A mapped piece as it is protected *during* the load, which is not how it ends up.
///
/// A writable segment is mapped `Read` — a `ReadWrite` view is copy-on-write and is charged its
/// **full size the moment it is mapped** — and only becomes writable when the load is finished. So
/// the relocation sweep has to restore each window to the protection the piece has *now*, not the
/// one it will have later. Conflating the two is not a subtle bug: it makes the sweep skip the
/// protect for `.data`, because `.data` will eventually be writable, and the first store into it
/// faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiveRange {
    pub(crate) start: GuestAddr,
    pub(crate) end: GuestAddr,
    pub(crate) protection: Protection,
    pub(crate) anonymous: bool,
}

/// The `PT_GNU_RELRO` region, as sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelroRegion {
    /// `p_vaddr` of the segment.
    pub vaddr: u64,
    /// `p_memsz` of the segment. For `libroblox.so` this is exactly 5,205,568 bytes (D9).
    pub memsz: u64,
    /// Guest start of the pages actually made read-only: `base + page_down(p_vaddr)`.
    pub start: GuestAddr,
    /// Guest end of the pages actually made read-only: `base + page_down(p_vaddr + p_memsz)`.
    ///
    /// Rounded **down**, as bionic does. A page only partly covered by the relro segment also holds
    /// `.data`, and sealing it would make that data read-only for the life of the process.
    pub end: GuestAddr,
    /// Whether it was actually sealed, or left writable by
    /// [`LoaderConfig::seal_relro`].
    pub sealed: bool,
}

impl RelroRegion {
    /// Bytes actually protected.
    #[must_use]
    pub fn sealed_bytes(&self) -> usize {
        self.end.saturating_sub(self.start)
    }
}

/// An import nothing supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedImport {
    /// The symbol name.
    pub name: String,
    /// Its index in `.dynsym`.
    pub index: u32,
    /// Function, data, or unspecified.
    pub kind: SymbolKind,
    /// The raw `STT_*` value, so `STT_NOTYPE` is distinguishable from a classification failure.
    pub st_type: u8,
    /// Whether the reference is weak. A weak import binding to null is normal; a strong one is a
    /// hole in the compatibility layer.
    pub weak: bool,
    /// The library `DT_VERNEED` says it comes from, when the file records one.
    pub library: Option<String>,
    /// The version `DT_VERNEED` records, e.g. `"LIBC"`.
    pub version: Option<String>,
}

/// An import a provider supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    /// The symbol name.
    pub name: String,
    /// Its index in `.dynsym`.
    pub index: u32,
    /// What the importer asked for.
    pub kind: SymbolKind,
    /// What it bound to.
    pub address: u64,
    /// Which provider supplied it.
    pub provider: String,
    /// Whether the provider offered a different kind than was asked for — a data symbol bound to a
    /// function, or the reverse. D9 recorded this as a failure mode that names no symbol, so it is
    /// named here.
    pub kind_mismatch: bool,
}

/// Every import of an object, accounted for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imports {
    /// Imports nothing supplied, in `.dynsym` order.
    pub unresolved: Vec<UnresolvedImport>,
    /// Imports a provider supplied, in `.dynsym` order.
    pub resolved: Vec<ResolvedImport>,
}

impl Imports {
    /// Total imports: undefined, named symbols in `.dynsym`. 565 for `libroblox.so`.
    #[must_use]
    pub fn total(&self) -> usize {
        self.unresolved.len() + self.resolved.len()
    }

    /// How many unresolved imports are of one kind.
    #[must_use]
    pub fn unresolved_of_kind(&self, kind: SymbolKind) -> usize {
        self.unresolved.iter().filter(|i| i.kind == kind).count()
    }

    /// Unresolved imports grouped by the library `DT_VERNEED` attributes them to. The key for an
    /// import the file does not attribute is `None`.
    #[must_use]
    pub fn unresolved_by_library(&self) -> BTreeMap<Option<&str>, Vec<&UnresolvedImport>> {
        let mut out: BTreeMap<Option<&str>, Vec<&UnresolvedImport>> = BTreeMap::new();
        for import in &self.unresolved {
            out.entry(import.library.as_deref()).or_default().push(import);
        }
        out
    }

    /// Resolved imports whose provider offered a different kind than the importer asked for.
    #[must_use]
    pub fn kind_mismatches(&self) -> Vec<&ResolvedImport> {
        self.resolved.iter().filter(|i| i.kind_mismatch).collect()
    }
}

/// What a load cost.
#[derive(Debug, Clone, Default)]
pub struct LoadStats {
    /// End to end.
    pub wall_time: Duration,
    /// Reserving the span and mapping every piece.
    pub map_time: Duration,
    /// Binding every symbol in `.dynsym`.
    pub bind_time: Duration,
    /// Decoding the relocation tables that are **materialised**: the plain `DT_RELA`, `DT_REL`,
    /// `DT_RELR` and `DT_JMPREL` tables. The `APS2` blob is streamed into the relocation pass rather
    /// than materialised, so its decode cost is inside [`Self::relocate_time`].
    pub decode_time: Duration,
    /// Applying relocations, windows included.
    pub relocate_time: Duration,
    /// Applying final protections, `PT_GNU_RELRO` included.
    pub protect_time: Duration,
    /// What the relocation pass did.
    pub relocations: RelocationStats,
    /// Bytes mapped from the library file. Shared between instances at near-zero commit charge for
    /// as long as they stay non-writable (D11).
    pub file_backed_bytes: usize,
    /// Bytes mapped as private anonymous memory: `.bss` and any final page the file does not cover.
    pub anonymous_bytes: usize,
    /// Process commit charge before the load, when measurement was enabled.
    pub commit_before: Option<u64>,
    /// Process commit charge once everything is mapped and the materialised relocation tables are
    /// decoded, and before any relocation window is opened.
    ///
    /// Measured separately so that the peak below can be told apart from the cost of mapping: the
    /// difference between this and [`Self::commit_peak`] is the relocation pass's own copy-on-write
    /// charge, which is the number the multi-instance requirement is about. For `libroblox.so` it is
    /// 5.4 MiB — the relro region, which becomes private either way — against a 64 KiB window.
    pub commit_after_decode: Option<u64>,
    /// Peak process commit charge observed during the load.
    pub commit_peak: Option<u64>,
    /// Process commit charge once the load was complete.
    pub commit_after: Option<u64>,
}

impl LoadStats {
    /// Commit charge the decoded relocation tables cost, which is host memory and is released
    /// before the load returns.
    #[must_use]
    pub fn decode_commit_delta(&self) -> Option<i64> {
        Some(self.commit_after_decode? as i64 - self.commit_before? as i64)
    }

    /// Peak commit charge attributable to the load: the peak observed minus the baseline.
    #[must_use]
    pub fn peak_commit_delta(&self) -> Option<i64> {
        Some(self.commit_peak? as i64 - self.commit_before? as i64)
    }

    /// Steady-state commit charge attributable to the load.
    #[must_use]
    pub fn steady_commit_delta(&self) -> Option<i64> {
        Some(self.commit_after? as i64 - self.commit_before? as i64)
    }
}

/// What `dl_iterate_phdr` has to report for one object.
///
/// Not a `dl_phdr_info` — that is `omni-android`'s ABI struct. This is the state the loader owes it,
/// kept here because the in-guest C++ unwinder resolves every frame through that call across 11.5 MB
/// of `.eh_frame`, and an unwinder given a wrong `dlpi_addr` fails in a way that looks like a
/// compiler bug (D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DlPhdrInfo {
    /// `dlpi_name`.
    pub name: String,
    /// `dlpi_addr`: the load bias, which is what has to be added to a `p_vaddr` to get an address.
    pub addr: GuestAddr,
    /// `dlpi_phdr`: the guest address of the program header table.
    pub phdr: GuestAddr,
    /// `dlpi_phnum`.
    pub phnum: u16,
}

/// A loaded, relocated object.
///
/// Dropping one does **not** unmap it: the guest address space owns the mappings and releases them
/// when it is closed. [`unload`](LoadedObject::unload) releases them early and is what the
/// leak-freedom test asserts against measured commit charge.
#[derive(Debug, Clone)]
pub struct LoadedObject {
    /// The name recorded for `dl_iterate_phdr`.
    pub name: String,
    /// `DT_SONAME`, if the object has one.
    pub soname: Option<String>,
    /// `DT_NEEDED`, in link order.
    pub needed: Vec<String>,
    /// The load bias: add it to any `p_vaddr` to get a guest address.
    pub base: GuestAddr,
    /// Guest address of the first byte of the image.
    pub start: GuestAddr,
    /// Guest address one past the last byte of the image.
    pub end: GuestAddr,
    /// Guest address of the program header table.
    pub phdr: GuestAddr,
    /// `e_phnum`.
    pub phnum: u16,
    /// `e_phentsize`.
    pub phentsize: u16,
    /// Every mapped piece, ascending.
    pub ranges: Vec<MappedRange>,
    /// `PT_GNU_RELRO`, if the object has one.
    pub relro: Option<RelroRegion>,
    /// `DT_INIT_ARRAY`, read from relocated memory, in order. **Not called**: see the module
    /// comment.
    pub init_array: Vec<u64>,
    /// `DT_FINI_ARRAY`, read from relocated memory, in order.
    pub fini_array: Vec<u64>,
    /// `DT_PREINIT_ARRAY`, read from relocated memory, in order.
    pub preinit_array: Vec<u64>,
    /// Every import, resolved and unresolved.
    pub imports: Imports,
    /// What the load cost.
    pub stats: LoadStats,
}

impl LoadedObject {
    /// Bytes of address space the image occupies, gaps included.
    #[must_use]
    pub fn span(&self) -> usize {
        self.end - self.start
    }

    /// The state `dl_iterate_phdr` needs.
    #[must_use]
    pub fn dl_phdr_info(&self) -> DlPhdrInfo {
        DlPhdrInfo {
            name: self.name.clone(),
            addr: self.base,
            phdr: self.phdr,
            phnum: self.phnum,
        }
    }

    /// The mapped range containing an address, if any.
    #[must_use]
    pub fn range_at(&self, address: GuestAddr) -> Option<&MappedRange> {
        range_at(&self.ranges, address)
    }

    /// Release every mapping this object holds, returning its commit charge.
    ///
    /// The whole span is unmapped in one call, holes included, so no partial-unmap emulation runs
    /// and the copy-on-write comparison `omni-mem` owes for a partial unmap is never paid.
    ///
    /// # Errors
    ///
    /// [`LoadError::Memory`] if the unmap fails.
    pub fn unload(&self, space: &GuestSpace) -> LoadResult<()> {
        space.unmap(self.start, self.span())?;
        Ok(())
    }
}

/// Load an object into a guest address space.
///
/// `backing` must have been opened [`MapExecutability::Executable`](omni_mem::MapExecutability::Executable)
/// if the object has an executable `PT_LOAD`, which every real library does: the section protection
/// caps every view's protection for the life of the mapping and cannot be raised afterwards (D11).
///
/// # Errors
///
/// Every variant of [`LoadError`]. On any failure the whole reserved span is released, so a failed
/// load leaves no mapping and no commit charge behind — which matters because the inputs this has to
/// refuse are tampered libraries (D6), and a loader that leaks a 109 MB reservation per rejected
/// file is its own denial of service.
pub fn load(
    space: &GuestSpace,
    backing: &Arc<Backing>,
    elf: &ElfImage<'_>,
    providers: &ProviderRegistry,
    config: &LoaderConfig,
) -> LoadResult<LoadedObject> {
    let started = Instant::now();
    let page = space.page_size();

    // The two arguments must be the same file. The plan below is built from `backing.len()` while
    // `PieceSource::FileTailCopy` copies out of `elf.data()`, so a backing longer than the parsed
    // slice maps and relocates file pages the parser never validated, and a shorter one substitutes
    // parsed bytes for the mapped file's. Nothing tied them together — not the signature, not the
    // doc comment, not a runtime check — and the invariant was enforced only by a *test helper*,
    // which is not enforcement.
    if backing.len() != elf.data().len() as u64 {
        return Err(LoadError::BackingLengthMismatch {
            backing: backing.len(),
            parsed: elf.data().len() as u64,
            name: backing.name().to_string(),
        });
    }

    let plan = LoadPlan::build(elf, backing.len(), page)?;

    // Refused before anything is reserved. `p_memsz` is a file field and the part of it past
    // `p_filesz` becomes private anonymous memory, so this is the one plan quantity an attacker can
    // inflate without limit. `omni-mem`'s commit ceiling catches it too, and deliberately: this one
    // refuses earlier and names the ELF-level quantity, that one protects the resource.
    let anonymous = plan.anonymous_bytes();
    if anonymous > config.max_anonymous_bytes {
        return Err(LoadError::AnonymousMemoryTooLarge {
            requested: anonymous,
            limit: config.max_anonymous_bytes,
        });
    }

    let commit_before = if config.measure_commit {
        omni_mem::process_commit_charge().ok()
    } else {
        None
    };

    // One inaccessible anonymous mapping over the whole span. Costs 0 bytes of commit charge (D10)
    // and keeps the holes between segments owned but unusable, as bionic leaves them PROT_NONE.
    let reserved = space.map_anonymous(
        Placement::Anywhere { align: plan.base_align },
        plan.span,
        Protection::None,
        CommitPolicy::Lazy,
    )?;
    let map_reserved = started.elapsed();

    let outcome = load_into(
        space,
        backing,
        elf,
        providers,
        config,
        &plan,
        reserved,
        commit_before,
        map_reserved,
        started,
    );
    if outcome.is_err() {
        // Release everything, including any piece already mapped over part of the reservation.
        if let Err(e) = space.unmap(reserved, plan.span) {
            tracing::error!(
                address = format_args!("{reserved:#x}"),
                span = plan.span,
                %e,
                "could not release the reservation of a failed load"
            );
        }
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn load_into(
    space: &GuestSpace,
    backing: &Arc<Backing>,
    elf: &ElfImage<'_>,
    providers: &ProviderRegistry,
    config: &LoaderConfig,
    plan: &LoadPlan,
    reserved: GuestAddr,
    commit_before: Option<u64>,
    map_reserved: Duration,
    started: Instant,
) -> LoadResult<LoadedObject> {
    let page = space.page_size();
    let image_start = usize::try_from(plan.image_start).map_err(|_| LoadError::AddressOverflow {
        what: "image start",
        base: reserved,
        vaddr: plan.image_start,
    })?;
    let base = reserved
        .checked_sub(image_start)
        .ok_or(LoadError::AddressOverflow {
            what: "load bias: the reservation lies below the image's own base virtual address",
            base: reserved,
            vaddr: plan.image_start,
        })?;

    let bias = |vaddr: u64, what: &'static str| -> LoadResult<GuestAddr> {
        usize::try_from(vaddr)
            .ok()
            .and_then(|v| base.checked_add(v))
            .ok_or(LoadError::AddressOverflow { what, base, vaddr })
    };

    // -------------------------------------------------------------------------------------------
    // Map every piece.
    // -------------------------------------------------------------------------------------------
    let map_started = Instant::now();
    let relro = relro_region(elf, plan, base, page, config.seal_relro)?;
    let mut ranges: Vec<MappedRange> = Vec::with_capacity(plan.pieces.len());
    let mut live: Vec<LiveRange> = Vec::with_capacity(plan.pieces.len());
    // Pieces that need a protect once everything is written: a file view mapped `Read` whose final
    // protection is writable, a tail copy mapped writable whose final protection is not, and
    // anything the relro segment covers.
    let mut final_protects: Vec<(GuestAddr, usize, Protection)> = Vec::new();

    for piece in &plan.pieces {
        let start = bias(piece.vaddr, "piece start")?;
        // `Placement::Fixed` is `MAP_FIXED_NOREPLACE`, not `MAP_FIXED`, so the reservation has to be
        // released first. Single-threaded within one load, and the address stays inside this
        // process's guest space either way.
        space.unmap(start, piece.len)?;

        let at_map = map_protection(piece);
        match piece.source {
            PieceSource::File { offset } => {
                // Executability is chosen here and can never be raised later: a view mapped
                // read-only out of a PAGE_EXECUTE_READ section fails to promote with error 87
                // (D11 correction). Everything else is mapped `Read`, because a `ReadWrite` view is
                // copy-on-write and is charged its **full size the moment it is mapped**.
                space.map_file(backing, offset, Placement::Fixed(start), piece.len, at_map)?;
            }
            PieceSource::FileTailCopy { offset, len } => {
                space.map_anonymous(
                    Placement::Fixed(start),
                    piece.len,
                    Protection::ReadWrite,
                    CommitPolicy::Eager,
                )?;
                let from = usize::try_from(offset).map_err(|_| LoadError::AddressOverflow {
                    what: "file tail copy offset",
                    base,
                    vaddr: offset,
                })?;
                let bytes = elf
                    .data()
                    .get(from..from.saturating_add(len))
                    .ok_or(crate::error::ElfError::OutOfBounds {
                        what: "PT_LOAD final page",
                        offset: from,
                        need: len,
                        have: elf.data().len(),
                    })?;
                let dst = space.ptr(start, piece.len)?;
                // SAFETY: `dst` is `piece.len` bytes of freshly committed, writable, private
                // anonymous memory in this space, `bytes` is a disjoint slice of the file image, and
                // `len <= piece.len` because the plan clipped it.
                unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len()) };
            }
            PieceSource::Zero => {
                // Anonymous memory is zero on commit, so `.bss` needs no write at all.
                space.map_anonymous(
                    Placement::Fixed(start),
                    piece.len,
                    at_map,
                    config.bss_commit,
                )?;
            }
        }
        live.push(LiveRange {
            start,
            end: start + piece.len,
            protection: at_map,
            anonymous: piece.anonymous,
        });

        // The protection this piece must end at, with relro folded in, so a 5.2 MB region is never
        // transiently writable as a whole just to be sealed a moment later.
        for (sub_start, sub_len, wanted) in final_pieces(piece, start, relro.as_ref()) {
            ranges.push(MappedRange {
                segment: piece.segment,
                start: sub_start,
                end: sub_start + sub_len,
                rest: wanted,
                anonymous: piece.anonymous,
                file_offset: match piece.source {
                    PieceSource::File { offset } => Some(offset + (sub_start - start) as u64),
                    _ => None,
                },
            });
            if at_map != wanted {
                final_protects.push((sub_start, sub_len, wanted));
            }
        }
    }
    ranges.sort_by_key(|r| r.start);
    live.sort_by_key(|r| r.start);
    let map_time = map_reserved + map_started.elapsed();

    // -------------------------------------------------------------------------------------------
    // Zero the `.bss` tail of each segment's last file page.
    // -------------------------------------------------------------------------------------------
    for fill in &plan.zero_fills {
        let at = bias(fill.vaddr, "zero-fill start")?;
        let from = at & !(page - 1);
        let to = (at + fill.len)
            .checked_next_multiple_of(page)
            .ok_or(LoadError::AddressOverflow {
                what: "zero-fill end",
                base,
                vaddr: fill.vaddr,
            })?;
        let range = live_at(&live, at).ok_or(LoadError::RelocationTargetUnmapped {
            ty: 0,
            type_name: "zero-fill",
            r_offset: fill.vaddr,
            target: at,
            end: at + fill.len,
        })?;
        let (rest, len) = (range.protection, to.min(range.end) - from);
        let protect = !rest.is_writable();
        if protect {
            space.protect(from, len, Protection::ReadWrite)?;
        }
        let dst = space.ptr(at, fill.len)?;
        // SAFETY: `[at, at + fill.len)` lies inside the mapped range found above, and that range is
        // writable for the duration of this call — either because it rests writable or because it
        // was just protected so.
        unsafe { core::ptr::write_bytes(dst, 0, fill.len) };
        if protect {
            space.protect(from, len, rest)?;
        }
    }

    // -------------------------------------------------------------------------------------------
    // Bind every symbol.
    // -------------------------------------------------------------------------------------------
    let bind_started = Instant::now();
    let (bindings, imports) = bind_symbols(elf, base, providers, config)?;
    let bind_time = bind_started.elapsed();

    // -------------------------------------------------------------------------------------------
    // Relocate, in windows.
    // -------------------------------------------------------------------------------------------
    // The packed blob is **streamed**, not materialised. 568,272 `Elf64_Rela` records are 13.6 MB
    // of host `Vec`, and because it grows by doubling it measured 36.8 MB of commit charge at its
    // peak — more than the entire rest of the load. The window machinery does not need the input
    // sorted, only mostly-sorted, and the blob is: two descents out of 568,272. Measured, that
    // costs 171 windows against 87 for the sorted path, because a descent replays every window
    // between where it lands and where it came from. One window is writable at a time either way,
    // so the peak is unchanged and the wall-time is better.
    let decode_started = Instant::now();
    let mut tables = elf.unpacked_relocations()?;
    let decode_time = decode_started.elapsed();
    let commit_after_decode = if config.measure_commit {
        omni_mem::process_commit_charge().ok()
    } else {
        None
    };

    let relocate_started = Instant::now();
    let mut relocator = relocate::Relocator::new(
        space,
        base,
        &live,
        &bindings,
        config.relocation_window,
        config.measure_commit,
    )?;
    let applied = (|| -> LoadResult<()> {
        if let Some(implicit) = elf.packed_has_implicit_addend() {
            // The sink's error type is fixed to `ElfError`, so a loader-level refusal has to travel
            // out of band. Anything else would throw away which relocation was refused and why.
            let mut stopped: Option<LoadError> = None;
            let summary = elf.decode_packed_with(|r| match relocator.feed(r, implicit, false) {
                Ok(()) => Ok(()),
                Err(LoadError::Elf(e)) => Err(e),
                Err(other) => {
                    stopped = Some(other);
                    Err(crate::error::ElfError::RelocationSinkStopped)
                }
            });
            if let Some(e) = stopped {
                return Err(e);
            }
            let summary = summary?;
            relocator.close()?;
            tracing::debug!(
                relocations = relocator.stats.packed,
                bytes_consumed = summary.as_ref().map(|s| s.bytes_consumed),
                "streamed the packed relocation blob"
            );
        }
        for table in &mut tables.general {
            relocator.apply_table(table)?;
        }
        if let Some(plt) = tables.plt.as_mut() {
            relocator.apply_table(plt)?;
        }
        Ok(())
    })();
    // Always restore an open window's protection, even on the way out of a refusal.
    let closed = relocator.close();
    applied?;
    closed?;
    let relocation_stats = relocator.stats;
    let relocate_time = relocate_started.elapsed();
    drop(tables);

    // -------------------------------------------------------------------------------------------
    // Collect the initializer arrays from relocated memory, never from the file image.
    // -------------------------------------------------------------------------------------------
    let init_array = read_pointer_array(
        space,
        elf,
        base,
        "DT_INIT_ARRAY",
        elf.dynamic().init_array,
        &live,
        plan,
    )?;
    let fini_array = read_pointer_array(
        space,
        elf,
        base,
        "DT_FINI_ARRAY",
        elf.dynamic().fini_array,
        &live,
        plan,
    )?;
    let preinit_array = read_pointer_array(
        space,
        elf,
        base,
        "DT_PREINIT_ARRAY",
        elf.dynamic().preinit_array,
        &live,
        plan,
    )?;

    // -------------------------------------------------------------------------------------------
    // Final protections, relro included.
    // -------------------------------------------------------------------------------------------
    let protect_started = Instant::now();
    for (start, len, protection) in final_protects {
        space.protect(start, len, protection)?;
    }
    let protect_time = protect_started.elapsed();

    let phdr = phdr_address(elf, base, &live)?;
    let commit_after = if config.measure_commit {
        omni_mem::process_commit_charge().ok()
    } else {
        None
    };
    let commit_peak = [relocation_stats.peak_commit_charge, commit_after_decode, commit_after]
        .into_iter()
        .flatten()
        .max();

    let loaded = LoadedObject {
        name: config
            .name
            .clone()
            .or_else(|| elf.soname().ok().flatten().map(str::to_owned))
            .unwrap_or_else(|| backing.name().to_string()),
        soname: elf.soname()?.map(str::to_owned),
        needed: elf.needed()?.into_iter().map(str::to_owned).collect(),
        base,
        start: reserved,
        end: reserved + plan.span,
        phdr,
        phnum: elf.header().e_phnum,
        phentsize: elf.header().e_phentsize,
        ranges,
        relro,
        init_array,
        fini_array,
        preinit_array,
        imports,
        stats: LoadStats {
            wall_time: started.elapsed(),
            map_time,
            bind_time,
            decode_time,
            relocate_time,
            protect_time,
            relocations: relocation_stats,
            file_backed_bytes: plan.file_backed_bytes(),
            anonymous_bytes: plan.anonymous_bytes(),
            commit_before,
            commit_after_decode,
            commit_peak,
            commit_after,
        },
    };
    tracing::info!(
        name = %loaded.name,
        base = format_args!("{:#x}", loaded.base),
        span = loaded.span(),
        relocations = loaded.stats.relocations.applied,
        windows = loaded.stats.relocations.windows,
        imports = loaded.imports.total(),
        unresolved = loaded.imports.unresolved.len(),
        init_array = loaded.init_array.len(),
        wall_time = ?loaded.stats.wall_time,
        "loaded a guest object"
    );
    Ok(loaded)
}

/// Split a piece into the sub-ranges that end at different protections, because `PT_GNU_RELRO`
/// covers part of it.
fn final_pieces(
    piece: &Piece,
    start: GuestAddr,
    relro: Option<&RelroRegion>,
) -> Vec<(GuestAddr, usize, Protection)> {
    let end = start + piece.len;
    let sealed = |p: Protection| if p.is_writable() { Protection::Read } else { p };
    match relro.filter(|r| r.sealed && r.start < end && r.end > start) {
        None => vec![(start, piece.len, piece.rest)],
        Some(r) => {
            let mut out = Vec::with_capacity(3);
            let lo = r.start.max(start);
            let hi = r.end.min(end);
            if lo > start {
                out.push((start, lo - start, piece.rest));
            }
            out.push((lo, hi - lo, sealed(piece.rest)));
            if hi < end {
                out.push((hi, end - hi, piece.rest));
            }
            out
        }
    }
}

fn range_at(ranges: &[MappedRange], address: GuestAddr) -> Option<&MappedRange> {
    let i = ranges.partition_point(|r| r.start <= address).checked_sub(1)?;
    ranges.get(i).filter(|r| address < r.end)
}

/// Whether the plan maps every byte of `[from, to)`, with no hole.
///
/// The pieces are in ascending address order by construction, so this is one sweep.
fn plan_covers(plan: &LoadPlan, from: u64, to: u64) -> bool {
    if to <= from {
        return true;
    }
    let mut cursor = from;
    for piece in &plan.pieces {
        if piece.vaddr > cursor {
            return false;
        }
        cursor = cursor.max(piece.vaddr_end());
        if cursor >= to {
            return true;
        }
    }
    false
}

fn live_at(ranges: &[LiveRange], address: GuestAddr) -> Option<&LiveRange> {
    let i = ranges.partition_point(|r| r.start <= address).checked_sub(1)?;
    ranges.get(i).filter(|r| address < r.end)
}

/// The protection a piece is *mapped* at, which is not the protection it ends at.
///
/// Executability has to be chosen here, because a view mapped read-only out of an executable section
/// can never be promoted (D11 correction, error 87). Writability must **not** be chosen here,
/// because a copy-on-write view is charged its full size the instant it is mapped — so a writable
/// segment is mapped `Read` and raised at the very end, and only ever in windows before that.
fn map_protection(piece: &Piece) -> Protection {
    match piece.source {
        PieceSource::File { .. } if piece.rest.is_executable() => Protection::ReadExecute,
        PieceSource::File { .. } => Protection::Read,
        // The tail copy has to be written during the load, so it is mapped writable. It is private
        // anonymous memory, so that costs its own size and nothing more.
        PieceSource::FileTailCopy { .. } => Protection::ReadWrite,
        PieceSource::Zero => piece.rest,
    }
}

/// Compute the relro region, and check the pages it would seal are all actually mapped.
///
/// The check is against the **mapped pieces**, not against `p_vaddr + p_memsz` of a `PT_LOAD`,
/// because a real relro segment routinely runs past the end of its own segment's memory image:
/// `libdatastore_shared_counter.so` has a writable `PT_LOAD` ending at `0x5428` and a relro segment
/// ending at `0x6000`, the page boundary above it. Comparing against the unrounded segment end
/// rejects that library, which is how this check was found to be wrong.
fn relro_region(
    elf: &ElfImage<'_>,
    plan: &LoadPlan,
    base: GuestAddr,
    page: usize,
    seal: bool,
) -> LoadResult<Option<RelroRegion>> {
    let Some(seg) = elf.relro() else { return Ok(None) };
    let end_vaddr = seg.p_vaddr.checked_add(seg.p_memsz).ok_or(LoadError::RelroOutsideImage {
        vaddr: seg.p_vaddr,
        end: seg.vaddr_end(),
        memsz: seg.p_memsz,
    })?;
    // bionic rounds both ends **down**: a page only partly covered by the relro segment also holds
    // writable data, and sealing it would make that data read-only for good.
    let start_page = plan::page_down(seg.p_vaddr, page);
    let end_page = plan::page_down(end_vaddr, page);
    if end_page < start_page || !plan_covers(plan, start_page, end_page) {
        return Err(LoadError::RelroOutsideImage {
            vaddr: seg.p_vaddr,
            end: end_vaddr,
            memsz: seg.p_memsz,
        });
    }
    let to_addr = |v: u64| -> LoadResult<GuestAddr> {
        usize::try_from(v)
            .ok()
            .and_then(|x| base.checked_add(x))
            .ok_or(LoadError::AddressOverflow { what: "PT_GNU_RELRO", base, vaddr: v })
    };
    Ok(Some(RelroRegion {
        vaddr: seg.p_vaddr,
        memsz: seg.p_memsz,
        start: to_addr(start_page)?,
        end: to_addr(end_page)?,
        sealed: seal,
    }))
}

/// Bind every entry of `.dynsym`, and account for every import.
fn bind_symbols(
    elf: &ElfImage<'_>,
    base: GuestAddr,
    providers: &ProviderRegistry,
    config: &LoaderConfig,
) -> LoadResult<(Vec<SymbolBinding>, Imports)> {
    let symtab = elf.symbols()?;
    let strtab = elf.strtab()?;
    let versions = elf.version_info()?;
    let count = symtab.len();
    let mut bindings = Vec::new();
    bindings
        .try_reserve(count as usize)
        .map_err(|_| crate::error::ElfError::AllocationFailed {
            bytes: (count as usize).saturating_mul(core::mem::size_of::<SymbolBinding>()),
        })?;
    let mut imports = Imports::default();

    for index in 0..count {
        let sym = symtab.get(index)?;
        if index == 0 {
            bindings.push(SymbolBinding::Null);
            continue;
        }
        if sym.is_undefined() {
            if sym.st_name == 0 {
                // An undefined symbol with no name cannot be looked up anywhere. It is not an
                // import; it is a hole in the table.
                bindings.push(SymbolBinding::Null);
                continue;
            }
            let name = strtab.get(u64::from(sym.st_name))?;
            let requirement = versions.requirement(index)?;
            let kind = SymbolKind::from_st_type(sym.sym_type());
            let request = SymbolRequest {
                name,
                kind,
                library: requirement.library(),
                version: requirement.version(),
                weak: sym.is_weak(),
            };
            match providers.resolve(&request) {
                Some(binding) => {
                    imports.resolved.push(ResolvedImport {
                        name: name.to_owned(),
                        index,
                        kind,
                        address: binding.value.address,
                        provider: binding.provider.to_owned(),
                        kind_mismatch: binding.kind_mismatch,
                    });
                    bindings.push(SymbolBinding::Value(binding.value.address));
                }
                None => {
                    if config.unresolved == UnresolvedPolicy::Fail && !sym.is_weak() {
                        return Err(LoadError::UnresolvedSymbol {
                            name: name.to_owned(),
                            kind: kind.elf_name(),
                            library: requirement
                                .library()
                                .unwrap_or("<unrecorded>")
                                .to_owned(),
                        });
                    }
                    imports.unresolved.push(UnresolvedImport {
                        name: name.to_owned(),
                        index,
                        kind,
                        st_type: sym.sym_type(),
                        weak: sym.is_weak(),
                        library: requirement.library().map(str::to_owned),
                        version: requirement.version().map(str::to_owned),
                    });
                    bindings.push(SymbolBinding::Unresolved);
                }
            }
        } else if sym.is_ifunc() {
            bindings.push(SymbolBinding::Ifunc);
        } else {
            let value = usize::try_from(sym.st_value)
                .ok()
                .and_then(|v| base.checked_add(v))
                .ok_or(LoadError::AddressOverflow {
                    what: "symbol value",
                    base,
                    vaddr: sym.st_value,
                })?;
            bindings.push(SymbolBinding::Value(value as u64));
        }
    }
    Ok((bindings, imports))
}

/// Read a `DT_*_ARRAY` out of relocated guest memory.
///
/// Reading it from the *file* image is the trap: all 3,594 of `libroblox.so`'s `DT_INIT_ARRAY` slots
/// are zero in the file, because the pointers are produced by `R_AARCH64_RELATIVE` relocations. A
/// loader that reads the file collects 3,594 nulls and has no way to notice.
fn read_pointer_array(
    space: &GuestSpace,
    elf: &ElfImage<'_>,
    base: GuestAddr,
    what: &'static str,
    array: Option<crate::DynArray>,
    ranges: &[LiveRange],
    plan: &LoadPlan,
) -> LoadResult<Vec<u64>> {
    let Some(array) = array else { return Ok(Vec::new()) };
    if array.size % SIZEOF_PTR as u64 != 0 {
        return Err(crate::error::ElfError::UnalignedTableSize {
            what,
            size: array.size,
            entsize: SIZEOF_PTR as u64,
        }
        .into());
    }
    // The array has to be inside the *file* image of a PT_LOAD, which is what makes it readable
    // here; `slice_at_vaddr` is the bounds check, and its result is discarded because the values
    // that matter are the relocated ones in memory.
    elf.slice_at_vaddr(what, array.vaddr, array.size)?;
    let start = usize::try_from(array.vaddr)
        .ok()
        .and_then(|v| base.checked_add(v))
        .ok_or(LoadError::AddressOverflow { what, base, vaddr: array.vaddr })?;
    let size = usize::try_from(array.size).map_err(|_| LoadError::AddressOverflow {
        what,
        base,
        vaddr: array.size,
    })?;
    let range = live_at(ranges, start).filter(|r| start + size <= r.end).ok_or(
        LoadError::RelocationTargetUnmapped {
            ty: 0,
            type_name: what,
            r_offset: array.vaddr,
            target: start,
            end: start + size,
        },
    )?;
    if !range.protection.is_readable() {
        return Err(LoadError::RelocationTargetUnmapped {
            ty: 0,
            type_name: what,
            r_offset: array.vaddr,
            target: start,
            end: start + size,
        });
    }

    let count = size / SIZEOF_PTR;
    let mut out = Vec::new();
    out.try_reserve(count).map_err(|_| crate::error::ElfError::AllocationFailed {
        bytes: count.saturating_mul(SIZEOF_PTR),
    })?;
    let ptr = space.ptr(start, size)?;
    let image_low = base.wrapping_add(usize::try_from(plan.image_start).unwrap_or(0));
    let image_high = image_low.wrapping_add(plan.span);
    for i in 0..count {
        // SAFETY: `[start, start + size)` lies inside one readable mapped range, checked above, and
        // `i * 8 + 8 <= size`.
        let value = unsafe { ptr.add(i * SIZEOF_PTR).cast::<u64>().read_unaligned() };
        let as_addr = usize::try_from(value).unwrap_or(usize::MAX);
        if as_addr < image_low || as_addr >= image_high {
            return Err(LoadError::InitArrayEntryOutsideImage {
                what,
                index: i,
                count,
                value,
                base: image_low,
                end: image_high,
            });
        }
        out.push(value);
    }
    Ok(out)
}

/// Where the program header table ended up in memory.
///
/// `PT_PHDR` is preferred, and `e_phoff` inside a `PT_LOAD` file image is the fallback, which is
/// what bionic does. A stripped-down object with neither is refused: `dl_iterate_phdr` has to report
/// a real address or the in-guest unwinder silently walks garbage.
fn phdr_address(
    elf: &ElfImage<'_>,
    base: GuestAddr,
    ranges: &[LiveRange],
) -> LoadResult<GuestAddr> {
    let header = elf.header();
    let bytes = u64::from(header.e_phnum) * u64::from(header.e_phentsize);
    let refuse = || LoadError::ProgramHeadersNotMapped {
        phoff: header.e_phoff,
        phnum: header.e_phnum,
        phentsize: header.e_phentsize,
    };

    let mut candidate = None;
    for seg in elf.segments() {
        if seg.p_type == PT_PHDR && seg.p_filesz >= bytes && seg.p_offset == header.e_phoff {
            candidate = Some(seg.p_vaddr);
            break;
        }
    }
    if candidate.is_none() {
        for seg in elf.load_segments() {
            let end = header.e_phoff.saturating_add(bytes);
            if header.e_phoff >= seg.p_offset && end <= seg.file_end() {
                candidate = Some(seg.p_vaddr + (header.e_phoff - seg.p_offset));
                break;
            }
        }
    }
    let vaddr = candidate.ok_or_else(refuse)?;
    // And it must actually be readable where it landed.
    if !elf.load_segments().any(|s| s.file_contains_vaddr(vaddr, bytes)) {
        return Err(refuse());
    }
    let at = usize::try_from(vaddr)
        .ok()
        .and_then(|v| base.checked_add(v))
        .ok_or_else(refuse)?;
    let len = usize::try_from(bytes).map_err(|_| refuse())?;
    live_at(ranges, at)
        .filter(|r| at + len <= r.end && r.protection.is_readable())
        .ok_or_else(refuse)?;
    Ok(at)
}
