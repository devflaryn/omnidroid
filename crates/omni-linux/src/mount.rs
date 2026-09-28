//! `mount` and `umount2`, for what the system's daemons mount at run time: bind mounts (vold
//! binds /data/data onto /data/user/0), changes of propagation, and remounts. A bind mount is
//! kept in the instance's table (`crate::vfs::Binds`), which every process of the instance sees.
//!
//! Shared storage: vold mounts the emulated volume with FUSE (`/mnt/user/<user>/emulated`, served
//! by MediaProvider from `/data/media`). There is no FUSE here; the mount is a bind of
//! `/data/media` -- what MediaProvider's daemon would pass through for an app with access to all
//! files -- and `/dev/fuse` answers the daemon's handshake (`crate::fuse`).
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
    let over = match &target.node {
        Node::HostDir { host } => Some(host.clone()),
        _ => None,
    };
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
        p.vfs.binds().bind(target.path, host, over);
        return Ok(0);
    }
    // Changing how mounts propagate, or remounting with other flags: nothing here depends on it.
    if flags & (PROPAGATION | MS_REMOUNT) != 0 {
        return Ok(0);
    }
    let fstype = if a[2] == 0 { Vec::new() } else { p.mem.read_cstr(a[2], 64)? };
    // The emulated volume's FUSE mount (vold's `MountUserFuse`): `/data/media`, passed through.
    if fstype == b"fuse" && is_emulated_volume(&target.path) {
        let lower = p.vfs.resolve(b"/", b"/data/media", true)?;
        let Node::HostDir { host } = lower.node else { return Err(ENOENT) };
        p.vfs.binds().bind(target.path, host, over);
        return Ok(0);
    }
    p.refusals.record(format!("mount: {} on {}", String::from_utf8_lossy(&fstype), String::from_utf8_lossy(&target.path)), t.pc, t.lr);
    Err(crate::errno::ENOSYS)
}

/// `/mnt/user/<user>/emulated`, where vold mounts the emulated volume for a user.
fn is_emulated_volume(path: &[u8]) -> bool {
    let Some(rest) = path.strip_prefix(b"/mnt/user/") else { return false };
    let mut parts = rest.split(|b| *b == b'/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(user), Some(b"emulated"), None) if !user.is_empty() && user.iter().all(u8::is_ascii_digit))
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
