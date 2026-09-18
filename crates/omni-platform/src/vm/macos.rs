//! macOS backend for the virtual-memory seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where every operation returns
//! [`VmError::Unsupported`](super::VmError::Unsupported) so that a macOS build fails at the first
//! virtual-memory call rather than appearing to work. See that module for the intended `mmap`
//! mapping of each operation.
//!
//! macOS-specific notes for whoever implements it, and reasons not to assume this is a small job:
//!
//! * **The page size is 16384 on Apple silicon, not 4096.** The seam already reports it from
//!   `sysconf` rather than assuming, but callers that were written against 4 KB pages — and the
//!   guest, which is an Android ARM64 binary whose segments may be 4 KB-aligned — will need a
//!   different segment-placement strategy. This is the single largest portability risk in the
//!   memory design, and it is a guest-visible one.
//! * `MAP_JIT` plus `pthread_jit_write_protect_np` is the sanctioned way to hold code memory, and
//!   it is *per-thread state*, not per-mapping: it behaves quite differently from the dual-mapped
//!   section measured on Windows (D12), and needs its own measurement rather than a translation.
//!   It also requires the `com.apple.security.cs.allow-jit` entitlement.
//! * There is no per-process commit charge to report, so the assertions the Windows tests make
//!   about [`process_commit_charge`](super::process_commit_charge) have no direct equivalent and
//!   the memory budget has to be expressed differently.

pub(super) use super::unix::{
    allocation_granularity, coalesce_placeholders, commit, commit_placeholder,
    create_shared_section, decommit, decommit_to_placeholder, map_file, map_section,
    open_file_for_mapping, page_size, process_commit_charge, process_working_set, protect, release,
    reserve, reserve_placeholder, split_placeholder, unmap, unmap_and_release, MappableFile,
    SharedSection, MISALIGNED_OS_ERROR,
};
