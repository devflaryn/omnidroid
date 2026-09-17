//! Linux backend for the virtual-memory seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, where every operation returns
//! [`VmError::Unsupported`](super::VmError::Unsupported) so that a Linux build fails at the first
//! virtual-memory call rather than appearing to work. See that module for the intended `mmap`
//! mapping of each operation and for what has to be measured first.
//!
//! Linux-specific notes for whoever implements it:
//!
//! * `MAP_NORESERVE` plus `vm.overcommit_memory = 0` is what makes a reservation far larger than
//!   RAM free, but under `vm.overcommit_memory = 2` (strict accounting) it is not, and the design
//!   has to cope with hosts configured that way. Windows always behaves like the strict case for
//!   *commit* and like the permissive case for *reserve*; Linux's behaviour depends on a sysctl.
//! * The JIT arena's dual RW/RX mapping is `memfd_create` plus two `mmap`s of the same fd (D12).
//!   `memfd_create` needs Linux 3.17 or later, and some hardened configurations refuse an
//!   executable mapping of a memfd.
//! * `MADV_DONTNEED` on a private anonymous mapping frees the pages and later reads return zero,
//!   which matches the measured Windows `MEM_DECOMMIT` semantics. That correspondence is the one
//!   thing here that can be relied on without measurement.

pub(super) use super::unix::{
    allocation_granularity, commit, commit_placeholder, decommit, decommit_to_placeholder,
    map_file, open_file_for_mapping, page_size, process_commit_charge, process_working_set,
    protect, release, reserve, reserve_placeholder, split_placeholder, unmap, unmap_and_release,
    MappableFile, MISALIGNED_OS_ERROR,
};
