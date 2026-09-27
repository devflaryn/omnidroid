//! `mount` and `umount2`, for what the system's daemons mount at run time: bind mounts (vold
//! binds /data/data onto /data/user/0), changes of propagation, and remounts. A bind mount is
//! kept in the instance's table (`crate::vfs::Binds`), which every process of the instance sees.
use crate::errno::{Errno, SysResult, EINVAL, ENOENT, ENOTDIR};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};
use crate::vfs::Node;

const MS_REMOUNT: u64 = 0x20;
const MS_BIND: u64 = 0x1000;
const MS_UNBINDABLE: u64 = 1 << 17;
const MS_PRIVATE: u64 = 1 << 18;
const MS_SLAVE: u64 = 1 << 19;
const MS_SHARED: u64 = 1 << 20;
const PROPAGATION: u64 = MS_UNBINDABLE | MS_PRIVATE | MS_SLAVE | MS_SHARED;
const EBUSY: Errno = crate::errno::EBUSY;
const UMOUNT_NOFOLLOW: u64 = 8;

fn path_arg(p: &Process, at: u64) -> Result<Vec<u8>, Errno> {
    p.mem.read_cstr(at, 4096)
}

/// `mount(source, target, fstype, flags, data)`.
fn sys_mount(p: &Process, t: &mut Task, a: [u64; 6]) -> SysResult {
    let flags = a[3];
    let cwd = p.cwd.lock().clone();
    let target = p.vfs.resolve(&cwd, &path_arg(p, a[1])?, true)?;
    if matches!(target.node, Node::Missing { .. }) {
        return Err(ENOENT);
    }
    if flags & MS_BIND != 0 && flags & MS_REMOUNT == 0 {
        let source = p.vfs.resolve(&cwd, &path_arg(p, a[0])?, true)?;
        let host = match source.node {
            Node::Missing { .. } => return Err(ENOENT),
            Node::HostDir { host } => {
                if !matches!(target.node, Node::Dir | Node::HostDir { .. }) {
                    return Err(ENOTDIR);
                }
                host
            }
            Node::HostFile { host } => host,
            // Binding a part of the image (or /proc, /dev) somewhere else is not offered.
            _ => {
                p.refusals.record(format!("mount: bind {}", String::from_utf8_lossy(&source.path)), t.pc, t.lr);
                return Err(EINVAL);
            }
        };
        p.vfs.binds().bind(target.path, host);
        return Ok(0);
    }
    // Changing how mounts propagate, or remounting with other flags: nothing here depends on it.
    if flags & (PROPAGATION | MS_REMOUNT) != 0 {
        return Ok(0);
    }
    let fstype = if a[2] == 0 { Vec::new() } else { p.mem.read_cstr(a[2], 64)? };
    p.refusals.record(format!("mount: {} on {}", String::from_utf8_lossy(&fstype), String::from_utf8_lossy(&target.path)), t.pc, t.lr);
    Err(crate::errno::ENOSYS)
}

/// `umount2(target, flags)`: a bind mount is removed; the kernel's own mounts and the writable
/// ones are in use (`EBUSY`); anything else is not a mount point (`EINVAL`).
fn sys_umount2(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    let cwd = p.cwd.lock().clone();
    let target = p.vfs.resolve(&cwd, &path_arg(p, a[0])?, a[1] & UMOUNT_NOFOLLOW == 0)?;
    if matches!(target.node, Node::Missing { .. }) {
        return Err(ENOENT);
    }
    if p.vfs.binds().unbind(&target.path) {
        return Ok(0);
    }
    if p.vfs.is_mount_point(&target.path) { Err(EBUSY) } else { Err(EINVAL) }
}

pub fn install(table: &mut Table) {
    table.set(nr::MOUNT, sys_mount);
    table.set(nr::UMOUNT2, sys_umount2);
}
