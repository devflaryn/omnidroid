//! Linux backend for the virtual-memory seam.
//!
//! Every rule this module encodes was measured on the port host (Linux 7.0, x86-64,
//! `vm.overcommit_memory = 0`) by the probes in `tests/vm_probe_linux.rs` and recorded in
//! `docs/ports/linux-notes/mem.md`. Where a rule is read from the kernel source rather than
//! measured, it says so.
//!
//! # What the seam's words mean here
//!
//! | seam | Linux | measured effect |
//! |---|---|---|
//! | reserve / reserve_placeholder | `mmap(PROT_NONE, MAP_PRIVATE \| MAP_ANONYMOUS)` | 0 commit charge, 0 RSS, to 100 TiB in one call |
//! | commit (plain) | `mprotect` | `VM_ACCOUNT` taken **at the call**, for a writable protection |
//! | commit_placeholder | `mmap(MAP_FIXED, MAP_PRIVATE \| MAP_ANONYMOUS)` | the same, and zero-filled |
//! | decommit / decommit_to_placeholder | `mmap(MAP_FIXED, PROT_NONE)` over the range | RSS **and** `VM_ACCOUNT` returned |
//! | map_file | `mmap(MAP_FIXED, MAP_PRIVATE)`; `MAP_SHARED` for a shared file | a writable private view is charged in full at map time |
//! | create_shared_section / map_section | `memfd_create` + `mmap(MAP_SHARED)` twice | no `VM_ACCOUNT`; shmem pages charged on first touch |
//! | process_commit_charge | sum of the VMAs `/proc/self/smaps` flags `ac` | exactly what this process adds to `Committed_AS` |
//!
//! # Why the reservation is **not** `MAP_NORESERVE`
//!
//! The port brief suggested it, and it was measured before it was rejected. `MAP_NORESERVE` does
//! nothing for the reservation itself: a `PROT_NONE` private mapping is not accountable with or
//! without it (`accountable_mapping()` requires `VM_WRITE`), so 16 GiB reserved measured **0 kB**
//! of `Committed_AS` both ways. What it *does* do is mark the VMA `VM_NORESERVE`, and
//! `mprotect_fixup()` skips accounting for a VMA carrying that flag when it becomes writable. So
//! with it, **commit stops being charged at all** -- 64 MiB `mprotect`ed read-write measured
//! +0 kB of `Committed_AS` and +0 kB of `ac` VMAs, against +65,536 kB and +65,536 kB without it.
//! That is D10's central asymmetry (commit is debited at commit, not at touch) switched off, and
//! it is invisible to every functional test. (Under `vm.overcommit_memory = 2` the kernel ignores
//! `MAP_NORESERVE`, so there the two spellings agree; they differ exactly on the default setting.)
//!
//! # Why decommit is a re-`mmap` and not `madvise`
//!
//! Measured, 64 MiB touched then released:
//!
//! | primitive | RSS returned | `Committed_AS` returned |
//! |---|---|---|
//! | `madvise(MADV_DONTNEED)` | **yes**, -65,536 kB | **no**, 0 kB |
//! | `madvise` then `mprotect(PROT_NONE)` | yes | **no**, 0 kB |
//! | `mmap(MAP_FIXED, PROT_NONE)` over the range | yes, -65,536 kB | **yes**, -65,536 kB |
//!
//! `MADV_DONTNEED` is Linux's `MEM_RESET` trap: cheap, frees the pages, looks right in every RSS
//! test, and returns none of the scarce resource. `mprotect(PROT_NONE)` only drops `VM_ACCOUNT`
//! from a VMA that has never been touched (`!vma->anon_vma`), which is the case where there was
//! nothing to return. Replacing the range with a fresh `PROT_NONE` mapping is the one call that
//! gives both back, and it zero-fills a later commit, as `MEM_DECOMMIT` does.
//!
//! # The ledger: why a backend with no placeholders keeps a table of them
//!
//! `mmap(MAP_FIXED)` replaces any page-aligned sub-range of a mapping, so splitting and merging
//! placeholders needs **no kernel call** -- the placeholder API is genuinely available here, and
//! [`placeholder_api_available`] says so. But the Windows kernel does something the Linux one does
//! not: it *refuses*. A view mapped into a placeholder that is not exactly its size, a release that
//! names half an allocation, an unmap of part of a view, a release of an address already released --
//! Windows answers each with an error, and `omni-mem`'s region map, its tests and the seam's own
//! documentation are written against those answers. Linux answers every one of them by doing it:
//! `munmap` of an address that was released and has since been handed to someone else unmaps
//! *their* memory, silently.
//!
//! So this module keeps a process-wide **ledger** of every range the seam has handed out, with
//! what each range currently is (plain reservation, placeholder, private commit, file view, section
//! view), and checks each call against it before any kernel call is made. It is the Windows
//! allocation table's role, not its implementation: it holds only what the seam's contract needs
//! to refuse, and it never refuses something Windows would accept (the region map is written
//! against Windows, so a stricter ledger would be a spurious failure). The places it is
//! deliberately *more* permissive than Windows are listed on each function.
//!
//! The ledger is a `Mutex<BTreeMap>`, and it is taken on the demand pager's path, inside a
//! `SIGSEGV` handler (`fault/linux.rs`). That is sound for the same reason the space's own lock is:
//! a guest page fault is **synchronous** -- it is delivered at the faulting instruction, which is
//! guest code or host code reading guest memory, and no thread faults while it holds this lock,
//! because nothing under it dereferences anything but the ledger's own heap nodes. It is **not**
//! async-signal-safe in the POSIX sense (it may allocate a node); `fault/linux.rs` states the whole
//! argument, and the conditions under which it would stop holding.

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use super::unix::posix;
use super::{MapExecutability, OsError, Protection, ReservationKind, VmError, VmResult};

/// `EINVAL`: what `mmap` returns for a base address or file offset that is not page-aligned.
pub(super) const MISALIGNED_OS_ERROR: u32 = libc::EINVAL as u32;

/// What the ledger answers with when it refuses a call the kernel would have carried out.
///
/// `EINVAL`, because that is `mmap(2)`'s and `munmap(2)`'s code for arguments that do not describe
/// a valid request -- and the refusal is always of that shape: the range is not the kind of thing
/// the operation is for. It is this module's answer, not the kernel's; the error's `operation`
/// names the seam call that was refused.
const REFUSED: u32 = libc::EINVAL as u32;

// -------------------------------------------------------------------------------------------
// Page size
// -------------------------------------------------------------------------------------------

/// `sysconf(_SC_PAGESIZE)`, read once. The seam's argument checks ask for it on every call, and on
/// the demand pager's path, so it is cached rather than re-asked.
pub(super) fn page_size() -> usize {
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    let cached = PAGE.load(Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }
    let page = super::unix::page_size();
    PAGE.store(page, Ordering::Relaxed);
    page
}

/// There is no separate reservation granularity on Linux: a mapping's base is page-aligned.
pub(super) fn allocation_granularity() -> usize {
    page_size()
}

fn round_up(value: usize, to: usize) -> Option<usize> {
    value.checked_add(to - 1).map(|v| v & !(to - 1))
}

// -------------------------------------------------------------------------------------------
// The ledger
// -------------------------------------------------------------------------------------------

/// What one ledger range currently is. Each is one "allocation" in the Windows sense: the unit a
/// release, an unmap or a placeholder replacement must name whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// From [`reserve`]. Commit, decommit and protect act on sub-ranges of it; it stays one piece.
    Plain,
    /// From [`reserve_placeholder`], [`split_placeholder`], [`decommit_to_placeholder`] or
    /// [`unmap`]: `PROT_NONE`, and replaceable by exactly-sized private commit or a file view.
    Placeholder,
    /// Private anonymous memory that replaced a placeholder ([`commit_placeholder`]).
    Private,
    /// A file view that replaced a placeholder ([`map_file`]).
    View {
        /// Whether the file was opened [`MapExecutability::Executable`]. A view of any other file is
        /// refused `PROT_EXEC` by [`protect`], as Windows' section protection refuses it.
        executable: bool,
    },
    /// A view of a [`SharedSection`] at an address the kernel chose ([`map_section`]).
    SectionView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    len: usize,
    kind: Kind,
}

/// Every range this seam has handed out and not given back, keyed by base. Ranges never overlap.
static LEDGER: Mutex<BTreeMap<usize, Piece>> = Mutex::new(BTreeMap::new());

fn ledger() -> MutexGuard<'static, BTreeMap<usize, Piece>> {
    // A panic while the ledger is held cannot leave it half-edited: every edit below is computed
    // first and applied as whole inserts and removes. So a poisoned lock is still a true ledger.
    LEDGER.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The piece containing `address`, if any.
fn containing(map: &BTreeMap<usize, Piece>, address: usize) -> Option<(usize, Piece)> {
    let (&base, &piece) = map.range(..=address).next_back()?;
    (address - base < piece.len).then_some((base, piece))
}

/// The pieces covering `[address, address + len)` end to end, or `None` if any byte of it is not
/// in the ledger. The first piece may start before `address` and the last may end after the range.
fn covering(map: &BTreeMap<usize, Piece>, address: usize, len: usize) -> Option<Vec<(usize, Piece)>> {
    let end = address.checked_add(len)?;
    let mut out = Vec::new();
    let mut position = address;
    while position < end {
        let (base, piece) = containing(map, position)?;
        out.push((base, piece));
        position = base + piece.len;
    }
    Some(out)
}

/// Give `[address, address + len)` the kind `kind`, splitting whatever pieces it cuts through.
/// The range must be covered by the ledger (callers check with [`covering`] first).
fn retag(map: &mut BTreeMap<usize, Piece>, address: usize, len: usize, kind: Kind) {
    let end = address + len;
    let Some(pieces) = covering(map, address, len) else {
        debug_assert!(false, "retag of a range the ledger does not cover");
        return;
    };
    for (base, piece) in pieces {
        map.remove(&base);
        let piece_end = base + piece.len;
        if base < address {
            map.insert(base, Piece { len: address - base, kind: piece.kind });
        }
        if piece_end > end {
            map.insert(end, Piece { len: piece_end - end, kind: piece.kind });
        }
    }
    map.insert(address, Piece { len, kind });
}

/// Record a range the kernel has just handed out. Anything the ledger still had there was given
/// back behind the seam's back (the kernel would not have returned the address otherwise), so it is
/// dropped rather than trusted.
fn record_fresh(map: &mut BTreeMap<usize, Piece>, address: usize, len: usize, kind: Kind) {
    let end = address + len;
    let stale: Vec<usize> = map
        .range(..end)
        .filter(|(&base, piece)| base + piece.len > address)
        .map(|(&base, _)| base)
        .collect();
    debug_assert!(stale.is_empty(), "the kernel returned {address:#x}, which the ledger still held");
    for base in stale {
        map.remove(&base);
    }
    map.insert(address, Piece { len, kind });
}

fn refused(operation: &'static str, address: usize, size: usize) -> VmError {
    VmError::Os { operation, address, size, source: OsError(REFUSED) }
}

fn os(operation: &'static str, address: usize, size: usize, code: u32) -> VmError {
    VmError::Os { operation, address, size, source: OsError(code) }
}

/// Put a `PROT_NONE` private anonymous mapping over `[address, address + len)`: the placeholder
/// state, and the one call measured to return both RSS and `VM_ACCOUNT` (see the module docs).
///
/// # Safety
///
/// The caller owns the range, per the ledger, and nothing may hold a reference into it.
unsafe fn placeholder_over(address: usize, len: usize) -> Result<(), u32> {
    // SAFETY: the caller's contract. MAP_FIXED replaces exactly this range and nothing else.
    unsafe {
        posix::mmap(
            address,
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    }
    .map(|_| ())
}

// -------------------------------------------------------------------------------------------
// Reservation
// -------------------------------------------------------------------------------------------

/// The flags every reservation is made with. **Not** `MAP_NORESERVE`: see the module docs for the
/// measurement that rules it out.
const RESERVE_FLAGS: libc::c_int = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

fn reserve_inner(operation: &'static str, size: usize, align: usize, kind: Kind) -> VmResult<usize> {
    let page = page_size();
    let len = round_up(size, page).ok_or_else(|| os(operation, 0, size, libc::ENOMEM as u32))?;
    let align = align.max(page);
    // Over-reserve and trim when the alignment is above a page. Linux can unmap part of a mapping,
    // which Windows cannot, so this is the ordinary way rather than a workaround.
    let span = if align > page {
        len.checked_add(align).ok_or_else(|| os(operation, 0, size, libc::ENOMEM as u32))?
    } else {
        len
    };
    // SAFETY: a NULL hint without MAP_FIXED asks the kernel for fresh address space; nothing that
    // exists is replaced, and PROT_NONE makes nothing reachable.
    let raw = unsafe { posix::mmap(0, span, libc::PROT_NONE, RESERVE_FLAGS, -1, 0) }
        .map_err(|code| os(operation, 0, size, code))?;
    let base = round_up(raw, align).expect("an address the kernel returned rounds up in range");
    if base > raw {
        // SAFETY: `[raw, base)` is the head of the mapping made just above, which nothing else knows.
        unsafe { posix::munmap(raw, base - raw) }.map_err(|code| os(operation, raw, base - raw, code))?;
    }
    let tail = raw + span;
    if tail > base + len {
        // SAFETY: as above, for the tail.
        unsafe { posix::munmap(base + len, tail - (base + len)) }
            .map_err(|code| os(operation, base + len, tail - (base + len), code))?;
    }
    record_fresh(&mut ledger(), base, len, kind);
    Ok(base)
}

pub(super) fn reserve(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner("reserve", size, align, Kind::Plain)
}

pub(super) fn reserve_placeholder(size: usize, align: usize) -> VmResult<usize> {
    reserve_inner("reserve_placeholder", size, align, Kind::Placeholder)
}

/// Split a placeholder: **no kernel call**, because a later `mmap(MAP_FIXED)` of the piece will
/// replace exactly the piece. The ledger records the new boundaries, which is what makes the
/// exact-size rule of [`commit_placeholder`] and [`map_file`] checkable.
///
/// The range must lie inside one placeholder. More permissive than Windows in one way: splitting
/// out a range that is already exactly one placeholder succeeds and changes nothing, where Windows
/// refuses it (487) -- a no-op refused is not a property anything relies on.
pub(super) fn split_placeholder(piece_base: usize, size: usize) -> VmResult<()> {
    const OP: &str = "split_placeholder";
    let mut map = ledger();
    match containing(&map, piece_base) {
        Some((base, piece))
            if piece.kind == Kind::Placeholder && piece_base + size <= base + piece.len =>
        {
            retag(&mut map, piece_base, size, Kind::Placeholder);
            Ok(())
        }
        _ => Err(refused(OP, piece_base, size)),
    }
}

/// Merge adjacent placeholders: no kernel call, as [`split_placeholder`]. The range must be exactly
/// a run of whole placeholders -- it starts at one's base and ends at one's end -- which is Windows'
/// rule that a range holding a view or private commit is refused rather than partially merged.
/// A run of one placeholder is accepted (Windows refuses it with 487; see [`split_placeholder`]).
pub(super) fn coalesce_placeholders(address: usize, size: usize) -> VmResult<()> {
    const OP: &str = "coalesce_placeholders";
    let mut map = ledger();
    let Some(pieces) = covering(&map, address, size) else {
        return Err(refused(OP, address, size));
    };
    let whole = pieces.first().is_some_and(|&(base, _)| base == address)
        && pieces.last().is_some_and(|&(base, piece)| base + piece.len == address + size);
    if !whole || pieces.iter().any(|(_, piece)| piece.kind != Kind::Placeholder) {
        return Err(refused(OP, address, size));
    }
    for (base, _) in pieces {
        map.remove(&base);
    }
    map.insert(address, Piece { len: size, kind: Kind::Placeholder });
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Commit and decommit
// -------------------------------------------------------------------------------------------

/// `mprotect` inside a plain reservation.
///
/// Charged at the call: the reservation's VMA carries no `VM_NORESERVE`, so `mprotect_fixup()`
/// sets `VM_ACCOUNT` on the range as it becomes writable -- measured +65,536 kB of `Committed_AS`
/// for 64 MiB before a byte was touched. Committing an already-committed range changes only its
/// protection and keeps its contents, which is what `MEM_COMMIT` does and what a fault handler that
/// races another one for the same page needs (`tests/fault_teardown_race_linux.rs`).
///
/// **A non-writable commit is not charged on Linux**, and that is the kernel being exact rather than
/// this backend being loose: a private page that can never be written can only ever be the zero
/// page, so there is nothing to promise. It is charged the moment [`protect`] makes it writable.
pub(super) fn commit(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    const OP: &str = "commit";
    let map = ledger();
    match containing(&map, address) {
        Some((base, piece)) if piece.kind == Kind::Plain && address + size <= base + piece.len => {}
        _ => return Err(refused(OP, address, size)),
    }
    // SAFETY: the ledger says the range is inside a live plain reservation this process owns.
    unsafe { posix::mprotect(address, size, posix::prot_bits(protection)) }
        .map_err(|code| os(OP, address, size, code))
}

/// Replace an exact-size placeholder with fresh private memory: `mmap(MAP_FIXED)`, zero-filled, and
/// charged at the call for a writable protection (not for the others, as [`commit`] explains).
pub(super) fn commit_placeholder(
    address: usize,
    size: usize,
    protection: Protection,
) -> VmResult<()> {
    const OP: &str = "commit_placeholder";
    let mut map = ledger();
    require_exact_placeholder(&map, OP, address, size)?;
    // SAFETY: the ledger says `[address, address + size)` is exactly one placeholder this process
    // owns; a placeholder is PROT_NONE, so nothing can hold a reference into it.
    let result = unsafe {
        posix::mmap(
            address,
            size,
            posix::prot_bits(protection),
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
            -1,
            0,
        )
    };
    if let Err(code) = result {
        // A failed MAP_FIXED may already have removed what was there. Put the placeholder back so
        // that the ledger's "this is a placeholder" is still true of the range.
        // SAFETY: as above; the range is ours and holds nothing anyone references.
        let _ = unsafe { placeholder_over(address, size) };
        return Err(os(OP, address, size, code));
    }
    retag(&mut map, address, size, Kind::Private);
    Ok(())
}

/// The exact-size rule: a placeholder must be replaced whole, by something exactly its size.
fn require_exact_placeholder(
    map: &BTreeMap<usize, Piece>,
    operation: &'static str,
    address: usize,
    size: usize,
) -> VmResult<()> {
    match map.get(&address) {
        Some(piece) if piece.kind == Kind::Placeholder && piece.len == size => Ok(()),
        _ => Err(VmError::PlaceholderNotExactSize {
            operation,
            address,
            size,
            source: OsError(REFUSED),
        }),
    }
}

/// Return a plain reservation's pages: a fresh `PROT_NONE` mapping over them. Frees the pages and
/// the `VM_ACCOUNT` charge together (the module docs have the table), and a later [`commit`] reads
/// back zeroes.
pub(super) fn decommit(address: usize, size: usize) -> VmResult<()> {
    const OP: &str = "decommit";
    let map = ledger();
    match containing(&map, address) {
        Some((base, piece)) if piece.kind == Kind::Plain && address + size <= base + piece.len => {}
        _ => return Err(refused(OP, address, size)),
    }
    // SAFETY: the ledger says the range is inside a live plain reservation; the caller's contract is
    // that nothing holds a reference into it.
    unsafe { placeholder_over(address, size) }.map_err(|code| os(OP, address, size, code))
}

/// Return private commit to placeholder state. Any sub-range of private commit may be given back,
/// leaving its neighbours' contents intact, which is Windows' measured rule and what `omni-mem`'s
/// region map relies on when it carves a committed granule in bookkeeping alone. More permissive
/// than Windows: the range may span several adjacent private pieces.
pub(super) fn decommit_to_placeholder(address: usize, size: usize) -> VmResult<()> {
    const OP: &str = "decommit_to_placeholder";
    let mut map = ledger();
    let Some(pieces) = covering(&map, address, size) else {
        return Err(refused(OP, address, size));
    };
    if pieces.iter().any(|(_, piece)| piece.kind != Kind::Private) {
        return Err(refused(OP, address, size));
    }
    // SAFETY: the ledger says the whole range is private commit this process owns; the caller's
    // contract is that nothing holds a reference into it.
    unsafe { placeholder_over(address, size) }.map_err(|code| os(OP, address, size, code))?;
    retag(&mut map, address, size, Kind::Placeholder);
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Protection
// -------------------------------------------------------------------------------------------

/// `mprotect`, after checking the range is committed or mapped end to end.
///
/// There is no flavour decision to make here, unlike Windows: `mprotect(PROT_WRITE)` on a
/// `MAP_PRIVATE` file view *is* copy-on-write, and on a `MAP_SHARED` one it stays shared, because
/// the sharing is a property of the mapping and not of the protection. The kernel also refuses
/// `PROT_WRITE` on a shared view of a descriptor that was not open for writing (`EACCES`).
///
/// `PROT_EXEC` on a view of a file opened [`MapExecutability::NonExecutable`] is refused with
/// `EACCES` -- the code Linux gives for `PROT_EXEC` on a `noexec` mount -- because the decision is
/// made when the file is opened, on every backend (D11). **One Windows refusal is not reproduced:**
/// Windows also refuses to raise a view *created* read-only to execute-read even from an executable
/// section (87, measured there); Linux allows it, nothing in the runtime relies on the refusal, and
/// inventing it here would be a failure the host does not have.
///
/// A placeholder is refused, since it has no pages. A plain reservation is not checked for which of
/// its pages are committed: **`protect` on an uncommitted page of a plain reservation commits it**
/// here, where Windows refuses. Only tests use plain reservations; `GuestSpace` uses placeholders,
/// whose state the ledger does track.
pub(super) fn protect(address: usize, size: usize, protection: Protection) -> VmResult<()> {
    const OP: &str = "protect";
    let map = ledger();
    let Some(pieces) = covering(&map, address, size) else {
        return Err(refused(OP, address, size));
    };
    for (_, piece) in &pieces {
        match piece.kind {
            Kind::Placeholder => return Err(refused(OP, address, size)),
            Kind::View { executable: false } if protection.is_executable() => {
                return Err(os(OP, address, size, libc::EACCES as u32));
            }
            _ => {}
        }
    }
    // SAFETY: the ledger says the range is committed or mapped memory this process owns.
    unsafe { posix::mprotect(address, size, posix::prot_bits(protection)) }
        .map_err(|code| os(OP, address, size, code))
}

// -------------------------------------------------------------------------------------------
// Unmap and release
// -------------------------------------------------------------------------------------------

/// Check that `[address, address + size)` is one whole view, answering exactly as Windows does
/// when it is not: [`VmError::NotViewBase`] for an address inside a view, and
/// [`VmError::ViewSizeMismatch`] for a view base with the wrong length.
///
/// Linux *could* unmap part of a view; the seam's contract is that this call does not, because on
/// Windows it cannot, and `omni-mem` emulates the partial case above the seam on every backend. A
/// backend that quietly did more would make code that is correct here wrong there.
fn require_whole_view(
    map: &BTreeMap<usize, Piece>,
    operation: &'static str,
    address: usize,
    size: usize,
    section_views_allowed: bool,
) -> VmResult<()> {
    let is_view = |kind: Kind| {
        matches!(kind, Kind::View { .. }) || (section_views_allowed && kind == Kind::SectionView)
    };
    match containing(map, address) {
        Some((base, piece)) if is_view(piece.kind) => {
            if base != address {
                return Err(VmError::NotViewBase {
                    address,
                    view_base: base,
                    view_len: piece.len,
                    offset: address - base,
                });
            }
            if piece.len != size {
                return Err(VmError::ViewSizeMismatch {
                    operation,
                    address,
                    requested: size,
                    view_len: piece.len,
                    surviving: piece.len.saturating_sub(size),
                });
            }
            Ok(())
        }
        _ => Err(refused(operation, address, size)),
    }
}

/// Unmap a file view and leave a placeholder: a fresh `PROT_NONE` mapping over it, so the address
/// stays owned by this process.
pub(super) fn unmap(address: usize, size: usize) -> VmResult<()> {
    const OP: &str = "unmap";
    let mut map = ledger();
    require_whole_view(&map, OP, address, size, false)?;
    // SAFETY: the ledger says this is one whole view this process owns; the caller's contract is
    // that nothing holds a reference into it.
    unsafe { placeholder_over(address, size) }.map_err(|code| os(OP, address, size, code))?;
    retag(&mut map, address, size, Kind::Placeholder);
    Ok(())
}

/// Unmap a file or section view and give the address back: `munmap`.
pub(super) fn unmap_and_release(address: usize, size: usize) -> VmResult<()> {
    const OP: &str = "unmap_and_release";
    let mut map = ledger();
    require_whole_view(&map, OP, address, size, true)?;
    // SAFETY: as `unmap`.
    unsafe { posix::munmap(address, size) }.map_err(|code| os(OP, address, size, code))?;
    map.remove(&address);
    Ok(())
}

/// Release a reservation: `munmap`, after checking that the ledger has an allocation starting at
/// `base` of exactly the rounded length.
///
/// Both refusals are the ones Windows gives, and on Linux they matter more, not less: `munmap` of an
/// address this process released a moment ago succeeds, and unmaps whatever the kernel has put
/// there since -- another thread's heap, another instance's view. So a double release is refused
/// ([`VmError::Os`], `EINVAL`), and so is a release of a split placeholder's parent
/// ([`VmError::ReleaseExtentMismatch`], with both extents), where Windows would have freed only the
/// first piece and Linux would have freed all of them *and* every view and commit inside.
pub(super) fn release(base: usize, len: usize, _kind: ReservationKind) -> VmResult<()> {
    const OP: &str = "release";
    let page = page_size();
    let requested = round_up(len, page).ok_or_else(|| refused(OP, base, len))?;
    let mut map = ledger();
    let Some(&piece) = map.get(&base) else {
        return Err(refused(OP, base, len));
    };
    if !matches!(piece.kind, Kind::Plain | Kind::Placeholder | Kind::Private) {
        return Err(refused(OP, base, len));
    }
    if piece.len != requested {
        return Err(VmError::ReleaseExtentMismatch { address: base, requested, actual: piece.len });
    }
    // SAFETY: the ledger says `[base, base + requested)` is one allocation this process owns and
    // that the seam has not released.
    unsafe { posix::munmap(base, requested) }.map_err(|code| os(OP, base, len, code))?;
    map.remove(&base);
    Ok(())
}

/// `true`: `mmap(MAP_FIXED)` places a mapping at an address of this process's choosing over any
/// page-aligned sub-range of its own reservation, which is everything the placeholder path is for.
/// It is a libc symbol linked at build time, so there is nothing to probe.
pub(super) fn placeholder_api_available() -> bool {
    true
}

/// Empty: nothing is resolved at runtime on Linux.
pub(super) fn placeholder_api_symbols() -> Vec<(&'static str, bool)> {
    Vec::new()
}

// -------------------------------------------------------------------------------------------
// Files
// -------------------------------------------------------------------------------------------

/// A file open for mapping. There is no section object on Linux: `mmap` takes the descriptor, and
/// the sharing and the protection are chosen per view.
pub struct MappableFile {
    file: File,
    len: u64,
    executability: MapExecutability,
    path: PathBuf,
    /// Views are `MAP_SHARED`: the file came from [`share_file_for_mapping`].
    shared: bool,
}

impl MappableFile {
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    pub(super) fn executability(&self) -> MapExecutability {
        self.executability
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn is_shared(&self) -> bool {
        self.shared
    }
}

/// Open a file read-only for mapping.
///
/// Linux has no open-time execute right on a descriptor: `PROT_EXEC` is decided per mapping, and
/// the kernel refuses it only for a file on a `noexec` mount (or by an LSM policy). D11's rule is
/// that the decision is made **at open**, so that `.text` that can never be executed fails here
/// rather than at the first instruction. So for [`MapExecutability::Executable`] this *asks the
/// kernel*: it maps the first page `PROT_READ | PROT_EXEC` and unmaps it again. A refusal is
/// [`VmError::SectionCreate`] naming the protection that was refused, the Windows spelling of the
/// same failure. A [`MapExecutability::NonExecutable`] file is refused execute later, by
/// [`protect`] and by the seam's `map_file` check.
pub(super) fn open_file_for_mapping(
    path: &Path,
    executability: MapExecutability,
) -> VmResult<MappableFile> {
    let shown = || path.display().to_string();
    let file = File::open(path).map_err(|error| VmError::FileOpen {
        path: shown(),
        executability,
        source: OsError(error.raw_os_error().unwrap_or(0) as u32),
    })?;
    let len = posix::file_len(file.as_raw_fd()).map_err(|code| VmError::FileOpen {
        path: shown(),
        executability,
        source: OsError(code),
    })?;
    if len == 0 {
        return Err(VmError::EmptyFile { path: shown() });
    }
    if executability == MapExecutability::Executable {
        let page = page_size();
        // SAFETY: a NULL hint without MAP_FIXED replaces nothing; the probe mapping is unmapped
        // straight away and never dereferenced.
        match unsafe {
            posix::mmap(0, page, libc::PROT_READ | libc::PROT_EXEC, libc::MAP_PRIVATE, file.as_raw_fd(), 0)
        } {
            // SAFETY: the probe mapping made on the line above, known to nothing else.
            Ok(probe) => unsafe {
                let _ = posix::munmap(probe, page);
            },
            Err(code) => {
                return Err(VmError::SectionCreate {
                    path: shown(),
                    len,
                    section_protection: "PROT_READ | PROT_EXEC",
                    source: OsError(code),
                });
            }
        }
    }
    Ok(MappableFile { file, len, executability, path: path.to_path_buf(), shared: false })
}

/// Keep the caller's descriptor, so that every view of it is `MAP_SHARED`.
///
/// Linux refuses a writable `MAP_SHARED` mapping of a descriptor not open `O_RDWR` with `EACCES`
/// -- at map time, and at `mprotect` time for a view later raised. Windows refuses the same thing
/// when the section is created. The check is made here, from the descriptor's own access mode, so
/// that both backends refuse at the same step: [`VmError::SectionCreate`] carrying `EACCES`.
pub(super) fn share_file_for_mapping(file: File, name: &Path) -> VmResult<MappableFile> {
    let shown = || name.display().to_string();
    let fd = file.as_raw_fd();
    let len = posix::file_len(fd).map_err(|code| VmError::FileOpen {
        path: shown(),
        executability: MapExecutability::NonExecutable,
        source: OsError(code),
    })?;
    if len == 0 {
        return Err(VmError::EmptyFile { path: shown() });
    }
    let writable = posix::opened_read_write(fd).map_err(|code| VmError::FileOpen {
        path: shown(),
        executability: MapExecutability::NonExecutable,
        source: OsError(code),
    })?;
    if !writable {
        return Err(VmError::SectionCreate {
            path: shown(),
            len,
            section_protection: "MAP_SHARED | PROT_WRITE",
            source: OsError(libc::EACCES as u32),
        });
    }
    Ok(MappableFile {
        file,
        len,
        executability: MapExecutability::NonExecutable,
        path: name.to_path_buf(),
        shared: true,
    })
}

/// `msync(MS_SYNC)`, which on Linux ends in `vfs_fsync_range` and so needs no separate flush of the
/// file to the device.
pub(super) fn sync_view(_file: &MappableFile, address: usize, size: usize) -> VmResult<()> {
    // SAFETY: the caller's contract is that the range is a view of `_file`; msync dereferences
    // nothing, and fails (ENOMEM) rather than touching memory if the range has changed since.
    unsafe { posix::msync(address, size) }.map_err(|code| os("sync_view", address, size, code))
}

/// Map a file view over an exact-size placeholder: `mmap(MAP_FIXED)` from the file's descriptor.
///
/// `MAP_PRIVATE` for a file from [`open_file_for_mapping`], so a [`Protection::ReadWrite`] view is
/// copy-on-write and never reaches the file -- and, being a writable private mapping, is charged
/// its full size in `VM_ACCOUNT` at map time, which is `PAGE_WRITECOPY`'s measured behaviour too.
/// `MAP_SHARED` for a file from [`share_file_for_mapping`], created with the protection asked for:
/// unlike a Windows view, a Linux shared view stays shared whatever it is later protected to.
pub(super) fn map_file(
    file: &MappableFile,
    file_offset: u64,
    size: usize,
    address: usize,
    protection: Protection,
) -> VmResult<()> {
    const OP: &str = "map_file";
    let mut map = ledger();
    require_exact_placeholder(&map, OP, address, size)?;
    let sharing = if file.shared { libc::MAP_SHARED } else { libc::MAP_PRIVATE };
    // SAFETY: the ledger says `[address, address + size)` is exactly one placeholder this process
    // owns, so nothing can hold a reference into it; the descriptor is live for this call.
    let result = unsafe {
        posix::mmap(
            address,
            size,
            posix::prot_bits(protection),
            sharing | libc::MAP_FIXED,
            file.file.as_raw_fd(),
            file_offset,
        )
    };
    if let Err(code) = result {
        // SAFETY: as in `commit_placeholder`: restore the placeholder a failed MAP_FIXED may have
        // removed.
        let _ = unsafe { placeholder_over(address, size) };
        return Err(os(OP, address, size, code));
    }
    retag(
        &mut map,
        address,
        size,
        Kind::View { executable: file.executability == MapExecutability::Executable },
    );
    Ok(())
}

// -------------------------------------------------------------------------------------------
// The D12 section
// -------------------------------------------------------------------------------------------

/// A `memfd`: anonymous shared memory with a descriptor, which can be mapped twice.
pub struct SharedSection {
    fd: OwnedFd,
    len: u64,
}

impl SharedSection {
    pub(super) fn len(&self) -> u64 {
        self.len
    }
}

/// `MFD_EXEC` (Linux 6.3): ask for an executable memfd explicitly. With `vm.memfd_noexec = 1` a
/// memfd created without it is sealed non-executable, and the arena's RX view would be refused.
const MFD_EXEC: libc::c_uint = 0x0010;

/// `memfd_create` plus `ftruncate`.
///
/// **Not charged at creation**, which is where it differs from a pagefile-backed Windows section
/// (committed in full when created). A memfd is shmem created `VM_NORESERVE`: its pages are charged
/// to `Committed_AS` one at a time as they are first written (`shmem_acct_blocks`), and neither view
/// is `VM_ACCOUNT`, so -- exactly as on Windows -- none of it appears in
/// [`process_commit_charge`]. `omni-mem`'s `CommitBudget` is what reports it.
pub(super) fn create_shared_section(size: u64) -> VmResult<SharedSection> {
    let create = |flags: libc::c_uint| {
        // SAFETY: a NUL-terminated name and a flag word; the call returns a new descriptor or -1.
        let fd = unsafe { libc::memfd_create(c"omnidroid-code".as_ptr(), flags) };
        if fd < 0 {
            Err(posix::errno())
        } else {
            // SAFETY: `fd` is a descriptor this call just created and nothing else owns.
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    };
    let section_error = |code: u32| VmError::SectionCreate {
        path: "<memfd>".to_string(),
        len: size,
        section_protection: "memfd_create(MFD_CLOEXEC | MFD_EXEC)",
        source: OsError(code),
    };
    // A kernel older than 6.3 rejects the unknown flag with EINVAL, and on such a kernel every
    // memfd is executable anyway.
    let fd = match create(libc::MFD_CLOEXEC | MFD_EXEC) {
        Err(code) if code == libc::EINVAL as u32 => create(libc::MFD_CLOEXEC),
        other => other,
    }
    .map_err(section_error)?;
    let Ok(length) = libc::off_t::try_from(size) else {
        return Err(section_error(libc::EFBIG as u32));
    };
    // SAFETY: `fd` is the live memfd created above.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), length) } != 0 {
        return Err(section_error(posix::errno()));
    }
    Ok(SharedSection { fd, len: size })
}

/// `mmap(NULL, MAP_SHARED)` of the memfd: `MAP_SHARED`, never `MAP_PRIVATE`, or the RW view's
/// writes would be privatised and never reach the RX view.
pub(super) fn map_section(
    section: &SharedSection,
    offset: u64,
    size: usize,
    protection: Protection,
) -> VmResult<usize> {
    const OP: &str = "map_section";
    // SAFETY: a NULL hint without MAP_FIXED replaces nothing; the descriptor is live.
    let address = unsafe {
        posix::mmap(0, size, posix::prot_bits(protection), libc::MAP_SHARED, section.fd.as_raw_fd(), offset)
    }
    .map_err(|code| os(OP, 0, size, code))?;
    record_fresh(&mut ledger(), address, size, Kind::SectionView);
    Ok(address)
}

// -------------------------------------------------------------------------------------------
// This process's memory, from /proc/self
// -------------------------------------------------------------------------------------------

/// Read a `/proc/self` file whole. Not on any hot path: these are measurement calls.
fn read_proc(name: &'static str) -> VmResult<String> {
    std::fs::read_to_string(name).map_err(|error| VmError::Os {
        operation: name,
        address: 0,
        size: 0,
        source: OsError(error.raw_os_error().unwrap_or(libc::EPROTO) as u32),
    })
}

/// A `/proc/self` file that did not have the shape the kernel writes.
fn malformed(name: &'static str) -> VmError {
    VmError::Os { operation: name, address: 0, size: 0, source: OsError(libc::EPROTO as u32) }
}

/// The sum of the sizes of this process's VMAs that carry `VM_ACCOUNT` (`ac` in `VmFlags`).
///
/// That flag is exactly what the kernel charges to `Committed_AS` for this process's own mappings:
/// `vm_acct_memory` is called for a VMA's pages when it gains the flag and `vm_unacct_memory` when it
/// loses it or goes away. So it is the faithful per-process counterpart of Windows' `PrivateUsage`:
/// private writable memory, charged at commit rather than at touch, and **not** shared file views or
/// the memfd section. Two measured differences: page tables are not in it (Windows charges them,
/// about `size/512`), and glibc's `MAP_NORESERVE` malloc arenas are not in it either (they are not
/// charged to `Committed_AS` at all on this host's overcommit setting).
///
/// Read from `smaps`, which walks every VMA's page tables to count residency, so it costs in
/// proportion to what is mapped. It is a measurement call, and the one statm field built on it is
/// read by the engine rarely.
fn accounted_bytes() -> VmResult<u64> {
    const NAME: &str = "/proc/self/smaps";
    let text = read_proc(NAME)?;
    let mut size_kb: Option<u64> = None;
    let mut total_kb = 0u64;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Size:") {
            let value = rest.trim().trim_end_matches("kB").trim();
            size_kb = Some(value.parse().map_err(|_| malformed(NAME))?);
        } else if let Some(flags) = line.strip_prefix("VmFlags:") {
            let size = size_kb.take().ok_or_else(|| malformed(NAME))?;
            if flags.split_whitespace().any(|flag| flag == "ac") {
                total_kb += size;
            }
        }
    }
    Ok(total_kb * 1024)
}

/// `/proc/self/statm`'s first three fields, in bytes: size, resident, shared.
fn statm() -> VmResult<(u64, u64, u64)> {
    const NAME: &str = "/proc/self/statm";
    let text = read_proc(NAME)?;
    let mut fields = text.split_whitespace().map(|field| field.parse::<u64>());
    let mut next = || -> VmResult<u64> {
        match fields.next() {
            Some(Ok(pages)) => Ok(pages * page_size() as u64),
            _ => Err(malformed(NAME)),
        }
    };
    Ok((next()?, next()?, next()?))
}

/// `start_code..end_code` from `/proc/self/stat` (fields 26 and 27): the span the ELF loader set
/// from the executable's `PF_X` segments, and only the executable's.
fn executable_code() -> VmResult<core::ops::Range<usize>> {
    const NAME: &str = "/proc/self/stat";
    let text = read_proc(NAME)?;
    // The command name (field 2) is in parentheses and may itself contain spaces and parentheses,
    // so the fields are counted from after the *last* ')'. Field 3 is the first one after it.
    let after = text.rfind(')').map(|at| &text[at + 1..]).ok_or_else(|| malformed(NAME))?;
    let mut fields = after.split_whitespace().skip(26 - 3);
    let mut next = || -> VmResult<usize> {
        fields.next().and_then(|f| f.parse().ok()).ok_or_else(|| malformed(NAME))
    };
    let (start, end) = (next()?, next()?);
    if start == 0 || end <= start {
        return Err(VmError::ExecutableImage {
            base: start,
            reason: "/proc/self/stat reports no code span for this executable",
        });
    }
    Ok(start..end)
}

/// Bytes charged to `Committed_AS` by this process's own mappings. See [`accounted_bytes`].
pub(super) fn process_commit_charge() -> VmResult<u64> {
    accounted_bytes()
}

/// Resident bytes: `/proc/self/statm`'s `resident`, which is the same `get_mm_rss()` that
/// `/proc/self/status` prints as `VmRSS`.
pub(super) fn process_working_set() -> VmResult<u64> {
    Ok(statm()?.1)
}

/// The host's own `/proc/self/statm` (size, resident, shared -- one read, so `shared <= resident`
/// holds), the `VM_ACCOUNT` total, and the executable's code span.
pub(super) fn process_memory() -> VmResult<super::ProcessMemory> {
    let (address_space, resident, resident_shared) = statm()?;
    Ok(super::ProcessMemory {
        address_space,
        resident,
        resident_shared,
        commit_charge: accounted_bytes()?,
        executable_code: executable_code()?,
    })
}

// -------------------------------------------------------------------------------------------
// Unit tests of the ledger's decisions, which the public seam reaches only through the kernel
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn piece(len: usize, kind: Kind) -> Piece {
        Piece { len, kind }
    }

    #[test]
    fn retag_splits_what_it_cuts_and_keeps_the_rest() {
        let mut map = BTreeMap::new();
        map.insert(0x1000, piece(0x4000, Kind::Placeholder));
        retag(&mut map, 0x2000, 0x1000, Kind::Private);
        let got: Vec<_> = map.iter().map(|(&b, &p)| (b, p.len, p.kind)).collect();
        assert_eq!(
            got,
            vec![
                (0x1000, 0x1000, Kind::Placeholder),
                (0x2000, 0x1000, Kind::Private),
                (0x3000, 0x2000, Kind::Placeholder),
            ]
        );
        // Across two pieces, the whole of each is accounted for.
        retag(&mut map, 0x1800, 0x1000, Kind::Placeholder);
        let covered: usize = map.values().map(|p| p.len).sum();
        assert_eq!(covered, 0x4000, "retag must neither lose nor invent bytes: {map:?}");
    }

    #[test]
    fn covering_refuses_a_hole() {
        let mut map = BTreeMap::new();
        map.insert(0x1000, piece(0x1000, Kind::Private));
        map.insert(0x3000, piece(0x1000, Kind::Private));
        assert!(covering(&map, 0x1000, 0x1000).is_some());
        assert!(covering(&map, 0x1000, 0x3000).is_none(), "0x2000..0x3000 is not in the ledger");
        assert!(containing(&map, 0x2000).is_none());
        assert_eq!(containing(&map, 0x1fff).map(|(b, _)| b), Some(0x1000));
    }
}
