//! The mount table and Linux path resolution.
//!
//! The AOSP sysroot is read-only and comes from its manifest (symlinks included: the tree on disk
//! has none). Writable mounts (`/data`, `/tmp`) are per-instance host directories. `/dev/*` and
//! `/proc/self/exe` are synthesized. A2 adds the rest of `/proc` and `/sys`.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_mem::{Backing, MapExecutability};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use crate::errno::{Errno, EIO, ELOOP, ENOENT, ENOTDIR};
use crate::manifest::{self, Entry, Manifest};

/// The sha256 of the pinned `sysroot.manifest` (Task 1, Step 2).
pub const SYSROOT_MANIFEST_SHA256: &str = "5b58665544077a8d807032e5caf6503081c8f04800be53373071a2e147d99656";

pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
const MAX_LINKS: usize = 40;

pub struct Sysroot {
    objects: PathBuf,
    /// Where the device overlay's files are on the host, by sha256 ([`crate::device`]).
    overlay: HashMap<String, PathBuf>,
    manifest: Manifest,
    children: HashMap<Vec<u8>, Vec<Vec<u8>>>,
    backings: Mutex<HashMap<Vec<u8>, Arc<Backing>>>,
}

impl Sysroot {
    pub fn open(dir: &Path) -> Result<Arc<Self>, String> {
        let path = dir.join("sysroot.manifest");
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if digest != SYSROOT_MANIFEST_SHA256 && std::env::var("OMNI_SYSROOT_UNPINNED").as_deref() != Ok("1") {
            return Err(format!(
                "{}: sha256 {digest} is not the pinned {SYSROOT_MANIFEST_SHA256} (tools/make_sysroot.py)",
                path.display()
            ));
        }
        let text = String::from_utf8(bytes).map_err(|_| "sysroot.manifest is not UTF-8".to_string())?;
        let mut manifest = manifest::parse(&text)?;
        for entry in manifest.entries.values() {
            if let Entry::File { size, sha256, .. } = entry {
                let host = object_path(&dir.join("objects"), sha256);
                let found = std::fs::metadata(&host).map(|m| m.len()).ok();
                if found != Some(*size) {
                    return Err(format!("{}: size {found:?}, manifest says {size}", host.display()));
                }
            }
        }
        // omnidroid's device overlay, over the image: its files where the image has none, and the
        // device configuration it replaces (`device::REPLACES`).
        let mut overlay = HashMap::new();
        for file in crate::device::materialize()? {
            let replaces = crate::device::REPLACES.iter().any(|r| r.as_bytes() == file.guest.as_slice());
            if manifest.entries.contains_key(&file.guest) && !replaces {
                return Err(format!("the device overlay's {} is also in the image", String::from_utf8_lossy(&file.guest)));
            }
            if let Entry::File { sha256, .. } = &file.entry {
                overlay.insert(sha256.clone(), file.host);
            }
            manifest.entries.insert(file.guest, file.entry);
        }
        Ok(Self::build(dir, manifest, overlay))
    }

    #[must_use]
    pub fn from_manifest(dir: &Path, manifest: Manifest) -> Arc<Self> {
        Self::build(dir, manifest, HashMap::new())
    }

    fn build(dir: &Path, manifest: Manifest, overlay: HashMap<String, PathBuf>) -> Arc<Self> {
        let mut children: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
        for path in manifest.entries.keys() {
            if path.as_slice() == b"/" {
                continue;
            }
            let cut = path.iter().rposition(|&b| b == b'/').expect("absolute");
            let parent = if cut == 0 { b"/".to_vec() } else { path[..cut].to_vec() };
            children.entry(parent).or_default().push(path[cut + 1..].to_vec());
        }
        Arc::new(Self { objects: dir.join("objects"), overlay, manifest, children, backings: Mutex::default() })
    }

    /// The names in a sysroot directory.
    #[must_use]
    pub fn children(&self, dir: &[u8]) -> Vec<Vec<u8>> {
        self.children.get(dir).cloned().unwrap_or_default()
    }

    /// Whether the sysroot holds `guest` (any kind of entry).
    #[must_use]
    pub fn has(&self, guest: &[u8]) -> bool {
        self.entry(guest).is_some()
    }

    /// A sysroot file's bytes.
    #[must_use]
    pub fn read(&self, guest: &[u8]) -> Option<Vec<u8>> {
        std::fs::read(self.host_path(guest)?).ok()
    }

    /// Where a regular file's content is on the host; `None` for anything that is not a file.
    #[must_use]
    pub fn host_path(&self, guest: &[u8]) -> Option<PathBuf> {
        match self.entry(guest)? {
            Entry::File { sha256, .. } => Some(self.overlay.get(sha256).cloned().unwrap_or_else(|| object_path(&self.objects, sha256))),
            _ => None,
        }
    }

    fn entry(&self, guest: &[u8]) -> Option<&Entry> {
        self.manifest.entries.get(guest)
    }

    /// One `Backing` per sysroot file, shared by every mapping of it in this process (and so its
    /// text is shared the way `libroblox.so`'s is).
    pub fn backing(&self, guest: &[u8]) -> Result<Arc<Backing>, Errno> {
        let mut cache = self.backings.lock();
        if let Some(b) = cache.get(guest) {
            return Ok(Arc::clone(b));
        }
        let host = self.host_path(guest).ok_or(EIO)?;
        let b = Backing::open(&host, MapExecutability::Executable).map_err(|_| EIO)?;
        cache.insert(guest.to_vec(), Arc::clone(&b));
        Ok(b)
    }
}

fn object_path(objects: &Path, sha256: &str) -> PathBuf {
    objects.join(&sha256[..2]).join(sha256)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevNode {
    Null,
    Zero,
    Random,
    Urandom,
    Binder,
    HwBinder,
    VndBinder,
    /// `/dev/kmsg`: the kernel log, which native daemons (`servicemanager`) log to.
    Kmsg,
    /// `/dev/ashmem`: opened, then sized and named by ioctl, it is a shared-memory region.
    Ashmem,
    /// `/dev/omni-gpu`: the host's GPU, which the guest's Vulkan driver forwards to
    /// ([`crate::gpu`]).
    OmniGpu,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir,
    SysFile { size: u64, mode: u32 },
    HostFile { host: PathBuf },
    HostDir { host: PathBuf },
    Symlink { target: Vec<u8> },
    Dev(DevNode),
    /// A regular file whose bytes are generated when it is opened (`/proc`, `/sys`); size 0.
    Generated,
    /// An in-memory file of a fixed size (`/dev/__properties__/*`): stat reports the size, and it
    /// can be mapped (as a private copy).
    Blob { size: u64 },
    /// The final component does not exist. `host` is where it would be created, on a writable mount.
    Missing { parent_is_dir: bool, host: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: Vec<u8>,
    pub node: Node,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEnt {
    pub name: Vec<u8>,
    pub kind: u8,
    pub ino: u64,
}

/// The host path of `rel` (a normalized guest path relative to a writable mount) under `root`,
/// or `None` when some component cannot be one plain host file name.
///
/// This is the sandbox: every guest name reaches the host through here. Joining the guest bytes
/// as one string let Windows read `\`, `..\` and `C:\` inside a single guest component, so a
/// guest could open or create any host file (A1 review, Critical 1). Each component must be valid
/// UTF-8, not `.`/`..`, free of the characters Windows gives meaning to and of control characters,
/// not end in a dot or space, and not be a DOS device name; the result must still lie under `root`.
#[must_use]
pub fn host_path(root: &Path, rel: &[u8]) -> Option<PathBuf> {
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let mut out = root.to_path_buf();
    for component in rel.split(|&b| b == b'/') {
        let name = std::str::from_utf8(component).ok()?;
        let plain = !name.is_empty()
            && name != "."
            && name != ".."
            && !name.bytes().any(|b| b < 0x20 || br#"\:*?"<>|"#.contains(&b))
            && !name.ends_with('.')
            && !name.ends_with(' ');
        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        if !plain || DEVICES.contains(&stem.as_str()) {
            return None;
        }
        out.push(name);
    }
    out.starts_with(root).then_some(out)
}

/// A stable inode number for a guest path (FNV-1a).
#[must_use]
pub fn ino_of(path: &[u8]) -> u64 {
    path.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)) | 1
}

/// The bind mounts of an instance (its mount namespace, which every process of it shares): a
/// guest directory that shows another's contents, by the host directory that holds them.
#[derive(Default)]
pub struct Binds {
    binds: parking_lot::RwLock<Vec<(Vec<u8>, PathBuf)>>,
}

impl Binds {
    /// The bind mounts of the instance at `instance` (one table per instance directory).
    #[must_use]
    pub fn of(instance: &Path) -> Arc<Self> {
        static TABLES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<PathBuf, Arc<Binds>>>> = std::sync::OnceLock::new();
        let mut tables = TABLES.get_or_init(Default::default).lock();
        Arc::clone(tables.entry(instance.to_path_buf()).or_default())
    }

    /// Mount `host` (a host directory or file) at the guest path `target`, over what was there.
    pub fn bind(&self, target: Vec<u8>, host: PathBuf) {
        let mut binds = self.binds.write();
        binds.retain(|(t, _)| *t != target);
        binds.push((target, host));
    }

    /// Unmount what is mounted at `target`. Whether something was.
    pub fn unbind(&self, target: &[u8]) -> bool {
        let mut binds = self.binds.write();
        let before = binds.len();
        binds.retain(|(t, _)| t.as_slice() != target);
        binds.len() != before
    }

    /// The mounts, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<(Vec<u8>, PathBuf)> {
        self.binds.read().clone()
    }

    /// The deepest bind mount at or above `path`: its target and host.
    fn covering(&self, path: &[u8]) -> Option<(Vec<u8>, PathBuf)> {
        self.binds
            .read()
            .iter()
            .filter(|(t, _)| path == t.as_slice() || (path.starts_with(t) && path.get(t.len()) == Some(&b'/')))
            .max_by_key(|(t, _)| t.len())
            .cloned()
    }
}

pub struct Vfs {
    sysroot: Arc<Sysroot>,
    writable: Vec<(Vec<u8>, PathBuf)>,
    binds: Arc<Binds>,
    /// The owners and modes of the writable mounts' files.
    owners: Arc<crate::owners::Owners>,
    exe: Vec<u8>,
    /// `/proc` and `/sys`, once the process exists (`attach_proc`).
    proc: std::sync::OnceLock<std::sync::Weak<dyn crate::procfs::ProcFs>>,
}

/// The cgroup controllers of the image's `cgroups.json`, where it mounts them.
const CGROUP_MOUNTS: [&[u8]; 5] = [b"/dev/blkio", b"/dev/cpuctl", b"/dev/cpuset", b"/dev/memcg", b"/sys/fs/cgroup"];

fn cgroup_node(path: &[u8]) -> Option<Node> {
    let mount = CGROUP_MOUNTS.iter().find(|m| path == **m || (path.starts_with(m) && path.get(m.len()) == Some(&b'/')))?;
    let rest = &path[mount.len()..];
    let last = rest.rsplit(|&b| b == b'/').next().unwrap_or_default();
    // A file is a named control (`tasks`, `cgroup.procs`, `cpu.shares`...): anything with a dot,
    // and the few without one. Everything else is a group.
    let file = last.contains(&b'.') || matches!(last, b"tasks" | b"notify_on_release" | b"release_agent");
    Some(if file { Node::Dev(DevNode::Null) } else { Node::Dir })
}

thread_local! {
    static CGROUP_RC: Vec<u8> = cgroup_rc();
}

/// `/dev/cgroup_info/cgroup.rc`, as init writes it from `cgroups.json` (libprocessgroup's
/// `CgroupFile`: version, count, then per controller version, flags, a 16-byte name and a
/// 32-byte path -- 56 bytes, as this image's libprocessgroup checks).
fn cgroup_rc() -> Vec<u8> {
    const MOUNTED: u32 = 1;
    const OPTIONAL: u32 = 4;
    let controllers: [(u32, u32, &str, &str); 6] = [
        (1, MOUNTED, "blkio", "/dev/blkio"),
        (1, MOUNTED, "cpu", "/dev/cpuctl"),
        (1, MOUNTED, "cpuset", "/dev/cpuset"),
        (1, MOUNTED | OPTIONAL, "memory", "/dev/memcg"),
        (2, MOUNTED, "cgroup2", "/sys/fs/cgroup"),
        (2, MOUNTED, "freezer", "/sys/fs/cgroup"),
    ];
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&(controllers.len() as u32).to_le_bytes());
    for (version, flags, name, path) in controllers {
        out.extend_from_slice(&version.to_le_bytes());
        out.extend_from_slice(&flags.to_le_bytes());
        let mut n = [0u8; 16];
        n[..name.len()].copy_from_slice(name.as_bytes());
        out.extend_from_slice(&n);
        let mut p = [0u8; 32];
        p[..path.len()].copy_from_slice(path.as_bytes());
        out.extend_from_slice(&p);
    }
    out
}

/// The generated blob at `path`, if the VFS itself makes it (not a process's `/proc`).
#[must_use]
pub fn vfs_blob(path: &[u8]) -> Option<Vec<u8>> {
    (path == b"/dev/cgroup_info/cgroup.rc").then(|| CGROUP_RC.with(Clone::clone))
}

fn is_generated_tree(path: &[u8]) -> bool {
    path == b"/apex/apex-info-list.xml"
        || [&b"/proc"[..], b"/sys", b"/dev/__properties__"].iter().any(|root| path == *root || (path.starts_with(root) && path.get(root.len()) == Some(&b'/')))
}

fn join(components: &[Vec<u8>]) -> Vec<u8> {
    if components.is_empty() {
        return b"/".to_vec();
    }
    components.iter().flat_map(|c| std::iter::once(b'/').chain(c.iter().copied())).collect()
}

fn split(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/').filter(|c| !c.is_empty()).map(<[u8]>::to_vec).collect()
}

impl Vfs {
    #[must_use]
    pub fn new(sysroot: Arc<Sysroot>, writable: Vec<(Vec<u8>, PathBuf)>, exe: Vec<u8>) -> Self {
        Self { sysroot, writable, binds: Arc::default(), owners: crate::owners::Owners::detached(), exe, proc: std::sync::OnceLock::new() }
    }

    /// The same filesystem (sysroot, writable mounts, bind mounts) for a process running `exe`:
    /// a fork child, or the program `execve` loads.
    #[must_use]
    pub fn for_exec(&self, exe: Vec<u8>) -> Self {
        Self { sysroot: Arc::clone(&self.sysroot), writable: self.writable.clone(), binds: Arc::clone(&self.binds), owners: Arc::clone(&self.owners), exe, proc: std::sync::OnceLock::new() }
    }

    /// This VFS with the instance's bind mounts (shared with its other processes).
    #[must_use]
    pub fn with_binds(mut self, binds: Arc<Binds>) -> Self {
        self.binds = binds;
        self
    }

    /// The instance's bind mounts.
    #[must_use]
    pub fn binds(&self) -> &Arc<Binds> {
        &self.binds
    }

    /// This VFS with the instance's owners (shared with its other processes, and kept on disk).
    #[must_use]
    pub fn with_owners(mut self, owners: Arc<crate::owners::Owners>) -> Self {
        self.owners = owners;
        self
    }

    /// The owners and modes of the writable mounts' files.
    #[must_use]
    pub fn owners(&self) -> &Arc<crate::owners::Owners> {
        &self.owners
    }

    /// Whether a guest path is where something is mounted: a writable mount, a bind mount, or
    /// one of the kernel's own (`/`, `/proc`, `/sys`, `/dev`).
    #[must_use]
    pub fn is_mount_point(&self, path: &[u8]) -> bool {
        matches!(path, b"/" | b"/proc" | b"/sys" | b"/dev" | b"/system" | b"/vendor" | b"/apex")
            || self.writable.iter().any(|(m, _)| m.as_slice() == path)
            || self.binds.list().iter().any(|(t, _)| t.as_slice() == path)
    }

    /// Hand `/proc` and `/sys` to `proc`. Only the first attachment counts.
    pub fn attach_proc(&self, proc: std::sync::Weak<dyn crate::procfs::ProcFs>) {
        let _ = self.proc.set(proc);
    }

    fn procfs(&self) -> Option<Arc<dyn crate::procfs::ProcFs>> {
        self.proc.get().and_then(std::sync::Weak::upgrade)
    }

    /// The bytes of a generated file, produced now.
    #[must_use]
    pub fn read_generated(&self, path: &[u8]) -> Option<Vec<u8>> {
        if let Some(blob) = vfs_blob(path) {
            return Some(blob);
        }
        self.procfs()?.read(path)
    }

    /// The executable's guest path (`/proc/<pid>/exe`).
    #[must_use]
    pub fn exe(&self) -> &[u8] {
        &self.exe
    }

    #[must_use]
    pub fn sysroot(&self) -> &Arc<Sysroot> {
        &self.sysroot
    }

    /// The node at an already-normalized absolute path, without following a final symlink.
    fn lookup(&self, path: &[u8]) -> Option<Node> {
        // The cgroup hierarchies init mounts (cgroups.json): every group is there, and a task
        // written into one is taken -- there is no scheduler here for it to change.
        if let Some(node) = cgroup_node(path) {
            return Some(node);
        }
        if is_generated_tree(path) {
            if let Some(proc) = self.procfs() {
                return proc.node(path);
            }
        }
        match path {
            b"/dev" | b"/proc" | b"/proc/self" => return Some(Node::Dir),
            // The root's links, as a device's first-stage init leaves them (where the image has
            // nothing of its own there).
            b"/etc" if !self.sysroot.has(path) => return Some(Node::Symlink { target: b"/system/etc".to_vec() }),
            b"/bin" if !self.sysroot.has(path) => return Some(Node::Symlink { target: b"/system/bin".to_vec() }),
            b"/dev/cgroup_info" => return Some(Node::Dir),
            b"/dev/cgroup_info/cgroup.rc" => return Some(Node::Blob { size: CGROUP_RC.with(|rc| rc.len() as u64) }),
            b"/dev/null" => return Some(Node::Dev(DevNode::Null)),
            b"/dev/zero" => return Some(Node::Dev(DevNode::Zero)),
            b"/dev/random" => return Some(Node::Dev(DevNode::Random)),
            b"/dev/urandom" => return Some(Node::Dev(DevNode::Urandom)),
            b"/dev/binder" => return Some(Node::Dev(DevNode::Binder)),
            b"/dev/hwbinder" => return Some(Node::Dev(DevNode::HwBinder)),
            b"/dev/vndbinder" => return Some(Node::Dev(DevNode::VndBinder)),
            b"/dev/kmsg" => return Some(Node::Dev(DevNode::Kmsg)),
            b"/dev/ashmem" => return Some(Node::Dev(DevNode::Ashmem)),
            b"/dev/omni-gpu" => return Some(Node::Dev(DevNode::OmniGpu)),
            b"/proc/self/exe" => return Some(Node::Symlink { target: self.exe.clone() }),
            _ => {}
        }
        let bound = self.binds.covering(path);
        let mounts = bound.iter().chain(self.writable.iter());
        for (mount, host) in mounts {
            if path == mount.as_slice() {
                if !host.is_dir() {
                    return Some(Node::HostFile { host: host.clone() });
                }
                return Some(Node::HostDir { host: host.clone() });
            }
            if path.starts_with(mount) && path.get(mount.len()) == Some(&b'/') {
                // A guest name the host cannot hold as one plain name is not there (see `host_path`).
                let host = host_path(host, &path[mount.len() + 1..])?;
                return match std::fs::metadata(&host) {
                    Ok(m) if m.is_dir() => Some(Node::HostDir { host }),
                    Ok(_) => Some(Node::HostFile { host }),
                    Err(_) => None,
                };
            }
        }
        match self.sysroot.entry(path)? {
            Entry::Dir { .. } => Some(Node::Dir),
            Entry::File { size, mode, .. } => Some(Node::SysFile { size: *size, mode: *mode }),
            Entry::Symlink { target } => Some(Node::Symlink { target: target.clone() }),
        }
    }

    fn host_for_missing(&self, path: &[u8]) -> Option<PathBuf> {
        let bound = self.binds.covering(path);
        bound.iter().chain(self.writable.iter()).find_map(|(mount, host)| {
            (path.starts_with(mount) && path.get(mount.len()) == Some(&b'/'))
                .then(|| host_path(host, &path[mount.len() + 1..]))
                .flatten()
        })
    }

    pub fn resolve(&self, cwd: &[u8], path: &[u8], follow_last: bool) -> Result<Resolved, Errno> {
        if path.is_empty() {
            return Err(ENOENT);
        }
        let mut done: Vec<Vec<u8>> = if path[0] == b'/' { Vec::new() } else { split(cwd) };
        let mut todo: Vec<Vec<u8>> = split(path);
        todo.reverse(); // a stack: the next component is at the end
        let mut links = 0usize;
        while let Some(component) = todo.pop() {
            match component.as_slice() {
                b"." => continue,
                b".." => {
                    done.pop();
                    continue;
                }
                _ => {}
            }
            done.push(component);
            let here = join(&done);
            let last = todo.is_empty();
            match self.lookup(&here) {
                None if last => {
                    let parent = join(&done[..done.len() - 1]);
                    let parent_is_dir = matches!(self.lookup(&parent), Some(Node::Dir | Node::HostDir { .. }));
                    return Ok(Resolved { node: Node::Missing { parent_is_dir, host: self.host_for_missing(&here) }, path: here });
                }
                None => return Err(ENOENT),
                Some(Node::Symlink { target }) if !last || follow_last => {
                    links += 1;
                    if links > MAX_LINKS {
                        return Err(ELOOP);
                    }
                    done.pop();
                    if target.first() == Some(&b'/') {
                        done.clear();
                    }
                    let mut more = split(&target);
                    more.reverse();
                    todo.extend(more);
                }
                Some(node) if last => return Ok(Resolved { path: here, node }),
                Some(Node::Dir | Node::HostDir { .. }) => {}
                Some(_) => return Err(ENOTDIR),
            }
        }
        let here = join(&done);
        let node = self.lookup(&here).ok_or(ENOENT)?;
        Ok(Resolved { path: here, node })
    }

    pub fn list(&self, dir: &Resolved) -> Result<Vec<DirEnt>, Errno> {
        let child = |name: &[u8]| {
            let mut p = dir.path.clone();
            if p.as_slice() != b"/" {
                p.push(b'/');
            }
            p.extend_from_slice(name);
            p
        };
        match &dir.node {
            Node::Dir if is_generated_tree(&dir.path) && self.procfs().is_some() => {
                Ok(self.procfs().expect("attached").list(&dir.path))
            }
            Node::Dir => {
                let mut out = Vec::new();
                let synthetic: &[&[u8]] = match dir.path.as_slice() {
                    b"/" => &[b"dev", b"proc", b"data", b"tmp"],
                    b"/dev" => &[b"null", b"zero", b"random", b"urandom", b"__properties__"],
                    b"/proc" => &[b"self"],
                    b"/proc/self" => &[b"exe"],
                    _ => &[],
                };
                for name in synthetic {
                    let path = child(name);
                    if let Some(node) = self.lookup(&path) {
                        out.push(DirEnt { name: name.to_vec(), kind: kind_of(&node), ino: ino_of(&path) });
                    }
                }
                for name in self.sysroot.children.get(&dir.path).into_iter().flatten() {
                    let path = child(name);
                    if out.iter().any(|e| &e.name == name) {
                        continue;
                    }
                    if let Some(node) = self.lookup(&path) {
                        out.push(DirEnt { name: name.clone(), kind: kind_of(&node), ino: ino_of(&path) });
                    }
                }
                Ok(out)
            }
            Node::HostDir { host } => {
                let mut out = Vec::new();
                for e in std::fs::read_dir(host).map_err(|_| EIO)? {
                    let e = e.map_err(|_| EIO)?;
                    let name = e.file_name().to_string_lossy().as_bytes().to_vec();
                    let kind = if e.file_type().map_err(|_| EIO)?.is_dir() { DT_DIR } else { DT_REG };
                    out.push(DirEnt { ino: ino_of(&child(&name)), name, kind });
                }
                Ok(out)
            }
            _ => Err(ENOTDIR),
        }
    }
}

fn kind_of(node: &Node) -> u8 {
    match node {
        Node::Dir | Node::HostDir { .. } => DT_DIR,
        Node::Symlink { .. } => DT_LNK,
        Node::Dev(_) => DT_CHR,
        _ => DT_REG,
    }
}
