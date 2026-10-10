//! Who owns a file on a writable mount, and its permission bits -- what an ext4 inode holds and
//! the host cannot (Windows keeps no Linux uid, gid or mode). A process creating a file owns it,
//! with the mode it asked for less its umask; `chown` and `chmod` change them; `stat` reports
//! them. installd checks each app directory's owner and mode before it trusts it
//! (`fs_prepare_dir_strict`), so they must hold across boots: each instance keeps them in
//! `<instance>/.omni-owners`, a log replayed (and compacted) when the instance is first used.
//!
//! The table is ordered (a `BTreeMap`): `Path` orders by components, so a directory and everything
//! under it are one range, and a rename moves that range alone. It was a hash map scanned whole,
//! twice, at every rename -- ~20 ms a rename once a device held ~18 000 owned files, which an app
//! writing its cache file by file (a temporary name, then renamed) paid on one thread for most of
//! a game's load (Roblox joining PS99, 2026-10-02: ~200 renames each 5 s, ~4 s of them).
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

/// An inode's owner and permission bits (`0o7777`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
}

/// The owners of an instance's files, by host path.
pub struct Owners {
    instance: Option<PathBuf>,
    map: Mutex<BTreeMap<PathBuf, Owner>>,
    log: Mutex<Option<std::fs::File>>,
}

const LOG: &str = ".omni-owners";

impl Owners {
    /// The owners of the instance at `instance` (one table per instance directory, loaded from its
    /// log the first time).
    #[must_use]
    pub fn of(instance: &Path) -> Arc<Self> {
        static TABLES: OnceLock<Mutex<HashMap<PathBuf, Arc<Owners>>>> = OnceLock::new();
        let mut tables = TABLES.get_or_init(Default::default).lock();
        Arc::clone(tables.entry(instance.to_path_buf()).or_insert_with(|| Arc::new(Self::load(instance))))
    }

    /// A table kept in memory only (a VFS with no instance: tests of single handlers).
    #[must_use]
    pub fn detached() -> Arc<Self> {
        Arc::new(Self { instance: None, map: Mutex::default(), log: Mutex::new(None) })
    }

    fn load(instance: &Path) -> Self {
        let path = instance.join(LOG);
        let mut map = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                let fields: Vec<&str> = line.split('\t').collect();
                match fields[..] {
                    ["S", mode, uid, gid, rel] => {
                        if let (Ok(mode), Ok(uid), Ok(gid)) = (u32::from_str_radix(mode, 8), uid.parse(), gid.parse()) {
                            map.insert(instance.join(rel), Owner { uid, gid, mode });
                        }
                    }
                    ["D", rel] => {
                        map.remove(&instance.join(rel));
                    }
                    ["R", from, to] => move_tree(&mut map, &instance.join(from), &instance.join(to)),
                    _ => {}
                }
            }
        }
        // Compacted: the table as it is now, one line a file.
        let _ = std::fs::create_dir_all(instance);
        let mut out = String::new();
        for (host, o) in &map {
            if let Some(rel) = relative(instance, host) {
                out.push_str(&format!("S\t{:o}\t{}\t{}\t{rel}\n", o.mode, o.uid, o.gid));
            }
        }
        let _ = std::fs::write(&path, out);
        let log = std::fs::OpenOptions::new().append(true).create(true).open(&path).ok();
        Self { instance: Some(instance.to_path_buf()), map: Mutex::new(map), log: Mutex::new(log) }
    }

    fn append(&self, line: impl FnOnce(&Path) -> Option<String>) {
        let Some(instance) = &self.instance else { return };
        let Some(line) = line(instance) else { return };
        if let Some(log) = self.log.lock().as_mut() {
            let _ = log.write_all(line.as_bytes());
        }
    }

    /// The instance directory this table is of (`None`: a detached one).
    #[must_use]
    pub fn instance(&self) -> Option<&Path> {
        self.instance.as_deref()
    }

    #[must_use]
    pub fn get(&self, host: &Path) -> Option<Owner> {
        self.map.lock().get(host).copied()
    }

    pub fn set(&self, host: &Path, owner: Owner) {
        self.map.lock().insert(host.to_path_buf(), owner);
        self.append(|i| relative(i, host).map(|rel| format!("S\t{:o}\t{}\t{}\t{rel}\n", owner.mode, owner.uid, owner.gid)));
    }

    /// A file removed. Said in the log whether or not this process's table held it: each host
    /// process has its own table, loaded when it started, and the process that removes a file is
    /// often not the one that made it (installd removing an app's data at its uninstall) -- the
    /// file's entry would otherwise outlive it in the log, the table growing with every app
    /// installed and removed (~500 entries to ~20 000 on a warm device in an hour of Roblox
    /// sessions, 2026-10-02).
    pub fn forget(&self, host: &Path) {
        self.map.lock().remove(host);
        self.append(|i| relative(i, host).map(|rel| format!("D\t{rel}\n")));
    }

    /// A file or directory renamed: it, and everything under it, keeps its owner.
    pub fn rename(&self, from: &Path, to: &Path) {
        move_tree(&mut self.map.lock(), from, to);
        self.append(|i| Some(format!("R\t{}\t{}\n", relative(i, from)?, relative(i, to)?)));
    }
}

/// `root` and everything under it: one range of the table (`Path` orders by components, so a
/// path's descendants come right after it, before its next sibling).
fn under(map: &BTreeMap<PathBuf, Owner>, root: &Path) -> Vec<PathBuf> {
    map.range::<Path, _>((Bound::Included(root), Bound::Unbounded)).take_while(|(p, _)| p.starts_with(root)).map(|(p, _)| p.clone()).collect()
}

fn move_tree(map: &mut BTreeMap<PathBuf, Owner>, from: &Path, to: &Path) {
    let moved: Vec<(PathBuf, Owner)> = under(map, from).into_iter().filter_map(|p| map.remove(&p).map(|o| (p, o))).collect();
    // What the target replaces goes.
    for p in under(map, to) {
        map.remove(&p);
    }
    for (p, o) in moved {
        if let Ok(rest) = p.strip_prefix(from) {
            map.insert(to.join(rest), o);
        }
    }
}

/// `host` relative to the instance, `/`-separated; `None` for a path the log cannot hold on a
/// line (a tab or a newline in a name) or one outside the instance.
fn relative(instance: &Path, host: &Path) -> Option<String> {
    let rel = host.strip_prefix(instance).ok()?;
    let parts: Vec<&str> = rel.components().map(|c| c.as_os_str().to_str()).collect::<Option<_>>()?;
    let rel = parts.join("/");
    (!rel.contains(['\t', '\n'])).then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::{Owner, Owners};

    #[test]
    fn the_log_brings_the_table_back() {
        let dir = std::env::temp_dir().join(format!("omni-owners-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let o = |uid, mode| Owner { uid, gid: uid, mode };
        {
            let t = Owners::load(&dir);
            t.set(&dir.join("data/a"), o(1000, 0o771));
            t.set(&dir.join("data/a/b"), o(10001, 0o700));
            t.set(&dir.join("data/c"), o(2000, 0o644));
            t.rename(&dir.join("data/a"), &dir.join("data/z"));
            t.forget(&dir.join("data/c"));
        }
        let t = Owners::load(&dir);
        assert_eq!(t.get(&dir.join("data/z")), Some(o(1000, 0o771)));
        assert_eq!(t.get(&dir.join("data/z/b")), Some(o(10001, 0o700)));
        assert_eq!(t.get(&dir.join("data/a")), None);
        assert_eq!(t.get(&dir.join("data/c")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rename_moves_the_tree_and_no_sibling_and_replaces_the_target() {
        let t = Owners::detached();
        let o = |uid| Owner { uid, gid: uid, mode: 0o700 };
        let p = |s: &str| std::path::Path::new("/i/data").join(s);
        for (name, uid) in [("a", 1), ("a/b", 2), ("a/b/c", 3), ("a.txt", 4), ("ab", 5), ("z", 6), ("z/old", 7)] {
            t.set(&p(name), o(uid));
        }
        t.rename(&p("a"), &p("z"));
        assert_eq!(t.get(&p("z")), Some(o(1)));
        assert_eq!(t.get(&p("z/b")), Some(o(2)));
        assert_eq!(t.get(&p("z/b/c")), Some(o(3)));
        assert_eq!(t.get(&p("z/old")), None, "what the target held is replaced");
        assert_eq!((t.get(&p("a")), t.get(&p("a/b"))), (None, None));
        assert_eq!((t.get(&p("a.txt")), t.get(&p("ab"))), (Some(o(4)), Some(o(5))), "siblings that share a prefix stay");
        // A file renamed over another, as a cache writes one: a temporary name, then the real one.
        t.set(&p("ab.tmp"), o(8));
        t.rename(&p("ab.tmp"), &p("ab"));
        assert_eq!((t.get(&p("ab")), t.get(&p("ab.tmp"))), (Some(o(8)), None));
    }

    #[test]
    fn a_rename_in_a_large_table_is_not_a_scan_of_it() {
        let t = Owners::detached();
        let o = Owner { uid: 10000, gid: 10000, mode: 0o600 };
        let root = std::path::Path::new("/instance/data/data/com.example.app/cache/store");
        for i in 0..50_000 {
            t.set(&root.join(format!("{:02x}/{i:08x}", i % 256)), o);
        }
        let started = std::time::Instant::now();
        for i in 0..2_000 {
            let tmp = root.join(format!("{:02x}/{i:08x}.tmp", i % 256));
            t.set(&tmp, o);
            t.rename(&tmp, &root.join(format!("{:02x}/new{i:08x}", i % 256)));
        }
        // A scan of the table at each rename took ~20 ms each here: 2 000 of them, ~40 s.
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "2000 renames took {:?}", started.elapsed());
        assert_eq!(t.get(&root.join("07/new00000007")), Some(o));
    }
}
