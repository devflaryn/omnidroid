//! Extended attributes (`setxattr`, `getxattr`, `listxattr`, `removexattr` and their `l`/`f`
//! forms), kept as a filesystem keeps them: per file, by name. A file's SELinux context
//! (`security.selinux`) is one: `restorecon` sets it from the image's file_contexts and `ls -Z`
//! reads it; a file never labelled reads as the kernel's unlabeled context.
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::OnceLock;

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EEXIST, EINVAL, ENOENT, ERANGE};
use crate::process::{Process, Task};
use crate::syscall::{nr, Table};
use crate::vfs::{Node, Resolved};

/// `ENODATA`: no attribute of that name.
const ENODATA: Errno = Errno(61);
const XATTR_CREATE: u64 = 1;
const XATTR_REPLACE: u64 = 2;
const XATTR_NAME_MAX: usize = 255;
const XATTR_SIZE_MAX: usize = 65536;
const SELINUX: &[u8] = b"security.selinux";
const CAPABILITY: &[u8] = b"security.capability";
/// What the kernel reports for a file with no context of its own.
const UNLABELED: &[u8] = b"u:object_r:unlabeled:s0\0";

/// Which file: a host file or directory by its host path (one per instance), anything else by
/// its guest path in its sysroot (the image's files, the same in every instance).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Host(PathBuf),
    Guest(usize, Vec<u8>),
}

type Attributes = BTreeMap<Vec<u8>, Vec<u8>>;

fn store() -> &'static Mutex<HashMap<Key, Attributes>> {
    static STORE: OnceLock<Mutex<HashMap<Key, Attributes>>> = OnceLock::new();
    STORE.get_or_init(Mutex::default)
}

fn key_of(p: &Process, r: Resolved) -> Result<Key, Errno> {
    match r.node {
        Node::Missing { .. } => Err(ENOENT),
        Node::HostFile { host } | Node::HostDir { host } => Ok(Key::Host(host)),
        _ => Ok(Key::Guest(std::sync::Arc::as_ptr(p.vfs.sysroot()) as usize, r.path)),
    }
}

/// The file a path-taking call names (`follow`: not the `l` form).
fn path_key(p: &Process, path: u64, follow: bool) -> Result<Key, Errno> {
    let path = p.mem.read_cstr(path, 4096)?;
    let cwd = p.cwd.lock().clone();
    key_of(p, p.vfs.resolve(&cwd, &path, follow)?)
}

/// The file an `f` call's descriptor is open on.
fn fd_key(p: &Process, fd: u64) -> Result<Key, Errno> {
    let file = p.fds.get(fd as i64 as i32)?;
    let guest = crate::fd::guest_path_of(&file);
    if guest.first() != Some(&b'/') {
        return Err(crate::errno::EOPNOTSUPP); // a pipe, socket or anonymous inode
    }
    key_of(p, p.vfs.resolve(b"/", &guest, true)?)
}

fn name_arg(p: &Process, at: u64) -> Result<Vec<u8>, Errno> {
    let name = p.mem.read_cstr(at, XATTR_NAME_MAX + 1)?;
    if name.is_empty() || name.len() > XATTR_NAME_MAX {
        return Err(ERANGE);
    }
    Ok(name)
}

fn set(p: &Process, key: Key, a: [u64; 6]) -> SysResult {
    let name = name_arg(p, a[1])?;
    let size = a[3] as usize;
    if size > XATTR_SIZE_MAX {
        return Err(crate::errno::E2BIG);
    }
    if a[4] & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
        return Err(EINVAL);
    }
    let mut value = p.mem.read(a[2], size)?;
    // A context is stored as the kernel stores it: NUL-terminated.
    if name == SELINUX && value.last() != Some(&0) {
        value.push(0);
    }
    let mut store = store().lock();
    let attrs = store.entry(key.clone()).or_default();
    match (attrs.contains_key(&name), a[4]) {
        (true, XATTR_CREATE) => return Err(EEXIST),
        (false, XATTR_REPLACE) => return Err(ENODATA),
        _ => {}
    }
    // A directory's label is kept across boots too (`crate::labels`).
    let kept = (name == SELINUX).then(|| value.clone());
    attrs.insert(name, value);
    drop(store);
    if let (Some(label), Key::Host(host)) = (kept, &key) {
        if let Some(labels) = p.vfs.owners().instance().and_then(crate::labels::Labels::of) {
            labels.set(host, &label);
        }
    }
    Ok(0)
}

/// The label kept for a directory of this instance at an earlier boot (`crate::labels`).
fn kept_label(p: &Process, key: &Key) -> Option<Vec<u8>> {
    let Key::Host(host) = key else { return None };
    p.vfs.owners().instance().and_then(crate::labels::Labels::of)?.get(host)
}

fn get(p: &Process, key: Key, a: [u64; 6]) -> SysResult {
    let name = name_arg(p, a[1])?;
    let value = store().lock().get(&key).and_then(|attrs| attrs.get(&name).cloned());
    let value = match value {
        Some(v) => v,
        // The BPF filesystem is labelled by the policy's genfs rules.
        None if name == SELINUX && matches!(&key, Key::Guest(_, path) if crate::bpf::on_bpffs(path)) => {
            let Key::Guest(_, path) = &key else { unreachable!() };
            let mut c = crate::bpf::context(path, p.vfs.sysroot()).into_bytes();
            c.push(0);
            c
        }
        // An image path: the image's label and capability.
        None if image_attr(p, &key, &name).is_some() => image_attr(p, &key, &name).expect("checked"),
        // A file of an instance whose label must outlive the host process that set it.
        None if name == SELINUX && persistent_label(&key).is_some() => persistent_label(&key).expect("checked").to_vec(),
        None if name == SELINUX && kept_label(p, &key).is_some() => kept_label(p, &key).expect("checked"),
        None if name == SELINUX => UNLABELED.to_vec(),
        None => return Err(ENODATA),
    };
    reply(p, a[2], a[3], &value)
}

/// **Labels kept by the place, not the process.** Attributes live in this host process's memory, so
/// what `apexd`'s `restorecon` set on a decompressed APEX is gone at the next boot of a saved device,
/// and `apexd` then found every one "unlabeled", called it invalid and decompressed all 22 again, at
/// every boot. A file in an instance's `/data/apex/decompressed` that nothing labelled reads as what
/// `file_contexts` gives it (`/data/apex/decompressed/(.*)?  u:object_r:staging_data_file:s0`), as
/// it would on a device's persistent filesystem -- `crate::apex::predecompress`'s files included.
fn persistent_label(key: &Key) -> Option<&'static [u8]> {
    let Key::Host(host) = key else { return None };
    let mut up = host.parent()?.components().rev().map(|c| c.as_os_str());
    (up.next()? == "decompressed" && up.next()? == "apex" && up.next()? == "data").then_some(b"u:object_r:staging_data_file:s0\0")
}

/// An image path's `security.selinux` or `security.capability`.
fn image_attr(p: &Process, key: &Key, name: &[u8]) -> Option<Vec<u8>> {
    let Key::Guest(_, path) = key else { return None };
    let meta = p.vfs.sysroot().image_meta(path)?;
    match name {
        SELINUX => meta.label.clone(),
        CAPABILITY => meta.capability.clone(),
        _ => None,
    }
}

fn list(p: &Process, key: Key, a: [u64; 6]) -> SysResult {
    let mut names = store().lock().get(&key).map(|attrs| attrs.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    if !names.iter().any(|n| n == SELINUX) {
        names.push(SELINUX.to_vec());
    }
    if !names.iter().any(|n| n == CAPABILITY) && image_attr(p, &key, CAPABILITY).is_some() {
        names.push(CAPABILITY.to_vec());
    }
    names.sort();
    let mut out = Vec::new();
    for n in names {
        out.extend_from_slice(&n);
        out.push(0);
    }
    reply(p, a[1], a[2], &out)
}

fn remove(p: &Process, key: Key, a: [u64; 6]) -> SysResult {
    let name = name_arg(p, a[1])?;
    store().lock().get_mut(&key).and_then(|attrs| attrs.remove(&name)).map(|_| 0).ok_or(ENODATA)
}

/// A value into the caller's buffer: size 0 asks only its length; too small a buffer is `ERANGE`.
fn reply(p: &Process, buf: u64, size: u64, value: &[u8]) -> SysResult {
    if size == 0 {
        return Ok(value.len() as u64);
    }
    if (size as usize) < value.len() {
        return Err(ERANGE);
    }
    p.mem.write(buf, value)?;
    Ok(value.len() as u64)
}

fn sys_setxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    set(p, path_key(p, a[0], true)?, a)
}
fn sys_lsetxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    set(p, path_key(p, a[0], false)?, a)
}
fn sys_fsetxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    set(p, fd_key(p, a[0])?, a)
}
fn sys_getxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    get(p, path_key(p, a[0], true)?, a)
}
fn sys_lgetxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    get(p, path_key(p, a[0], false)?, a)
}
fn sys_fgetxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    get(p, fd_key(p, a[0])?, a)
}
fn sys_listxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    list(p, path_key(p, a[0], true)?, a)
}
fn sys_llistxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    list(p, path_key(p, a[0], false)?, a)
}
fn sys_flistxattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    list(p, fd_key(p, a[0])?, a)
}
fn sys_removexattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    remove(p, path_key(p, a[0], true)?, a)
}
fn sys_lremovexattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    remove(p, path_key(p, a[0], false)?, a)
}
fn sys_fremovexattr(p: &Process, _t: &mut Task, a: [u64; 6]) -> SysResult {
    remove(p, fd_key(p, a[0])?, a)
}

pub fn install(table: &mut Table) {
    table.set(nr::SETXATTR, sys_setxattr);
    table.set(nr::LSETXATTR, sys_lsetxattr);
    table.set(nr::FSETXATTR, sys_fsetxattr);
    table.set(nr::GETXATTR, sys_getxattr);
    table.set(nr::LGETXATTR, sys_lgetxattr);
    table.set(nr::FGETXATTR, sys_fgetxattr);
    table.set(nr::LISTXATTR, sys_listxattr);
    table.set(nr::LLISTXATTR, sys_llistxattr);
    table.set(nr::FLISTXATTR, sys_flistxattr);
    table.set(nr::REMOVEXATTR, sys_removexattr);
    table.set(nr::LREMOVEXATTR, sys_lremovexattr);
    table.set(nr::FREMOVEXATTR, sys_fremovexattr);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decompressed_apex_keeps_its_label_and_nothing_else_does() {
        let at = |p: &str| persistent_label(&Key::Host(PathBuf::from(p)));
        let label: &[u8] = b"u:object_r:staging_data_file:s0\0";
        assert_eq!(at("/tmp/inst/data/apex/decompressed/com.android.art@352090000.decompressed.apex"), Some(label));
        assert_eq!(at("/tmp/inst/data/apex/active/com.android.art.apex"), None);
        assert_eq!(at("/tmp/inst/data/local/tmp/decompressed/x"), None);
        assert_eq!(persistent_label(&Key::Guest(0, b"/data/apex/decompressed/x".to_vec())), None);
    }
}
