//! The `omni_root` syscall: the engine's own door to root. A non-rooted device answers `ENOSYS`,
//! exactly like an unimplemented syscall.
use crate::errno::{Errno, SysResult, EINVAL, ENOSYS};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const OP_ELEVATE: u64 = 1;
const OP_STATUS: u64 = 2;

fn sys_omni_root(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let profile = super::profile_of(p).filter(|pr| pr.is_rooted()).ok_or(ENOSYS)?;
    match a[0] {
        OP_ELEVATE => {
            let target = if a[1] == u64::from(u32::MAX) { 0 } else { a[1] as u32 };
            let ids = profile.elevation(p.sys.uid(), target).map_err(Errno)?;
            p.sys.assume(ids.uid, ids.gid, ids.groups, ids.caps);
            Ok(0)
        }
        OP_STATUS => Ok(u64::from(profile.magisk_version_code())),
        _ => Err(EINVAL),
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::OMNI_ROOT, sys_omni_root);
}
