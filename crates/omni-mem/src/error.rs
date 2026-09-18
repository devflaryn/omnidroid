//! Typed, diagnostic errors for the guest address space and the code arena.
//!
//! Every variant names the operation that failed and the values it failed with (Global
//! Constraint 7). Failures that came from the OS carry the underlying
//! [`VmError`](omni_platform::vm::VmError), which carries the OS code, so a failure can still be
//! traced back to the measurements in `docs/research/windows-memory-model.md`.

use omni_platform::vm::VmError;

use crate::GuestAddr;

/// Result alias for every operation in this crate.
pub type MemResult<T> = Result<T, MemError>;

/// Everything that can go wrong in `omni-mem`.
///
/// There is deliberately no catch-all variant: a new failure mode gets a new variant with the
/// values that explain it.
#[derive(Debug, thiserror::Error)]
pub enum MemError {
    /// A virtual-memory operation failed in the platform layer.
    ///
    /// The address and length are this crate's view of the request, which is usually more
    /// informative than the OS-level one: a guest `munmap` of one page can become several
    /// platform calls, and this says which guest request was being served.
    #[error("`{operation}` on {address:#x}..{end:#x} ({len} bytes) failed: {source}")]
    Platform {
        /// The `omni-mem` operation that was being performed.
        operation: &'static str,
        /// Start of the guest range involved.
        address: GuestAddr,
        /// End of the guest range involved, exclusive.
        end: GuestAddr,
        /// Length of the guest range involved.
        len: usize,
        /// The platform failure.
        source: VmError,
    },

    /// A range was not inside this guest address space.
    #[error(
        "`{operation}`: guest range {address:#x}..{end:#x} is not inside the guest address space \
         {space_base:#x}..{space_end:#x} ({space_len} bytes)"
    )]
    OutsideSpace {
        /// The operation that was called.
        operation: &'static str,
        /// Start of the requested range.
        address: GuestAddr,
        /// End of the requested range, exclusive. Saturated, so a range that wrapped reads as the
        /// maximum address rather than as a smaller number than `address`.
        end: GuestAddr,
        /// Base of the guest address space.
        space_base: GuestAddr,
        /// End of the guest address space, exclusive.
        space_end: GuestAddr,
        /// Length of the guest address space.
        space_len: usize,
    },

    /// A value that must be a multiple of some granularity was not.
    #[error("`{operation}`: {what} is {value:#x}, which is not a multiple of {required:#x}")]
    Misaligned {
        /// The operation that was called.
        operation: &'static str,
        /// Which argument was misaligned, e.g. `"fixed address"`.
        what: &'static str,
        /// The offending value.
        value: u64,
        /// The granularity it must be a multiple of.
        required: u64,
    },

    /// A size argument was zero, which is never meaningful.
    #[error("`{operation}` was called with a size of 0 bytes")]
    ZeroSize {
        /// The operation that was called.
        operation: &'static str,
    },

    /// A fixed-address mapping was demanded over a range that is already in use.
    ///
    /// This is the `mmap(MAP_FIXED_NOREPLACE)` case, not `MAP_FIXED`: nothing is unmapped on the
    /// caller's behalf, because silently replacing a mapping the ELF loader placed earlier is how
    /// a loader bug becomes an unexplainable crash much later.
    #[error(
        "`{operation}`: {requested:#x}..{requested_end:#x} was demanded as a fixed mapping, but \
         {conflict_start:#x}..{conflict_end:#x} is already {conflict} — unmap it first if that is \
         really what you meant"
    )]
    AddressTaken {
        /// The operation that was called.
        operation: &'static str,
        /// Start of the demanded range.
        requested: GuestAddr,
        /// End of the demanded range, exclusive.
        requested_end: GuestAddr,
        /// Start of the region that is in the way.
        conflict_start: GuestAddr,
        /// End of the region that is in the way, exclusive.
        conflict_end: GuestAddr,
        /// What that region is, e.g. `"an anonymous mapping"`.
        conflict: &'static str,
    },

    /// No free range in the guest address space could satisfy the request.
    #[error(
        "`{operation}`: no free range of {len} bytes at alignment {align:#x} in the guest address \
         space; {free} bytes are free in total and the largest single free range is {largest}"
    )]
    NoSpace {
        /// The operation that was called.
        operation: &'static str,
        /// The length that was asked for.
        len: usize,
        /// The alignment that was asked for.
        align: usize,
        /// Total free bytes, to distinguish exhaustion from fragmentation.
        free: usize,
        /// The largest single free range, which is the number that decides the request.
        largest: usize,
    },

    /// An operation that needs a live mapping was given a range that has none.
    #[error(
        "`{operation}`: {address:#x}..{end:#x} is not mapped; {unmapped_start:#x}..\
         {unmapped_end:#x} of it is free address space"
    )]
    NotMapped {
        /// The operation that was called.
        operation: &'static str,
        /// Start of the requested range.
        address: GuestAddr,
        /// End of the requested range, exclusive.
        end: GuestAddr,
        /// Start of the first unmapped part of it.
        unmapped_start: GuestAddr,
        /// End of that unmapped part, exclusive.
        unmapped_end: GuestAddr,
    },

    /// An alignment argument was larger than the guest address space, so no address could satisfy
    /// it.
    ///
    /// Separate from [`MemError::Misaligned`] because the value is a perfectly good power of two and
    /// the problem is its magnitude: the free-range search rounds up to it, which panics in a debug
    /// build and wraps to a spurious "no space" in release.
    #[error(
        "`{operation}`: alignment {align:#x} is larger than the {space_len}-byte guest address \
         space, so no address in it can satisfy the request"
    )]
    AlignmentTooLarge {
        /// The operation that was called.
        operation: &'static str,
        /// The alignment that was asked for.
        align: usize,
        /// Length of the guest address space.
        space_len: usize,
    },

    /// A commit would take this guest address space past its configured commit ceiling.
    ///
    /// Commit charge is the scarce resource (D10, Global Constraint 6), and every quantity that
    /// decides how much to spend ultimately comes from a file: an eight-byte edit to a `PT_LOAD`'s
    /// `p_memsz` was measured to turn a 16.7 MiB load into a 3.4 GiB one. This is the refusal that
    /// stops it, and it names all four numbers so that a caller can tell "the limit is too low" from
    /// "the request is absurd".
    ///
    /// Raise [`GuestSpaceConfig::max_committed`](crate::GuestSpaceConfig::max_committed) if the
    /// instance legitimately needs more.
    #[error(
        "`{operation}`: committing {requested} bytes at {address:#x} would take this guest address \
         space to {would_total} bytes of commit charge, past its ceiling of {limit} \
         ({committed} bytes are committed now); commit charge is the scarce resource — raise \
         GuestSpaceConfig::max_committed if this is legitimate"
    )]
    CommitCeiling {
        /// The operation that was called.
        operation: &'static str,
        /// Start of the range whose commit was refused.
        address: GuestAddr,
        /// Bytes this single request asked to commit.
        requested: usize,
        /// Bytes already committed in this space.
        committed: usize,
        /// What the total would have become.
        would_total: usize,
        /// The configured ceiling.
        limit: usize,
    },

    /// One commit request was larger than the per-request ceiling.
    ///
    /// A [`CommitPolicy::Eager`](crate::CommitPolicy::Eager) mapping commits its whole length in one
    /// call, so in practice this is the refusal an oversized eager mapping gets — including the one
    /// an attacker-controlled `p_memsz` produces. A
    /// [`CommitPolicy::Lazy`](crate::CommitPolicy::Lazy) mapping commits one granule per call and
    /// never reaches this, however large it is.
    #[error(
        "`{operation}`: a single commit of {requested} bytes at {address:#x} exceeds the \
         {limit}-byte per-request ceiling; an eagerly-committed mapping is charged its whole length \
         at once — map it lazily, or raise GuestSpaceConfig::max_commit_request"
    )]
    CommitRequestTooLarge {
        /// The operation that was called.
        operation: &'static str,
        /// Start of the range whose commit was refused.
        address: GuestAddr,
        /// Bytes this single request asked to commit.
        requested: usize,
        /// The configured per-request ceiling.
        limit: usize,
    },

    /// A [`CodeBlock`](crate::CodeBlock) minted by one arena was passed to another.
    ///
    /// The arena hands out plain `Copy` values that hold addresses rather than borrows, so a block
    /// outlives the arena that made it as a *value* even though the memory it names does not. Without
    /// an identity check, `let b = a.alloc(16)?; drop(a); other.write(&b, 0, &bytes)` would be a
    /// write through an unmapped address expressible in entirely safe code — and per-thread code
    /// caches (D5: 20–35 MiB each, not shared between threads) mean several live arenas is the
    /// expected shape.
    #[error(
        "code arena: a block minted by arena {block_arena} was passed to arena {arena}; a block \
         belongs to the arena that allocated it and names memory only that arena owns"
    )]
    ForeignBlock {
        /// Identity of the arena the call was made on.
        arena: u64,
        /// Identity of the arena that minted the block.
        block_arena: u64,
    },

    /// A block's recorded chunk does not hold it.
    ///
    /// Unreachable for a block this arena minted, and returned rather than asserted because the
    /// alternative was an index panic and a `end - start` underflow that in a release build became a
    /// huge page-aligned length handed to the OS.
    #[error(
        "code arena: a block at {write:#x}+{len:#x} names chunk {chunk} of {chunks}, which does not \
         contain it"
    )]
    BlockOutsideChunk {
        /// The block's writable address.
        write: usize,
        /// The block's length.
        len: usize,
        /// The chunk index the block records.
        chunk: usize,
        /// How many chunks the arena has.
        chunks: usize,
    },

    /// A partial unmap of a file-backed view could not re-map a surviving piece, and the
    /// copy-on-write content that piece held is gone.
    ///
    /// Windows cannot partially unmap a view, so the emulation unmaps the whole view and maps the
    /// survivors again. Once the view is gone there is no way back: the survivors that were re-mapped
    /// are intact, and a survivor whose re-map failed is free address space whose privatised pages
    /// existed only in memory. The re-map is attempted for **every** survivor before this is
    /// returned, so the loss is confined to the pieces named here, and the ranges are logged at
    /// `error` level as well as carried in this variant. There is nothing to retry: the content is
    /// not recoverable, and reporting that plainly is the only honest option.
    #[error(
        "`{operation}`: emulating a partial unmap of the view at {view_start:#x}+{view_len:#x} \
         failed to re-map {lost_ranges} surviving range(s) holding {lost_bytes} bytes of \
         copy-on-write content, which is unrecoverable; first failure: {source}"
    )]
    UnmapEmulationLostContent {
        /// The operation that was called.
        operation: &'static str,
        /// Base of the view that was being partially unmapped.
        view_start: GuestAddr,
        /// Length of that view.
        view_len: usize,
        /// How many surviving ranges could not be re-mapped.
        lost_ranges: usize,
        /// How many bytes of preserved copy-on-write content were in them.
        lost_bytes: usize,
        /// The first failure, which is why the emulation could not finish.
        source: Box<MemError>,
    },

    /// The guest address space configuration is not usable.
    #[error("guest address space configuration: {field} is {value:#x}, which {reason}")]
    InvalidConfig {
        /// The configuration field at fault.
        field: &'static str,
        /// The offending value.
        value: u64,
        /// Why it cannot be used.
        reason: &'static str,
    },

    /// The code arena has reached its configured ceiling.
    #[error(
        "code arena: cannot allocate {requested} bytes; {in_use} of {limit} bytes are already \
         mapped and the arena will not grow past its configured limit"
    )]
    ArenaFull {
        /// The size that was asked for.
        requested: usize,
        /// Bytes of arena currently mapped.
        in_use: usize,
        /// The configured ceiling.
        limit: usize,
    },

    /// A write into a code block would have touched a sealed page.
    ///
    /// This is a refusal rather than a fault, and the distinction is the whole reason the variant
    /// exists. [`CodeArena::write`](crate::CodeArena::write) is a **safe** function that stores
    /// through the writable view; [`CodeArena::seal`](crate::CodeArena::seal) makes that view
    /// `PAGE_READONLY`. So without this check `let b = a.alloc(64)?; a.seal(&b)?; a.write(&b, 0,
    /// &code)?` is an access violation reachable from entirely safe code, and an access violation
    /// cannot be contained by any caller.
    ///
    /// It names the page, not just the block, because sealing is **page-granular** while the call
    /// that requests it is block-granular: with the default 16-byte block alignment, sealing one
    /// block seals up to 255 of its page-mates, and a translator patching one of *those* is the way
    /// this is most likely to be hit. Unseal the block that covers `page` first.
    #[error(
        "code arena: a write of {len} bytes at offset {offset} into the block at {write:#x} covers \
         the sealed page {page:#x}; sealing is page-granular, so this block may have been sealed by \
         a page-mate — unseal it before writing"
    )]
    BlockSealed {
        /// The block's writable address.
        write: usize,
        /// Offset within the block the write started at.
        offset: usize,
        /// Length of the attempted write.
        len: usize,
        /// The first sealed page the write would have touched.
        page: usize,
    },

    /// A write into a code block ran past its end.
    #[error(
        "code arena: a write of {len} bytes at offset {offset} runs past the end of a \
         {block_len}-byte block"
    )]
    BlockOverflow {
        /// Offset within the block.
        offset: usize,
        /// Length of the attempted write.
        len: usize,
        /// Length of the block.
        block_len: usize,
    },
}

impl MemError {
    /// The platform failure behind this error, if it came from one.
    ///
    /// Lets a caller or a test assert on the measured OS code without matching on every shape.
    #[must_use]
    pub fn platform_error(&self) -> Option<&VmError> {
        match self {
            MemError::Platform { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Attach guest-range context to a platform failure.
pub(crate) fn platform(
    operation: &'static str,
    address: GuestAddr,
    len: usize,
) -> impl FnOnce(VmError) -> MemError {
    move |source| MemError::Platform {
        operation,
        address,
        end: address.saturating_add(len),
        len,
        source,
    }
}
