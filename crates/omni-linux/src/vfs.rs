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

/// The sha256 of the pinned `sysroot.meta` (tools/make_sysroot.py --meta).
pub const SYSROOT_META_SHA256: &str = "dc7309c933d135a55f68fa19c978dea5da4759aa56f661246dd9c645da99e7b1";

/// An image path's owner, permission bits, SELinux label and file capability, as the image's
/// filesystem holds them (fs_config's owners and modes, file_contexts' labels): what a device's
/// mounted image reports, and an unprivileged extraction loses.
#[derive(Clone, Debug, Default)]
pub struct ImageMeta {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    /// `security.selinux`, NUL-terminated as the kernel returns it.
    pub label: Option<Vec<u8>>,
    /// `security.capability`'s raw value.
    pub capability: Option<Vec<u8>>,
}

/// Parse `sysroot.meta`: `path \t uid \t gid \t mode (octal) \t label|- \t capability (hex)|-`.
fn parse_meta(text: &str) -> Result<HashMap<Vec<u8>, ImageMeta>, String> {
    let mut out = HashMap::new();
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let [path, uid, gid, mode, label, cap] = f[..] else { return Err(format!("sysroot.meta: {line}")) };
        let num = |s: &str, radix| u32::from_str_radix(s, radix).map_err(|_| format!("sysroot.meta: {line}"));
        let label = (label != "-").then(|| {
            let mut l = label.as_bytes().to_vec();
            l.push(0);
            l
        });
        let capability = if cap == "-" {
            None
        } else {
            Some((0..cap.len()).step_by(2).map(|i| u8::from_str_radix(&cap[i..i + 2], 16)).collect::<Result<Vec<u8>, _>>().map_err(|_| format!("sysroot.meta: {line}"))?)
        };
        out.insert(path.as_bytes().to_vec(), ImageMeta { uid: num(uid, 10)?, gid: num(gid, 10)?, mode: num(mode, 8)?, label, capability });
    }
    Ok(out)
}

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
    /// Each image path's owner, mode, label and capability (`sysroot.meta`; empty without one).
    meta: HashMap<Vec<u8>, ImageMeta>,
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
        let mut sysroot = Self::build(dir, manifest, overlay);
        let meta_path = dir.join("sysroot.meta");
        if let Ok(bytes) = std::fs::read(&meta_path) {
            let digest = format!("{:x}", Sha256::digest(&bytes));
            if digest != SYSROOT_META_SHA256 && std::env::var("OMNI_SYSROOT_UNPINNED").as_deref() != Ok("1") {
                return Err(format!("{}: sha256 {digest} is not the pinned {SYSROOT_META_SHA256} (tools/make_sysroot.py --meta)", meta_path.display()));
            }
            let meta = parse_meta(&String::from_utf8_lossy(&bytes))?;
            Arc::get_mut(&mut sysroot).expect("a new sysroot").meta = meta;
        }
        Ok(sysroot)
    }

    /// An image path's owner, mode, label and capability.
    #[must_use]
    pub fn image_meta(&self, path: &[u8]) -> Option<&ImageMeta> {
        self.meta.get(path)
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
        Arc::new(Self { objects: dir.join("objects"), overlay, manifest, children, backings: Mutex::default(), meta: HashMap::new() })
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
    /// `/dev/fuse` ([`crate::fuse`]).
    Fuse,
    /// `/dev/input/event<n>`: an input device ([`crate::evdev`]).
    Input(u16),
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

/// One bind mount: the guest directory `target` shows the host directory `host`. `over` is the
/// host directory that was at `target` when it was mounted, if it was one: a kernel attaches a
/// mount to the directory, not to its path, so the mount is seen through any other path that
/// reaches that directory -- init binds /mnt/user/0 onto /storage, then vold mounts the emulated
/// volume on /mnt/user/0/emulated, and /storage/emulated shows the volume.
#[derive(Clone)]
struct Bind {
    target: Vec<u8>,
    host: PathBuf,
    over: Option<PathBuf>,
}

/// The bind mounts of an instance (its mount namespace, which every process of it shares): a
/// guest directory that shows another's contents, by the host directory that holds them.
#[derive(Default)]
pub struct Binds {
    binds: parking_lot::RwLock<Vec<Bind>>,
    /// Whether any bind is attached over a host directory (`Bind::over`): only then does a host
    /// path need [`Binds::redirect`].
    any_over: std::sync::atomic::AtomicBool,
    /// Where the table is kept (`<instance>/.omni-binds`), so every host process of the instance
    /// has the same mounts -- as a kernel's are the whole system's (vold binds /data/data onto
    /// /data/user/0 in the system's host process; an app, in its own, finds its data there).
    file: Option<PathBuf>,
    /// The file's modification time when last read, and when that was checked.
    seen: parking_lot::Mutex<(Option<std::time::SystemTime>, Option<std::time::Instant>)>,
}

impl Binds {
    /// The instance's directory, when the table is kept in one.
    #[must_use]
    pub fn instance_dir(&self) -> Option<&Path> {
        self.file.as_deref().and_then(Path::parent)
    }

    /// The bind mounts of the instance at `instance` (one table per instance directory).
    #[must_use]
    pub fn of(instance: &Path) -> Arc<Self> {
        static TABLES: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashMap<PathBuf, Arc<Binds>>>> = std::sync::OnceLock::new();
        let mut tables = TABLES.get_or_init(Default::default).lock();
        Arc::clone(tables.entry(instance.to_path_buf()).or_insert_with(|| {
            let binds = Binds { file: Some(instance.join(".omni-binds")), ..Binds::default() };
            binds.reload(true);
            Arc::new(binds)
        }))
    }

    /// Read the table again if another host process changed it (checked at most once a second,
    /// or now when `force`).
    fn reload(&self, force: bool) {
        let Some(file) = &self.file else { return };
        // The time lock is never held with the table's (bind and unbind take the table's first).
        {
            let mut seen = self.seen.lock();
            let now = std::time::Instant::now();
            if !force && seen.1.is_some_and(|at| now.duration_since(at) < std::time::Duration::from_secs(1)) {
                return;
            }
            seen.1 = Some(now);
            let modified = std::fs::metadata(file).and_then(|m| m.modified()).ok();
            if modified == seen.0 {
                return;
            }
            seen.0 = modified;
        }
        let text = std::fs::read(file).unwrap_or_default();
        let table: Vec<Bind> = text
            .split(|b| *b == b'\n')
            .filter_map(|line| {
                let mut fields = line.split(|b| *b == b'\t');
                let target = fields.next()?.to_vec();
                let host = PathBuf::from(String::from_utf8_lossy(fields.next()?).into_owned());
                let over = fields.next().filter(|o| !o.is_empty()).map(|o| PathBuf::from(String::from_utf8_lossy(o).into_owned()));
                Some(Bind { target, host, over })
            })
            .collect();
        self.any_over.store(table.iter().any(|b| b.over.is_some()), std::sync::atomic::Ordering::Relaxed);
        *self.binds.write() = table;
    }

    /// Write the table for the instance's other host processes.
    fn save(&self, binds: &[Bind]) {
        self.any_over.store(binds.iter().any(|b| b.over.is_some()), std::sync::atomic::Ordering::Relaxed);
        let Some(file) = &self.file else { return };
        let mut text = Vec::new();
        for b in binds {
            text.extend_from_slice(&b.target);
            text.push(b'\t');
            text.extend_from_slice(b.host.to_string_lossy().as_bytes());
            text.push(b'\t');
            if let Some(over) = &b.over {
                text.extend_from_slice(over.to_string_lossy().as_bytes());
            }
            text.push(b'\n');
        }
        let _ = std::fs::write(file, text);
        let mut seen = self.seen.lock();
        seen.0 = std::fs::metadata(file).and_then(|m| m.modified()).ok();
    }

    /// Mount `host` (a host directory or file) at the guest path `target`, over what was there --
    /// `over`, the host directory at `target` if it was one (see [`Bind`]).
    pub fn bind(&self, target: Vec<u8>, host: PathBuf, over: Option<PathBuf>) {
        self.reload(true);
        let over = over.filter(|o| *o != host);
        let table = {
            let mut binds = self.binds.write();
            binds.retain(|b| b.target != target);
            binds.push(Bind { target, host, over });
            binds.clone()
        };
        self.save(&table);
    }

    /// `host` as the mounts show it: a path at or under a directory something is mounted over is
    /// in what is mounted there (repeatedly: a mount inside a mount).
    #[must_use]
    pub fn redirect(&self, mut host: PathBuf) -> PathBuf {
        if !self.any_over.load(std::sync::atomic::Ordering::Relaxed) {
            return host;
        }
        let binds = self.binds.read();
        for _ in 0..8 {
            let Some(b) = binds
                .iter()
                // A bind of a directory inside the one it covers would only lead into itself.
                .filter(|b| b.over.as_ref().is_some_and(|o| host.starts_with(o) && !b.host.starts_with(o)))
                .max_by_key(|b| b.over.as_ref().map_or(0, |o| o.as_os_str().len()))
            else {
                break;
            };
            let over = b.over.as_ref().expect("filtered");
            let rest = host.strip_prefix(over).map(Path::to_path_buf).unwrap_or_default();
            host = if rest.as_os_str().is_empty() { b.host.clone() } else { b.host.join(rest) };
        }
        host
    }

    /// Unmount what is mounted at `target`. Whether something was.
    pub fn unbind(&self, target: &[u8]) -> bool {
        self.reload(true);
        let (removed, table) = {
            let mut binds = self.binds.write();
            let before = binds.len();
            binds.retain(|b| b.target.as_slice() != target);
            (binds.len() != before, binds.clone())
        };
        if removed {
            self.save(&table);
        }
        removed
    }

    /// The mounts, oldest first.
    #[must_use]
    pub fn list(&self) -> Vec<(Vec<u8>, PathBuf)> {
        self.reload(false);
        self.binds.read().iter().map(|b| (b.target.clone(), b.host.clone())).collect()
    }

    /// The deepest bind mount at or above `path`: its target and host.
    fn covering(&self, path: &[u8]) -> Option<(Vec<u8>, PathBuf)> {
        self.reload(false);
        self.binds
            .read()
            .iter()
            .filter(|b| path == b.target.as_slice() || (path.starts_with(&b.target) && path.get(b.target.len()) == Some(&b'/')))
            .max_by_key(|b| b.target.len())
            .map(|b| (b.target.clone(), b.host.clone()))
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
            // The ashmem device as libcutils names it: after this boot's id.
            _ if path.strip_prefix(b"/dev/ashmem").is_some_and(|id| id == crate::procfs::boot_id().as_bytes()) => return Some(Node::Dev(DevNode::Ashmem)),
            b"/dev/omni-gpu" => return Some(Node::Dev(DevNode::OmniGpu)),
            b"/dev/fuse" => return Some(Node::Dev(DevNode::Fuse)),
            // Input devices, where the embedding made any (`crate::evdev`).
            b"/dev/input" => return Some(Node::Dir),
            _ if path.starts_with(b"/dev/input/") => {
                return crate::evdev::node_number(&path[b"/dev/input/".len()..]).map(|n| Node::Dev(DevNode::Input(n as u16)));
            }
            b"/proc/self/exe" => return Some(Node::Symlink { target: self.exe.clone() }),
            _ => {}
        }
        let bound = self.binds.covering(path);
        let mounts = bound.iter().chain(self.writable.iter());
        for (mount, host) in mounts {
            let host = if path == mount.as_slice() {
                host.clone()
            } else if path.starts_with(mount) && path.get(mount.len()) == Some(&b'/') {
                // A guest name the host cannot hold as one plain name is not there (see `host_path`).
                host_path(host, &path[mount.len() + 1..])?
            } else {
                continue;
            };
            let host = self.binds.redirect(host);
            return match std::fs::metadata(&host) {
                Ok(m) if m.is_dir() => Some(Node::HostDir { host }),
                Ok(_) => match self.owners.get(&host) {
                    // A symbolic link on a writable mount: a file holding its target (`symlinkat`).
                    Some(o) if o.mode & 0o170_000 == 0o120_000 => Some(Node::Symlink { target: std::fs::read(&host).ok()? }),
                    _ => Some(Node::HostFile { host }),
                },
                Err(_) if path == mount.as_slice() => Some(Node::HostDir { host }),
                Err(_) => None,
            };
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
                .map(|h| self.binds.redirect(h))
        })
    }

    /// The host path of a guest path on a writable or bind mount, whether or not anything is there.
    #[must_use]
    pub fn host_of(&self, path: &[u8]) -> Option<PathBuf> {
        self.host_for_missing(path)
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
                    b"/" => &[b"dev", b"proc", b"data", b"tmp", b"mnt", b"storage"],
                    b"/dev" => &[b"null", b"zero", b"random", b"urandom", b"__properties__", b"input"],
                    b"/proc" => &[b"self"],
                    b"/proc/self" => &[b"exe"],
                    _ => &[],
                };
                if dir.path.as_slice() == b"/dev/input" {
                    for n in 0..crate::evdev::count() {
                        let name = format!("event{n}").into_bytes();
                        out.push(DirEnt { ino: ino_of(&child(&name)), kind: DT_CHR, name });
                    }
                }
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
