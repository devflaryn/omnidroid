//! Virtual memory: the platform seam.
//!
//! # What this module is
//!
//! This module *is* the seam. Every virtual-memory capability Omnidroid has is one of the free
//! functions below, and each one is a one-line delegation to a `cfg`-selected backend module:
//! `windows` on Windows, `linux` on Linux, `macos` on macOS. Nothing else in the workspace
//! may call an OS memory API (Global Constraint 4).
//!
//! # Why free functions and not a trait
//!
//! A trait was considered and rejected. Exactly one backend is ever compiled into a binary, so a
//! trait would buy no runtime substitutability; it would only add either a type parameter
//! threaded through `omni-mem`'s public API or a `dyn` call on the guest-memory hot path. The
//! substitutability the plan asks for is *compile-time* substitutability, and this shape already
//! has it, enforced by the compiler: `mod.rs` calls a fixed list of `backend::*` functions, so a
//! backend that is missing one — or whose signature has drifted — does not build for that target.
//!
//! The list a backend must provide is exactly:
//!
//! ```text
//! page_size() -> usize
//! allocation_granularity() -> usize
//! reserve(size, align) -> VmResult<usize>
//! reserve_placeholder(size, align) -> VmResult<usize>
//! split_placeholder(piece_base, size) -> VmResult<()>
//! coalesce_placeholders(ptr, size) -> VmResult<()>
//! commit(ptr, size, prot) -> VmResult<()>
//! commit_placeholder(ptr, size, prot) -> VmResult<()>
//! decommit(ptr, size) -> VmResult<()>
//! decommit_to_placeholder(ptr, size) -> VmResult<()>
//! protect(ptr, size, prot) -> VmResult<()>
//! open_file_for_mapping(path, exec) -> VmResult<MappableFile>
//! map_file(&MappableFile, file_offset, size, ptr, prot) -> VmResult<()>
//! create_shared_section(size) -> VmResult<SharedSection>
//! map_section(&SharedSection, offset, size, prot) -> VmResult<usize>
//! unmap(ptr, size) -> VmResult<()>
//! unmap_and_release(ptr, size) -> VmResult<()>
//! release(base, len, kind) -> VmResult<()>
//! process_commit_charge() -> VmResult<u64>
//! process_working_set() -> VmResult<u64>
//! ```
//!
//! Backends work in `usize` addresses rather than raw pointers, so that the descriptor types in
//! this module are plain data and stay `Send + Sync`.
//!
//! # The model
//!
//! Address space is free and commit charge is scarce (D10). The API makes that asymmetry
//! explicit: [`reserve`] never costs commit charge no matter how large it is, and only
//! [`commit`]/[`commit_placeholder`] spend it. Only [`decommit`], [`decommit_to_placeholder`] and
//! [`release`] give it back — `MEM_RESET`, `DiscardVirtualMemory`, `OfferVirtualMemory` and
//! `EmptyWorkingSet` all measured as returning exactly 0 bytes of commit and are therefore not
//! exposed here as reclamation at all (Global Constraint 6).
//!
//! # The D12 dual-mapped code arena
//!
//! [`create_shared_section`] and [`map_section`] are the *mechanism*: a pagefile-backed section
//! mapped twice, once [`Protection::ReadWrite`] for emission and once [`Protection::ReadExecute`]
//! for execution, so that no page is ever both. The *policy* — how big a chunk is, how blocks are
//! carved out of it, when a block is sealed — lives in `omni-mem`'s `CodeArena`.
//!
//! [`Protection::ReadWrite`] means *writable, and writes go wherever writes to that region are
//! supposed to go*. For private memory that is the memory; for a file-backed view it is a private
//! copy-on-write page, so writes never reach the file; and for a **shared-writable** view, such as
//! the arena's RW view of a pagefile-backed section, it stays shared so that writes are visible
//! through the paired RX view.
//!
//! [`protect`] gets this right by reading back the protection the view was *created* with, so you
//! do not have to do anything — but know that the distinction exists and is load-bearing, because
//! the failure mode if it were ever collapsed is silent: the emitter would write happily, every
//! write would land on a privatised page, and nothing would ever appear through the RX view. There
//! is a unit test in the Windows backend pinning the decision table, including the shared case that
//! no public operation can currently produce. Extend it when you add the arena.
//!
//! There are two flavours of reservation, and they are not interchangeable:
//!
//! * [`reserve`] — an ordinary reservation. Commit into it with [`commit`]. It cannot be
//!   subdivided and it cannot accept a file mapping.
//! * [`reserve_placeholder`] — a *placeholder* reservation. It can be split at 4 KB granularity
//!   with [`split_placeholder`], and each exact-size piece can then be replaced by either private
//!   commit ([`commit_placeholder`]) or a file-backed view ([`map_file`]) at a chosen address.
//!   This is the only path on Windows that gives `mmap(MAP_FIXED)` semantics, and the only path
//!   that accepts a 4 KB-aligned file offset (D11).

use core::fmt;
use std::path::Path;

mod error;

pub use error::{OsError, VmError, VmResult};

// The backend modules are **private**. Everything a caller may use is re-exported from this module
// by a `cfg`-free name, because a `pub mod windows` is a public API surface no other crate can name
// without writing `#[cfg(target_os = "windows")]` itself — which Global Constraint 4 forbids
// everywhere but here. The capability probes that used to live behind `vm::windows::` are
// [`placeholder_api_available`] and [`placeholder_api_symbols`] below.
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as backend;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

/// Page protection.
///
/// # Why there is no `ReadWriteExecute`
///
/// **The absence of a W+X variant is deliberate and load-bearing. Do not add one as a
/// convenience.** Omnidroid never holds a page that is simultaneously writable and executable.
/// That is not only a hardening choice: per D12 the dual-mapped-section approach the JIT will use
/// (one RW view and one RX view of the same pages) measured **162 ns** per emit-and-execute cycle
/// against **2259 ns** for flipping one mapping RW→RX→RW with `VirtualProtect`, so the W^X design
/// is also about fourteen times faster. A `ReadWriteExecute` variant would let a caller take the
/// slow, unsafe path without noticing, and on macOS (`MAP_JIT`) and on ARM64 hosts it would not be
/// available anyway.
///
/// If you are here because you want to write to code pages: use the dual-mapped code arena, or
/// [`protect`] the pages down to [`Protection::ReadWrite`], write, and [`protect`] them back to
/// [`Protection::ReadExecute`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Protection {
    /// No access at all. Touching the page faults.
    ///
    /// Used for guard pages and for the unmapped holes of a guest address space.
    None,
    /// Read only.
    Read,
    /// Read and write, not executable.
    ReadWrite,
    /// Read and execute, not writable.
    ReadExecute,
}

impl Protection {
    /// Every variant, in order. Exists so that invariants can be asserted over all of them.
    pub const ALL: [Protection; 4] = [
        Protection::None,
        Protection::Read,
        Protection::ReadWrite,
        Protection::ReadExecute,
    ];

    /// Whether the page may be read.
    #[must_use]
    pub const fn is_readable(self) -> bool {
        !matches!(self, Protection::None)
    }

    /// Whether the page may be written.
    #[must_use]
    pub const fn is_writable(self) -> bool {
        matches!(self, Protection::ReadWrite)
    }

    /// Whether the page may be executed.
    #[must_use]
    pub const fn is_executable(self) -> bool {
        matches!(self, Protection::ReadExecute)
    }
}

impl fmt::Display for Protection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Protection::None => "none",
            Protection::Read => "r--",
            Protection::ReadWrite => "rw-",
            Protection::ReadExecute => "r-x",
        };
        f.write_str(s)
    }
}

/// Whether a reservation is an ordinary reservation or a placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReservationKind {
    /// An ordinary reservation, from [`reserve`]. Commit into it with [`commit`].
    Plain,
    /// A placeholder reservation, from [`reserve_placeholder`] or [`split_placeholder`]. It can be
    /// split, and its exact-size pieces can be replaced by private commit or a file view.
    Placeholder,
}

/// A reserved range of address space.
///
/// # This is a descriptor, not an RAII guard
///
/// `Reservation` does **not** release its range when dropped, and that is deliberate.
/// [`split_placeholder`] carves an independent piece out of a placeholder, after which the parent
/// range no longer corresponds to a single OS allocation and releasing "the whole thing" is not an
/// operation the OS offers — each piece, view and private-commit region has to be given back
/// individually. A `Drop` impl would therefore be wrong exactly in the case Omnidroid relies on
/// most, and would either leak silently or free a neighbour's memory.
///
/// Lifecycle is owned by `omni-mem`, which keeps the region map that says what each sub-range
/// currently is. Call [`release`] for ranges that are still plain reservations or unreplaced
/// placeholders, [`unmap`]/[`unmap_and_release`] for file views, and
/// [`decommit`]/[`decommit_to_placeholder`] for private commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "a Reservation is not released on drop; hand it to release() or to omni-mem"]
pub struct Reservation {
    base: usize,
    len: usize,
    kind: ReservationKind,
}

impl Reservation {
    /// Base address of the reservation.
    #[must_use]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Base address as a pointer.
    ///
    /// The range is reserved but, unless something has been committed or mapped into it, not
    /// accessible: dereferencing this is a fault until [`commit`], [`commit_placeholder`] or
    /// [`map_file`] has covered the page in question.
    #[must_use]
    pub const fn as_ptr(&self) -> *mut u8 {
        self.base as *mut u8
    }

    /// Length of the reservation in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the reservation is empty. Always false: a zero-size reservation is rejected, by
    /// [`reserve`], [`reserve_placeholder`], [`split_placeholder`] and [`subrange`](Self::subrange)
    /// alike.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// End address, exclusive.
    ///
    /// Saturating, and deliberately so. Every bounds check in this type routes through it, and a
    /// wrapped end would make a range that runs off the top of the address space look *smaller*
    /// than it is — which is the direction that turns a bad argument into a permission. It cannot
    /// actually saturate for a reservation this module produced, because one is only ever built
    /// from a successful OS reservation or from a bounds-checked
    /// [`subrange`](Self::subrange) of one; saturation is here so that the invariant does not have
    /// to be re-derived at every call site.
    #[must_use]
    pub const fn end(&self) -> usize {
        self.base.saturating_add(self.len)
    }

    /// Whether this is a plain reservation or a placeholder.
    #[must_use]
    pub const fn kind(&self) -> ReservationKind {
        self.kind
    }

    /// Address of `offset` bytes into the reservation, checked against its bounds.
    ///
    /// # Errors
    ///
    /// [`VmError::OutsideReservation`] if `offset + len` runs past the end.
    pub fn offset_ptr(&self, offset: usize, len: usize) -> VmResult<*mut u8> {
        let address = self.base.wrapping_add(offset);
        let end = address.wrapping_add(len);
        if offset > self.len || len > self.len - offset {
            return Err(VmError::OutsideReservation {
                operation: "offset_ptr",
                address,
                end,
                reservation_base: self.base,
                reservation_end: self.end(),
            });
        }
        Ok(address as *mut u8)
    }

    /// Describe a sub-range of this reservation as a reservation in its own right.
    ///
    /// This performs no OS call; it builds a descriptor. It exists because
    /// [`split_placeholder`] hands back only the piece it carved, while splitting also leaves the
    /// ranges on either side as independent OS placeholders that something must eventually give
    /// back. The caller — in practice `omni-mem`'s region map — is the only thing that knows what
    /// those ranges currently are, so it is the only thing that can name them.
    ///
    /// Getting it wrong is not silently destructive: [`release`] releases exactly the allocation
    /// that *starts* at the given base, so a descriptor that does not name a real allocation base
    /// is rejected with `ERROR_INVALID_ADDRESS` (487, measured) rather than freeing a neighbour.
    ///
    /// # Errors
    ///
    /// [`VmError::ZeroSize`] for `len == 0` — a zero-length reservation is not a thing this seam
    /// has, and one built here would contradict [`is_empty`](Self::is_empty) and then be *accepted*
    /// by [`release`], which would free whatever allocation happens to start at its base. Also
    /// [`VmError::OutsideReservation`] if the sub-range is not within this reservation.
    pub fn subrange(&self, offset: usize, len: usize, kind: ReservationKind) -> VmResult<Self> {
        check_size("subrange", len)?;
        let base = self.offset_ptr(offset, len)? as usize;
        Ok(Reservation { base, len, kind })
    }

    /// Whether `[ptr, ptr + len)` lies inside this reservation.
    ///
    /// `len == 0` is **false** for every address, including one inside the reservation: a
    /// zero-length range is not anywhere, and answering `true` for `ptr == end()` — which the
    /// arithmetic alone does — would report an address one past the reservation as being inside it.
    #[must_use]
    pub fn contains(&self, ptr: *const u8, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let address = ptr as usize;
        address >= self.base && address < self.end() && len <= self.end() - address
    }
}

/// Whether a file opened for mapping may ever back an executable view.
///
/// This has to be decided when the file is *opened*, not when it is mapped. Measured (D11):
/// a file opened with only `GENERIC_READ` cannot have a `PAGE_EXECUTE_READ` section created over
/// it (`CreateFileMapping` fails with `ERROR_INVALID_HANDLE`, 6), and a view of a `PAGE_READONLY`
/// section can never be raised to executable afterwards (`VirtualProtect` fails with
/// `ERROR_INVALID_PARAMETER`, 87). Get this wrong and guest `.text` simply cannot be made
/// executable — and the failure appears far from its cause, at the point someone tries to run the
/// code rather than at the point the file was opened.
///
/// It is a two-variant enum rather than a `bool` so that the decision is readable at the call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MapExecutability {
    /// The file's pages will never be executed: data, assets, the zip directory.
    NonExecutable,
    /// The file's pages may be mapped or protected [`Protection::ReadExecute`]: the
    /// content-addressed library cache, or a `zipalign`ed APK whose `.so` payloads are mapped in
    /// place.
    Executable,
}

impl fmt::Display for MapExecutability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            MapExecutability::NonExecutable => "non-executable",
            MapExecutability::Executable => "executable",
        };
        f.write_str(s)
    }
}

/// A file opened for mapping, plus the OS mapping object over it.
///
/// Opaque and platform-owned: the only thing to do with one is pass it to [`map_file`]. It closes
/// its handles when dropped, and dropping it does **not** invalidate views already mapped from it
/// — on Windows the section keeps the file alive for as long as any view exists, which is what
/// lets `omni-apk`'s extraction cache be opened, mapped and then forgotten about.
pub struct MappableFile(backend::MappableFile);

impl MappableFile {
    /// Length of the file in bytes, as measured when it was opened.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// Whether the file is empty. Always false: opening a zero-length file for mapping fails.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    /// Whether views of this file may be executable.
    #[must_use]
    pub fn executability(&self) -> MapExecutability {
        self.0.executability()
    }

    /// The path it was opened from, for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

impl fmt::Debug for MappableFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MappableFile")
            .field("path", &self.path())
            .field("len", &self.len())
            .field("executability", &self.executability())
            .finish()
    }
}

// ---------------------------------------------------------------------------------------------
// The seam. Each function below delegates to exactly one backend function.
// ---------------------------------------------------------------------------------------------

/// Size of a page, in bytes. 4096 on every target Omnidroid supports.
#[must_use]
pub fn page_size() -> usize {
    backend::page_size()
}

/// Granularity that reservation *base addresses* are rounded to. 65536 on Windows.
///
/// Almost nothing else is constrained by it: commit, protect, decommit, placeholder splits and
/// placeholder-replacing file views are all 4 KB-granular (D10, D11). Use [`page_size`] unless you
/// specifically mean the base address of a fresh reservation.
#[must_use]
pub fn allocation_granularity() -> usize {
    backend::allocation_granularity()
}

/// Reserve address space with no commit charge.
///
/// `size` is rounded up to a whole number of pages by the OS. `align` must be a power of two;
/// alignments up to [`allocation_granularity`] are satisfied for free, and larger ones are
/// requested explicitly. A reservation of many times physical RAM is normal and costs nothing:
/// measured at 0 bytes of commit charge and 0 bytes of working set at sizes up to 97.7 TB (D10).
///
/// Nothing in the returned range is accessible until [`commit`] covers it.
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::AlignmentNotPowerOfTwo`], or [`VmError::Os`] carrying the
/// OS code — `ERROR_NOT_ENOUGH_MEMORY` (8) when the address space cannot satisfy the request.
pub fn reserve(size: usize, align: usize) -> VmResult<Reservation> {
    check_size("reserve", size)?;
    check_align("reserve", align)?;
    let base = backend::reserve(size, align)?;
    Ok(Reservation { base, len: size, kind: ReservationKind::Plain })
}

/// Reserve address space as a *placeholder*, for later `MAP_FIXED`-style replacement.
///
/// Like [`reserve`], this costs no commit charge. Unlike [`reserve`], the range can be
/// subdivided with [`split_placeholder`] and each exact-size piece can then be replaced by
/// private commit ([`commit_placeholder`]) or by a file-backed view ([`map_file`]) at an address
/// of the caller's choosing.
///
/// This is the reservation kind a guest address space wants, because it is the only one that
/// supports placing a mapping at a specific address.
///
/// # Errors
///
/// As [`reserve`], plus [`VmError::MissingSymbol`] if `VirtualAlloc2` cannot be resolved from
/// `kernelbase.dll` (Windows 10 1803 or later is required).
pub fn reserve_placeholder(size: usize, align: usize) -> VmResult<Reservation> {
    check_size("reserve_placeholder", size)?;
    check_align("reserve_placeholder", align)?;
    let base = backend::reserve_placeholder(size, align)?;
    Ok(Reservation { base, len: size, kind: ReservationKind::Placeholder })
}

/// Split an exact-size piece out of a placeholder reservation, at 4 KB granularity.
///
/// `offset` and `size` are both page-granular — **not** [`allocation_granularity`]-granular. This
/// was the decisive measurement behind D11: splits succeed at 4 KB offsets and at 4 KB-aligned,
/// 64 KB-*mis*aligned base addresses.
///
/// The returned piece is an independent placeholder. The parent `Reservation` remains a valid
/// descriptor of the original address range, but it no longer corresponds to one OS allocation,
/// so it must not be passed to [`release`] as a whole afterwards — release the pieces.
///
/// Split to exactly the size of the thing that will replace the piece: replacement requires an
/// exact-size placeholder, and a short view into a long placeholder fails with
/// `ERROR_INVALID_ADDRESS` (487).
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::Misaligned`] if `offset` or `size` is not page-granular,
/// [`VmError::OutsideReservation`], or [`VmError::Os`].
pub fn split_placeholder(
    reservation: &Reservation,
    offset: usize,
    size: usize,
) -> VmResult<Reservation> {
    const OP: &str = "split_placeholder";
    check_size(OP, size)?;
    check_page_multiple(OP, "offset", offset as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    let base = reservation.offset_ptr(offset, size)? as usize;
    backend::split_placeholder(base, size)?;
    Ok(Reservation { base, len: size, kind: ReservationKind::Placeholder })
}

/// Merge a run of adjacent placeholders back into a single placeholder.
///
/// The inverse of [`split_placeholder`], and the operation that makes a guest address space
/// survive contact with a guest. Splitting is one-way at the OS level: once a placeholder has been
/// carved into pieces — by mapping a segment, or by the guest unmapping part of a range — those
/// pieces stay separate, and replacing a placeholder needs one placeholder of exactly the target
/// size. So a guest that unmaps two adjacent mappings and then maps one larger range across both
/// cannot be served without this call.
///
/// `[ptr, ptr + size)` must contain nothing but placeholders. A range that still holds a view or
/// private commit is rejected by the kernel with `ERROR_INVALID_PARAMETER` (87) rather than being
/// partially merged.
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::Misaligned`], or [`VmError::Os`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be a run of adjacent placeholders this process owns, and nothing may
/// rely on their individual boundaries afterwards — after this call there is exactly one
/// placeholder covering the whole range.
pub unsafe fn coalesce_placeholders(ptr: *mut u8, size: usize) -> VmResult<()> {
    const OP: &str = "coalesce_placeholders";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::coalesce_placeholders(ptr as usize, size)
}

/// Commit pages inside a plain reservation, spending commit charge.
///
/// Commit charge is debited **immediately, at commit, not at first touch** — 1024 MB committed
/// and never touched measured as 1026.66 MB of commit charge against a 4.68 MB working set (D10).
/// So commit late, commit only what will be used, and commit in blocks: bulk commit measured at
/// 3 ns/page against 284 ns/page for one call per page.
///
/// Recommitting a decommitted page yields zero-filled memory, matching anonymous `mmap` and
/// `MADV_DONTNEED`.
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::Misaligned`], or [`VmError::Os`] — `ERROR_COMMITMENT_LIMIT`
/// (1455) when the system commit limit is reached.
///
/// # Safety
///
/// `ptr` must point into a reservation made by [`reserve`] that is still live, and
/// `[ptr, ptr + size)` must lie inside it. Committing over memory that something else is using
/// resets its protection and may zero it.
pub unsafe fn commit(ptr: *mut u8, size: usize, protection: Protection) -> VmResult<()> {
    const OP: &str = "commit";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::commit(ptr as usize, size, protection)
}

/// Replace an exact-size placeholder with private committed memory.
///
/// The placeholder counterpart of [`commit`], for a piece carved by [`split_placeholder`]. This
/// is how a guest anonymous `mmap` at a fixed address is served. Commit charge is debited exactly
/// as for [`commit`] — measured at exactly 65536 bytes for a 64 KB placeholder.
///
/// # Errors
///
/// [`VmError::PlaceholderNotExactSize`] when the placeholder at `ptr` is not exactly `size`
/// bytes, [`VmError::MissingSymbol`] if `VirtualAlloc2` is unavailable, otherwise as [`commit`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be exactly one unreplaced placeholder piece.
pub unsafe fn commit_placeholder(
    ptr: *mut u8,
    size: usize,
    protection: Protection,
) -> VmResult<()> {
    const OP: &str = "commit_placeholder";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::commit_placeholder(ptr as usize, size, protection)
}

/// Return the commit charge of a page range while keeping the address space reserved.
///
/// This is the *only* primitive that gives commit charge back short of releasing the address
/// space: measured at 256.50 MB returned for a 256 MB range, against exactly 0.00 MB for
/// `MEM_RESET`, `DiscardVirtualMemory`, `OfferVirtualMemory` and `EmptyWorkingSet` (D10). It is
/// 4 KB-granular — a single page out of a committed run can be decommitted while its neighbours
/// keep their contents — and the data is gone: a later [`commit`] of the same page reads back
/// zeroes.
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::Misaligned`], or [`VmError::Os`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be committed pages of a live reservation, and nothing may hold a
/// reference into them.
pub unsafe fn decommit(ptr: *mut u8, size: usize) -> VmResult<()> {
    const OP: &str = "decommit";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::decommit(ptr as usize, size)
}

/// Return the commit charge of a placeholder-backed private region and restore the placeholder.
///
/// Use this instead of [`decommit`] for memory that came from [`commit_placeholder`], when the
/// range must stay available for another [`commit_placeholder`] or [`map_file`] later. The
/// address stays owned by this process either way; the difference is whether the range is left as
/// a replaceable placeholder or as ordinary reserved-but-uncommitted memory.
///
/// # Errors
///
/// As [`decommit`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be exactly one region previously produced by [`commit_placeholder`].
pub unsafe fn decommit_to_placeholder(ptr: *mut u8, size: usize) -> VmResult<()> {
    const OP: &str = "decommit_to_placeholder";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::decommit_to_placeholder(ptr as usize, size)
}

/// Change the protection of a page range. 4 KB-granular.
///
/// # Do not call this on a guest mapping
///
/// **This is the raw primitive, and it is invisible to the region map.** `omni-mem` records, per
/// sub-range of every view, whether that range has ever been writable, because a partial unmap on
/// Windows has to unmap the whole view and re-map the survivors — and a survivor that has held a
/// copy-on-write write must have its privatised pages carried across that re-map or the content is
/// silently lost (D11, measured: the relocated byte read back `0x0`). That flag is set by
/// [`GuestSpace::protect`](../../omni_mem/struct.GuestSpace.html#method.protect) and by nothing
/// else. Calling this function directly on an address inside a guest view therefore makes the range
/// writable *without* arming the preservation, and the loss that follows is silent and arbitrarily
/// far away.
///
/// So: anything that owns a region map calls it (that is `omni-mem`, and its own calls all sit on
/// ranges already marked or on a [`Protection::None`] view that can hold no content). Anything that
/// does not own a region map must go through `omni-mem` instead — which is also why no crate but
/// `omni-mem` depends on `omni-platform` for memory operations.
///
/// A single page in the middle of a run can be changed while its neighbours keep their
/// protection, including a single page of a file-backed view. On a view mapped
/// [`Protection::ReadExecute`], the measured legal transitions are to read-only, to
/// [`Protection::ReadWrite`], back to [`Protection::ReadExecute`] and to [`Protection::None`]; a
/// transition to writable-and-executable is rejected by the OS, and there is no [`Protection`]
/// variant that could ask for it.
///
/// [`Protection::ReadWrite`] resolves to whatever "writable" means for what is actually at `ptr`,
/// which the implementation determines rather than guessing: private memory becomes plainly
/// writable, a private file view becomes copy-on-write so writes never reach the file, and a
/// shared-writable view stays shared. The range must be homogeneous — one protect call should not
/// span both a view and private memory.
///
/// # Errors
///
/// [`VmError::Os`] — `ERROR_INVALID_PARAMETER` (87) when the requested protection exceeds what
/// the underlying section allows. A view of a non-executable file can never be raised to
/// [`Protection::ReadExecute`]; that decision was made when the file was opened, see
/// [`MapExecutability`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be committed or mapped pages this process owns.
pub unsafe fn protect(ptr: *mut u8, size: usize, protection: Protection) -> VmResult<()> {
    const OP: &str = "protect";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::protect(ptr as usize, size, protection)
}

/// Open a file so that its contents can be mapped, and create the mapping object over it.
///
/// `executability` must be [`MapExecutability::Executable`] if any view of this file will ever be
/// executable — see [`MapExecutability`] for why this cannot be decided later. It is not free to
/// say yes: the file handle then carries `GENERIC_EXECUTE`, so the caller must have execute
/// access to it.
///
/// The file is opened for reading only and shared for reading, so mapping never modifies it and
/// several instances can map the same cache entry concurrently. A [`Protection::ReadWrite`] view
/// is therefore copy-on-write: writes go to private pages and never reach the file.
///
/// # Errors
///
/// [`VmError::FileOpen`], [`VmError::EmptyFile`], or [`VmError::SectionCreate`].
pub fn open_file_for_mapping(
    path: &Path,
    executability: MapExecutability,
) -> VmResult<MappableFile> {
    backend::open_file_for_mapping(path, executability).map(MappableFile)
}

/// Map part of a file over an exact-size placeholder, at a chosen address.
///
/// Both `ptr` and `file_offset` are 4 KB-granular. That is the whole point of the placeholder
/// path: the same `MapViewOfFile3` call with `BaseAddress = NULL` accepts only 64 KB-aligned file
/// offsets, while replacing a placeholder accepts any page-aligned offset — measured 512/512
/// successes at consecutive 4 KB steps with content verified, against 4/64 on the `NULL` path
/// (D11). A sub-page offset is impossible on any path.
///
/// `protection` maps onto the view as follows:
///
/// | [`Protection`] | view | notes |
/// |---|---|---|
/// | [`Read`](Protection::Read) | `PAGE_READONLY` | shared with the file cache; ~0 commit charge |
/// | [`ReadExecute`](Protection::ReadExecute) | `PAGE_EXECUTE_READ` | needs [`MapExecutability::Executable`]; ~0 commit charge |
/// | [`ReadWrite`](Protection::ReadWrite) | `PAGE_WRITECOPY` | private copy-on-write; **charged its full size at map time** |
/// | [`None`](Protection::None) | — | rejected; map [`Read`](Protection::Read) then [`protect`] to `None` |
///
/// `PAGE_READWRITE` is not reachable and is not offered: the file is opened without
/// `GENERIC_WRITE`, so a writable *shared* view fails with `ERROR_ACCESS_DENIED` (5). Omnidroid
/// never wants one — the extraction cache is immutable and shared between instances.
///
/// A read-only or execute-read view of 4 MB measured 8–16 KB of commit charge (page tables only),
/// and 16 concurrent 8 MB views of one file, every page touched, cost 0.254 MB of commit between
/// them. This is what makes ~109 MB of `libroblox.so` shareable across instances at near-zero
/// marginal cost. A copy-on-write view, by contrast, is charged in full the moment it is mapped —
/// 4.008 MB for a 4 MB view, before anything is written.
///
/// # Errors
///
/// [`VmError::Misaligned`] for a sub-page `file_offset` — the condition the kernel reports as
/// `ERROR_MAPPED_ALIGNMENT` (1132), rejected here with the offending value instead;
/// [`VmError::FileNotOpenedExecutable`]; [`VmError::UnsupportedViewProtection`];
/// [`VmError::ViewPastEndOfFile`]; [`VmError::PlaceholderNotExactSize`] for
/// `ERROR_INVALID_ADDRESS` (487); [`VmError::MissingSymbol`]; or [`VmError::Os`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be exactly one unreplaced placeholder piece, as produced by
/// [`split_placeholder`] or by a [`reserve_placeholder`] of exactly `size` bytes.
pub unsafe fn map_file(
    file: &MappableFile,
    file_offset: u64,
    size: usize,
    ptr: *mut u8,
    protection: Protection,
) -> VmResult<()> {
    const OP: &str = "map_file";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    check_page_multiple(OP, "file offset", file_offset)?;

    if protection == Protection::None {
        return Err(VmError::UnsupportedViewProtection {
            operation: OP,
            protection,
            path: file.path().display().to_string(),
            reason: "a view cannot be created with no access; map it Protection::Read and then \
                     protect() the pages down to Protection::None",
        });
    }
    if protection.is_executable() && file.executability() == MapExecutability::NonExecutable {
        return Err(VmError::FileNotOpenedExecutable {
            operation: OP,
            path: file.path().display().to_string(),
        });
    }
    let end = file_offset.saturating_add(size as u64);
    if end > file.len() {
        return Err(VmError::ViewPastEndOfFile {
            path: file.path().display().to_string(),
            file_offset,
            size,
            end,
            len: file.len(),
        });
    }
    backend::map_file(&file.0, file_offset, size, ptr as usize, protection)
}

/// A pagefile-backed section that can be mapped more than once, for the D12 code arena.
///
/// Anonymous shared memory with no file behind it. The only thing to do with one is pass it to
/// [`map_section`], twice: once [`Protection::ReadWrite`] and once [`Protection::ReadExecute`]. The
/// two views are two sets of page-table entries over the *same* physical pages, so a write through
/// the first is visible through the second immediately and with no instruction-cache flush on
/// x86-64 — measured at 162 ns per emit-and-execute cycle against 2259 ns for flipping one mapping
/// with `VirtualProtect`, across 200,000 trials with 0 mismatches (D12).
///
/// Dropping it closes the section handle but does **not** unmap views already made from it; the
/// kernel keeps the section alive until the last view goes. Views are given back with
/// [`unmap_and_release`].
pub struct SharedSection(backend::SharedSection);

impl SharedSection {
    /// The section's size in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.0.len()
    }

    /// Whether the section is empty. Always false: a zero-length section cannot be created.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }
}

impl fmt::Debug for SharedSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSection").field("len", &self.len()).finish()
    }
}

/// Create a pagefile-backed section whose views may be writable *or* executable.
///
/// The section is created with a maximum protection that allows both, because a section's
/// protection caps every view's protection for the life of the mapping (D11) and the arena needs
/// one writable view and one executable view. That is a property of the *section*, which is a
/// capability rather than a mapping: no page is ever both, and [`Protection`] has no variant that
/// could ask for one.
///
/// # Cost
///
/// A pagefile-backed section is committed at creation, not on first touch, so `size` is charged
/// against the system commit limit immediately — the same asymmetry [`commit`] has (D10). Create
/// sections in modest chunks as the arena fills rather than one large one up front.
///
/// # Errors
///
/// [`VmError::ZeroSize`] or [`VmError::SectionCreate`].
pub fn create_shared_section(size: u64) -> VmResult<SharedSection> {
    if size == 0 {
        return Err(VmError::ZeroSize { operation: "create_shared_section" });
    }
    backend::create_shared_section(size).map(SharedSection)
}

/// Map a view of a section at an address the OS chooses.
///
/// `offset` must be a multiple of [`allocation_granularity`], not of [`page_size`]: this is the
/// `BaseAddress = NULL` path, which measured only 4 successes in 64 at consecutive 4 KB offsets
/// against 512 in 512 for the placeholder path (D11). The arena maps whole sections from offset 0,
/// so the restriction never binds in practice.
///
/// [`Protection::ReadWrite`] gives a genuinely *shared* writable view — `PAGE_READWRITE`, not
/// `PAGE_WRITECOPY`. That distinction is the whole point: a copy-on-write view would privatise
/// every write, the emitter would see its own stores, and the executable view would never change.
///
/// Give the view back with [`unmap_and_release`], passing the size it was mapped with.
///
/// # Errors
///
/// [`VmError::ZeroSize`], [`VmError::Misaligned`], [`VmError::UnsupportedViewProtection`] for
/// [`Protection::None`], [`VmError::ViewPastEndOfFile`] if the view runs past the end of the
/// section, [`VmError::MissingSymbol`], or [`VmError::Os`].
///
/// # Safety
///
/// The returned address is a live mapping of `size` bytes that the caller becomes responsible for
/// unmapping. Nothing else in the process may map over it.
pub unsafe fn map_section(
    section: &SharedSection,
    offset: u64,
    size: usize,
    protection: Protection,
) -> VmResult<*mut u8> {
    const OP: &str = "map_section";
    check_size(OP, size)?;
    check_page_multiple(OP, "size", size as u64)?;

    let granularity = allocation_granularity() as u64;
    if offset % granularity != 0 {
        return Err(VmError::Misaligned {
            operation: OP,
            what: "section offset",
            value: offset,
            required: granularity,
            os_equivalent: OsError(backend::MISALIGNED_OS_ERROR),
        });
    }
    if protection == Protection::None {
        return Err(VmError::UnsupportedViewProtection {
            operation: OP,
            protection,
            path: "<pagefile>".to_string(),
            reason: "a view cannot be created with no access; map it Protection::Read and then \
                     protect() the pages down to Protection::None",
        });
    }
    let end = offset.saturating_add(size as u64);
    if end > section.len() {
        return Err(VmError::ViewPastEndOfFile {
            path: "<pagefile>".to_string(),
            file_offset: offset,
            size,
            end,
            len: section.len(),
        });
    }
    backend::map_section(&section.0, offset, size, protection).map(|address| address as *mut u8)
}

/// Unmap a file-backed view, leaving the address range as a placeholder.
///
/// The range goes back to being an unreplaced placeholder, so a later [`map_file`] or
/// [`commit_placeholder`] can use it again — verified by measurement. This is what a guest
/// `munmap` inside its own address space needs: the address must stay owned by this process, or
/// something else could be handed it.
///
/// `size` must be the size of the whole view. Windows unmaps a view from its base address and
/// cannot partially unmap one, so a request that is not a whole view is refused rather than
/// silently over-unmapped.
///
/// # Errors
///
/// [`VmError::NotViewBase`] if `ptr` is inside a view rather than at its base, or
/// [`VmError::ViewSizeMismatch`] if `ptr` is a view base but `size` is not the view's whole
/// length. Both carry the view's real base and length, which is what a caller emulating a partial
/// unmap needs. Also [`VmError::MissingSymbol`] or [`VmError::Os`].
///
/// # Safety
///
/// `[ptr, ptr + size)` must be one whole view produced by [`map_file`], and nothing may hold a
/// reference into it.
pub unsafe fn unmap(ptr: *mut u8, size: usize) -> VmResult<()> {
    const OP: &str = "unmap";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::unmap(ptr as usize, size)
}

/// Unmap a view and give the address range back to the OS.
///
/// The counterpart of [`unmap`] for teardown: after this the range is free and another
/// reservation may be placed there, so it must not be used for a guest `munmap` of a range the
/// guest address space still claims to own.
///
/// This is also how a [`map_section`] view is released: such a view is not a placeholder
/// replacement, so there is no placeholder to preserve and [`unmap`] would have nothing to leave
/// behind.
///
/// # Errors
///
/// As [`unmap`].
///
/// # Safety
///
/// As [`unmap`].
pub unsafe fn unmap_and_release(ptr: *mut u8, size: usize) -> VmResult<()> {
    const OP: &str = "unmap_and_release";
    check_size(OP, size)?;
    check_page_multiple(OP, "address", ptr as usize as u64)?;
    check_page_multiple(OP, "size", size as u64)?;
    backend::unmap_and_release(ptr as usize, size)
}

/// Release a reservation, returning its address space and any commit charge inside it.
///
/// Releases the whole reservation; there is no partial release (a partial `MEM_RELEASE` fails
/// with `ERROR_INVALID_PARAMETER`, 87). A placeholder that has been split must have its pieces
/// released individually — see [`Reservation`] — or be merged back into one placeholder with
/// [`coalesce_placeholders`] first.
///
/// The reservation's extent is checked against what is really at its base before anything is freed,
/// because `MEM_RELEASE` takes no length and frees whatever allocation starts at the address it is
/// given: a release of a split placeholder's parent would otherwise free only the first piece and
/// report success. That is [`VmError::ReleaseExtentMismatch`], and it is a refusal rather than a
/// partial free precisely because the partial free is silent.
///
/// # Errors
///
/// [`VmError::ZeroSize`] for a zero-length reservation, which `MEM_RELEASE` would otherwise accept
/// and turn into "free whatever allocation starts at this base"; otherwise
/// [`VmError::Os`], carrying whichever code the OS reports. Measured on Windows: an address that
/// is not the base of a live reservation — a double release, or a release of a placeholder parent
/// that has been split — gives `ERROR_INVALID_ADDRESS` (**487**, *not* 87), while a partial
/// release of a live reservation gives `ERROR_INVALID_PARAMETER` (87). 487 is therefore the code
/// to look for after a double free, and it is also the exact-size-placeholder code, so key
/// diagnostics on the [`VmError`] variant rather than on the number alone.
pub fn release(reservation: Reservation) -> VmResult<()> {
    check_size("release", reservation.len)?;
    backend::release(reservation.base, reservation.len, reservation.kind)
}

/// Whether every OS entry point the placeholder mapping path needs is present.
///
/// `cfg`-free by design: this is the seam's one runtime dependency, and a caller must be able to
/// report on it without naming a platform. On Windows it says whether `VirtualAlloc2`,
/// `MapViewOfFile3` and `UnmapViewOfFile2` all resolved from `kernelbase.dll` — they are not
/// exported from `kernel32.dll` (D11), so this is a real question with a real answer. On a backend
/// where the placeholder path is not implemented it is `false`, because the honest answer to "can
/// this process place a mapping at a chosen address" there is no.
#[must_use]
pub fn placeholder_api_available() -> bool {
    backend::placeholder_api_available()
}

/// The OS entry points the placeholder mapping path depends on, each paired with whether it
/// resolved.
///
/// For a diagnostic that says *which* symbol is missing rather than only that something is. Empty
/// on a backend that resolves nothing dynamically.
#[must_use]
pub fn placeholder_api_symbols() -> Vec<(&'static str, bool)> {
    backend::placeholder_api_symbols()
}

/// This process's commit charge, in bytes.
///
/// Commit charge is the scarce resource (D10), so it is observable rather than assumed: tests
/// assert what actually happened to it instead of trusting that a call did what it claims. On
/// Windows this is `PROCESS_MEMORY_COUNTERS_EX::PrivateUsage`, which counts private committed
/// memory and page tables and does **not** count shared file-backed views — exactly the quantity
/// that limits how many instances fit.
///
/// # Errors
///
/// [`VmError::Os`] if the OS refuses to report it.
pub fn process_commit_charge() -> VmResult<u64> {
    backend::process_commit_charge()
}

/// This process's working set, in bytes.
///
/// Working set tracks *touch*, not commit: 1024 MB committed and untouched measured a 4.68 MB
/// working set. It is the wrong number to budget instances against, and is exposed so that tests
/// can demonstrate the difference rather than assert it.
///
/// # Errors
///
/// [`VmError::Os`] if the OS refuses to report it.
pub fn process_working_set() -> VmResult<u64> {
    backend::process_working_set()
}

// ---------------------------------------------------------------------------------------------
// Shared argument validation. Kept here so every backend enforces the same contract.
// ---------------------------------------------------------------------------------------------

fn check_size(operation: &'static str, size: usize) -> VmResult<()> {
    if size == 0 {
        return Err(VmError::ZeroSize { operation });
    }
    Ok(())
}

fn check_align(operation: &'static str, align: usize) -> VmResult<()> {
    if align == 0 || !align.is_power_of_two() {
        return Err(VmError::AlignmentNotPowerOfTwo { operation, align: align as u64 });
    }
    Ok(())
}

fn check_page_multiple(operation: &'static str, what: &'static str, value: u64) -> VmResult<()> {
    let page = page_size() as u64;
    if value % page != 0 {
        return Err(VmError::Misaligned {
            operation,
            what,
            value,
            required: page,
            os_equivalent: OsError(backend::MISALIGNED_OS_ERROR),
        });
    }
    Ok(())
}
