//! The `omni_root` syscall: the engine's own door to root. A non-rooted device answers `ENOSYS`,
//! exactly like an unimplemented syscall.
use crate::errno::{Errno, SysResult, EACCES, EINVAL, ENAMETOOLONG, ENOSYS};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const OP_ELEVATE: u64 = 1;
const OP_STATUS: u64 = 2;
const OP_SETPROP: u64 = 3;
const OP_DELPROP: u64 = 4;

const PROP_NAME_MAX: usize = 256;
const PROP_VALUE_MAX: usize = 8192;

fn guest_str(p: &Process, addr: u64, max: usize) -> Result<String, Errno> {
    let s = String::from_utf8(p.mem.read_cstr(addr, max)?).map_err(|_| EINVAL)?;
    if s.len() > max {
        return Err(ENAMETOOLONG);
    }
    Ok(s)
}

fn sys_omni_root(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    // A hidden (DenyList) process gets nothing, even from a planted su or a raw svc.
    if p.view.hidden {
        return Err(ENOSYS);
    }
    let profile = p.vfs.binds().instance_dir().and_then(super::Profile::of).ok_or(ENOSYS)?;
    match a[0] {
        OP_ELEVATE => {
            let target = if a[1] == u64::from(u32::MAX) { 0 } else { u32::try_from(a[1]).map_err(|_| EINVAL)? };
            let ids = profile.elevation(p.sys.uid(), target).map_err(Errno)?;
            p.sys.assume(ids.uid, ids.gid, ids.groups, ids.caps);
            Ok(0)
        }
        OP_STATUS => Ok(u64::from(profile.magisk_version_code())),
        OP_SETPROP | OP_DELPROP => {
            if p.sys.uid() != 0 {
                return Err(EACCES);
            }
            let name = guest_str(p, a[1], PROP_NAME_MAX)?;
            if name.is_empty() {
                return Err(EINVAL);
            }
            let props = crate::props::PropertyService::global(p.vfs.sysroot());
            if a[0] == OP_SETPROP {
                let value = guest_str(p, a[2], PROP_VALUE_MAX)?;
                if props.set_forced(&name, &value) != crate::props::PROP_SUCCESS {
                    return Err(EINVAL);
                }
            } else {
                props.delete(&name);
            }
            Ok(0)
        }
        _ => Err(EINVAL),
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::OMNI_ROOT, sys_omni_root);
}
