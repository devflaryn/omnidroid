//! The engine-native, Magisk-compatible rooted device (docs/superpowers/specs/2026-10-03-magisk-root-design.md).
//! R1: the per-instance root profile, the module catalog, the root layer over /system, the `su`
//! tool and the `omni_root` syscall. When an instance has no profile, nothing here takes effect.
pub mod syscall;
pub mod profile;
pub use profile::{Profile, Shamiko, SuPolicy};
pub mod module;
pub use module::{builtin_dir, user_dir, Catalog, Module, ModuleProp, ModuleSource};

use std::sync::Arc;

use crate::process::Process;

/// TEMPORARY (Task 5 replaces this with the cached `Profile::of`): the instance's profile, read
/// and parsed from `<instance>/data/adb/omni/profile` on every call.
pub(crate) fn profile_of(p: &Process) -> Option<Arc<Profile>> {
    let path = p.vfs.binds().instance_dir()?.join("data/adb/omni/profile");
    Some(Arc::new(Profile::parse(&std::fs::read_to_string(path).ok()?)))
}
