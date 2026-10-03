//! The engine-native, Magisk-compatible rooted device (docs/superpowers/specs/2026-10-03-magisk-root-design.md).
//! R1: the per-instance root profile, the module catalog, the root layer over /system, the `su`
//! tool and the `omni_root` syscall. When an instance has no profile, nothing here takes effect.
pub mod profile;
pub use profile::{Profile, Shamiko, SuPolicy};
