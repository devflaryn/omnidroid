//! The root layer over `/system`: the su/magisk tools and the enabled Magisk modules' `system/`
//! files, overlaid on the image for one instance and consulted on every path resolution
//! ([`crate::vfs::Vfs`]). Every file node is a host file (the tools are written under the instance
//! at build), so no new `Node` variant is needed.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::module::{Catalog, ModuleSource};
use super::{tools, Profile};
use crate::owners::{Owner, Owners};
use crate::vfs::Node;

/// What the layer has at a guest path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerNode {
    /// A file, held by this host file.
    File { host: PathBuf },
    /// A directory the layer's files lie under.
    Dir,
}

/// Directories of a module's `system/` that are their own partitions.
const PARTITIONS: [&str; 3] = ["vendor", "product", "system_ext"];
/// Marker files of a module directory that keep it from mounting.
const SKIP_MARKERS: [&str; 3] = ["disable", "remove", "skip_mount"];
const TOOLS: [&str; 3] = ["su", "magisk", "resetprop"];

#[derive(Debug, Default)]
pub struct Layer {
    files: BTreeMap<Vec<u8>, LayerNode>,
    /// Directories a module replaces wholesale (`.replace`): the image's children under them are gone.
    replaced: BTreeSet<Vec<u8>>,
}

fn parent_of(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) | None => b"/",
        Some(i) => &path[..i],
    }
}

impl Layer {
    fn add_file(&mut self, guest: Vec<u8>, host: PathBuf) {
        let mut dir = parent_of(&guest).to_vec();
        while dir.as_slice() != b"/" {
            let up = parent_of(&dir).to_vec();
            self.files.entry(dir).or_insert(LayerNode::Dir);
            dir = up;
        }
        self.files.insert(guest, LayerNode::File { host });
    }

    /// Whether `guest` is under a directory a module replaced (and so is not the image's to show).
    fn under_replaced(&self, guest: &[u8]) -> bool {
        self.replaced.iter().any(|r| guest.len() > r.len() && guest.starts_with(r) && guest[r.len()] == b'/')
    }

    fn overlay_dir(&mut self, host: &Path, guest: &str, top: bool) {
        let Ok(rd) = std::fs::read_dir(host) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for e in entries {
            let Ok(name) = e.file_name().into_string() else { continue };
            if name == ".replace" {
                continue;
            }
            // `system/vendor` etc. are the /vendor partitions, not directories of /system.
            let child = if top && PARTITIONS.contains(&name.as_str()) { format!("/{name}") } else { format!("{guest}/{name}") };
            let path = e.path();
            if path.is_dir() {
                if path.join(".replace").exists() {
                    // Drop what an earlier module put there: this module's directory replaces it.
                    let key = child.as_bytes().to_vec();
                    let under: Vec<Vec<u8>> = self.files.keys().filter(|k| k.len() > key.len() && k.starts_with(&key) && k[key.len()] == b'/').cloned().collect();
                    for k in under {
                        self.files.remove(&k);
                    }
                    self.replaced.insert(key);
                }
                self.overlay_dir(&path, &child, false);
            } else {
                self.add_file(child.into_bytes(), path);
            }
        }
    }

    /// The layer of `profile`'s enabled modules (found in `catalog`, installed as directories under
    /// `<instance>/data/adb/modules/<id>/`), plus the tools. Later modules win.
    #[must_use]
    pub fn build(profile: &Profile, catalog: &Catalog, instance: &Path) -> Layer {
        let mut layer = Layer::default();
        let tools_dir = instance.join("data/adb/omni/tools");
        let _ = std::fs::create_dir_all(&tools_dir);
        let owners = Owners::of(instance);
        for name in TOOLS {
            let host = tools_dir.join(name);
            if std::fs::read(&host).ok().as_deref() != Some(tools::magisk_binary()) {
                let _ = std::fs::write(&host, tools::magisk_binary());
            }
            owners.set(&host, Owner { uid: 0, gid: 0, mode: 0o755 });
            layer.add_file(format!("/system/bin/{name}").into_bytes(), host.clone());
            layer.add_file(format!("/debug_ramdisk/{name}").into_bytes(), host);
        }
        for id in profile.modules() {
            let Some(module) = catalog.find(id) else { continue };
            let ModuleSource::Dir(dir) = &module.source else { continue };
            if SKIP_MARKERS.iter().any(|m| dir.join(m).exists()) {
                continue;
            }
            layer.overlay_dir(&dir.join("system"), "/system", true);
        }
        layer
    }

    /// The layer of the instance at `instance`, or `None` when it has no rooted profile. One cache
    /// per instance directory (as [`crate::vfs::Binds::of`]), rebuilt when the profile file or
    /// `<instance>/data/adb/omni/layer.gen` changes (checked at most once a second).
    #[must_use]
    pub fn of(instance: &Path) -> Option<Arc<Layer>> {
        #[derive(Default)]
        struct Cell {
            state: parking_lot::Mutex<(Option<(Option<SystemTime>, Option<SystemTime>)>, Option<Instant>, Option<Arc<Layer>>)>,
        }
        static CELLS: OnceLock<parking_lot::Mutex<HashMap<PathBuf, Arc<Cell>>>> = OnceLock::new();
        let cell = Arc::clone(CELLS.get_or_init(Default::default).lock().entry(instance.to_path_buf()).or_default());
        let mut st = cell.state.lock();
        let now = Instant::now();
        if st.1.is_some_and(|at| now.duration_since(at) < Duration::from_secs(1)) {
            return st.2.clone();
        }
        st.1 = Some(now);
        let mtime = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let stamp = (mtime(instance.join(super::profile::PROFILE_FILE)), mtime(instance.join("data/adb/omni/layer.gen")));
        if st.0 == Some(stamp) {
            return st.2.clone();
        }
        st.0 = Some(stamp);
        st.2 = Profile::of(instance).map(|profile| {
            let catalog = Catalog::discover(&instance.join("data/adb/modules"), None).unwrap_or_default();
            Arc::new(Layer::build(&profile, &catalog, instance))
        });
        st.2.clone()
    }

    /// The node the layer has at `path`: a host file, or `Node::Dir` for a directory its files lie
    /// under (so the image's own directory, and its children, still show through `Vfs::list`).
    #[must_use]
    pub fn lookup(&self, path: &[u8]) -> Option<Node> {
        Some(match self.files.get(path)? {
            LayerNode::File { host } => Node::HostFile { host: host.clone() },
            LayerNode::Dir => Node::Dir,
        })
    }

    /// Whether `path` is an image entry a module's `.replace` removed.
    #[must_use]
    pub fn hides(&self, path: &[u8]) -> bool {
        !self.replaced.is_empty() && self.under_replaced(path)
    }

    /// The layer's entries directly under `dir`.
    #[must_use]
    pub fn children(&self, dir: &[u8]) -> Vec<(Vec<u8>, Node)> {
        let prefix: Vec<u8> = if dir == b"/" { b"/".to_vec() } else { [dir, b"/"].concat() };
        self.files
            .range(prefix.clone()..)
            .take_while(|(k, _)| k.starts_with(&prefix))
            .filter(|(k, _)| !k[prefix.len()..].contains(&b'/'))
            .filter_map(|(k, _)| Some((k[prefix.len()..].to_vec(), self.lookup(k)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_instance() -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let d = std::env::temp_dir().join(format!("omni-layer-test-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("data/adb/modules")).unwrap();
        d
    }

    fn install_fake_module(inst: &Path, id: &str, files: &[(&str, &[u8])], markers: &[&str]) {
        let dir = inst.join("data/adb/modules").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("module.prop"), format!("id={id}\nname={id}\nversion=1\nversionCode=1\nauthor=t\ndescription=t\n")).unwrap();
        for (rel, bytes) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        for m in markers {
            std::fs::write(dir.join(m), b"").unwrap();
        }
    }

    fn read_bytes(layer: &Layer, path: &[u8]) -> Option<Vec<u8>> {
        match layer.lookup(path)? {
            Node::HostFile { host } => std::fs::read(host).ok(),
            _ => None,
        }
    }

    #[test]
    fn overlays_adds_replaces_and_vendor_remap() {
        let inst = tmp_instance();
        install_fake_module(&inst, "a", &[
            ("system/etc/added.txt", b"A"),
            ("system/bin/sh", b"replaced-sh"),
            ("system/vendor/lib/v.so", b"V"),
        ], &[]);
        install_fake_module(&inst, "b", &[
            ("system/etc/added.txt", b"B"),
            ("system/fonts/.replace", b""),
            ("system/fonts/only.ttf", b"F"),
        ], &[]);
        let profile = crate::root::Profile::parse("root=1\nmodule=a\nmodule=b\n");
        let cat = crate::root::module::Catalog::discover(&inst.join("data/adb/modules"), None).unwrap();
        let layer = Layer::build(&profile, &cat, &inst);

        assert!(matches!(layer.lookup(b"/system/bin/su"), Some(_)));
        assert!(matches!(layer.lookup(b"/debug_ramdisk/su"), Some(_)));
        assert_eq!(read_bytes(&layer, b"/system/etc/added.txt"), Some(b"B".to_vec()));
        assert_eq!(read_bytes(&layer, b"/system/bin/sh"), Some(b"replaced-sh".to_vec()));
        assert!(layer.lookup(b"/vendor/lib/v.so").is_some());
        assert!(layer.lookup(b"/system/vendor/lib/v.so").is_none());
        let fonts: Vec<_> = layer.children(b"/system/fonts").into_iter().map(|(n, _)| n).collect();
        assert_eq!(fonts, vec![b"only.ttf".to_vec()]);
        assert!(layer.hides(b"/system/fonts/Roboto.ttf"));
        assert!(!layer.hides(b"/system/fonts"));
        assert!(!layer.hides(b"/system/etc/other.txt"));
    }

    #[test]
    fn disabled_and_skip_mount_modules_are_ignored() {
        let inst = tmp_instance();
        install_fake_module(&inst, "a", &[("system/etc/x.txt", b"A")], &["disable"]);
        install_fake_module(&inst, "b", &[("system/etc/y.txt", b"B")], &["skip_mount"]);
        let profile = crate::root::Profile::parse("root=1\nmodule=a\nmodule=b\n");
        let cat = crate::root::module::Catalog::discover(&inst.join("data/adb/modules"), None).unwrap();
        let layer = Layer::build(&profile, &cat, &inst);
        assert!(layer.lookup(b"/system/etc/x.txt").is_none());
        assert!(layer.lookup(b"/system/etc/y.txt").is_none());
    }

    #[test]
    fn of_needs_a_host_only_rooted_profile() {
        let inst = tmp_instance();
        assert!(Layer::of(&inst).is_none());
        // A profile the guest could write (under /data) grants nothing.
        std::fs::create_dir_all(inst.join("data/adb/omni")).unwrap();
        std::fs::write(inst.join("data/adb/omni/profile"), "root=1\n").unwrap();
        let inst2 = tmp_instance();
        std::fs::create_dir_all(inst2.join("data/adb/omni")).unwrap();
        std::fs::write(inst2.join("data/adb/omni/profile"), "root=1\n").unwrap();
        assert!(Layer::of(&inst2).is_none());
        let inst3 = tmp_instance();
        std::fs::write(inst3.join(".omni-root-profile"), "root=1\n").unwrap();
        let layer = Layer::of(&inst3).expect("rooted");
        assert!(layer.lookup(b"/system/bin/su").is_some());
        assert!(Profile::of(&inst3).is_some());
    }
}
