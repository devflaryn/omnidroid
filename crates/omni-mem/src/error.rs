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
