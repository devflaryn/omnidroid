//! macOS backend for the filesystem seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, covering the two operations with no portable `std` spelling.
//!
//! macOS-specific notes for whoever implements them, and a reason not to assume this is
//! symmetric with Linux:
//!
//! * `pread(2)` is the same `std::os::unix::fs::FileExt::read_at` here, so that half really is
//!   shared and the two files only differ in what is written around it.
//! * **`statvfs(3)` on macOS is a compatibility wrapper over `statfs(2)`**, and the two do not
//!   agree about everything: `f_flag` carries `MNT_*` bits rather than POSIX's `ST_*`, so a
//!   `read_only` derived by testing `ST_RDONLY` against it would be testing the wrong bit. The
//!   value happens to be 1 in both numberings, which is exactly the kind of coincidence that
//!   makes a wrong test pass until it does not.
//! * APFS is case-insensitive by default and case-preserving, so two guest paths differing only
//!   in case name one file there and two files on Linux. [`path`](super::path) does not normalise
//!   case, because normalising it would make the *opposite* mistake on Linux; the difference is
//!   the host's and is worth knowing about before a guest is trusted to tell two such paths
//!   apart.

pub(super) use super::unix::{pread, pwrite, volume_stats};
