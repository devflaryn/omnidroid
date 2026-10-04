//! The `omni_root` syscall: the engine's own door to root. A non-rooted device answers `ENOSYS`,
//! exactly like an unimplemented syscall.
use crate::errno::{Errno, SysResult, EACCES, EINVAL, ENAMETOOLONG, ENOSYS};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};

const OP_ELEVATE: u64 = 1;
const OP_STATUS: u64 = 2;
const OP_SETPROP: u64 = 3;
const OP_DELPROP: u64 = 4;
/// `omni_root(OP_DENYLIST, action, arg, len)`: uid 0 only. add/rm take a package C string in `arg`;
/// ls writes the newline-joined denylist to the buffer `arg` of `len` bytes and returns its length.
const OP_DENYLIST: u64 = 5;
const DL_ADD: u64 = 1;
const DL_RM: u64 = 2;
const DL_LS: u64 = 3;
const PKG_MAX: usize = 256;

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
        OP_DENYLIST => {
            if p.sys.uid() != 0 {
                return Err(EACCES);
            }
            let dir = p.vfs.binds().instance_dir().ok_or(ENOSYS)?;
            // Read the current file fresh (not the cached copy) so concurrent edits are not lost.
            let mut edited = super::Profile::load(dir).ok_or(ENOSYS)?;
            match a[1] {
                DL_ADD | DL_RM => {
                    let pkg = guest_str(p, a[2], PKG_MAX)?;
                    let pkg = pkg.trim();
                    if pkg.is_empty() || pkg.contains(['\n', ',', '=']) {
                        return Err(EINVAL);
                    }
                    if a[1] == DL_ADD {
                        edited.denylist_add(pkg);
                    } else {
                        edited.denylist_remove(pkg);
                    }
                    let file = dir.join(super::profile::PROFILE_FILE);
                    let tmp = dir.join(format!("{}.tmp", super::profile::PROFILE_FILE));
                    std::fs::write(&tmp, edited.serialize()).and_then(|()| std::fs::rename(&tmp, &file)).map_err(|_| EINVAL)?;
                    Ok(0)
                }
                DL_LS => {
                    let mut text = edited.denylist.join("\n");
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    let n = text.len().min(usize::try_from(a[3]).map_err(|_| EINVAL)?);
                    p.mem.write(a[2], &text.as_bytes()[..n])?;
                    Ok(n as u64)
                }
                _ => Err(EINVAL),
            }
        }
        _ => Err(EINVAL),
    }
}

pub fn install(table: &mut Table) {
    table.set(nr::OMNI_ROOT, sys_omni_root);
}
