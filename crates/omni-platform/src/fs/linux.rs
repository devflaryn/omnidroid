//! Linux backend for the filesystem seam.
//!
//! **Structural only: unverified and not implemented.** The body is the shared
//! [`unix`](super::unix) module, and it covers exactly the two operations with no portable `std`
//! spelling — [`pread`](super::unix::pread) and [`volume_stats`](super::unix::volume_stats). The
//! other fifteen operations the seam offers are `std::fs` and are implemented once for all five
//! targets, which is D22's rule applied in the direction it points.
//!
//! Linux-specific notes for whoever implements these:
//!
//! * `pread(2)` is `std::os::unix::fs::FileExt::read_at`, one call, no loop. See the shared
//!   module for why looping would be wrong rather than merely unnecessary.
//! * `statvfs(3)` on Linux fills `f_frsize` **and** `f_bsize`, and they are different fields with
//!   different meanings. The block counts are in `f_frsize` units.
//! * The **confinement** in [`path`](super::path) would be strictly better here than it is on
//!   Windows, and this is the one place where a unix implementation is not merely a translation:
//!   `openat(2)` with `O_NOFOLLOW` per component, from a directory descriptor held open on the
//!   root, closes the symlink race the shared design documents as open. That is a real
//!   improvement to make on this target rather than a like-for-like port, and the module says so
//!   where the race is admitted.
//! * The guest is an Android ARM64 binary and this host would be a Linux ARM64 one, which is the
//!   configuration `ARCHITECTURE.md` section 6 runs guest code **natively** on — so the guest's
//!   own `struct stat` and the host's would be the same layout, and the adapter's encoder would
//!   be doing a conversion that happens to be the identity. That is the target where a layout
//!   error in it would be hardest to notice.

pub(super) use super::unix::{allocate, pread, pwrite, volume_stats};
