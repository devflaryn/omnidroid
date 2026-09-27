//! Who owns a file on a writable mount, and its permission bits -- what an ext4 inode holds and
//! the host cannot (Windows keeps no Linux uid, gid or mode). A process creating a file owns it,
//! with the mode it asked for less its umask; `chown` and `chmod` change them; `stat` reports
//! them. installd checks each app directory's owner and mode before it trusts it
//! (`fs_prepare_dir_strict`), so they must hold across boots: each instance keeps them in
//! `<instance>/.omni-owners`, a log replayed (and compacted) when the instance is first used.
use std::collections::HashMap;
use std::io::Write;
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
    map: Mutex<HashMap<PathBuf, Owner>>,
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
        let mut map = HashMap::new();
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

    #[must_use]
    pub fn get(&self, host: &Path) -> Option<Owner> {
        self.map.lock().get(host).copied()
    }

    pub fn set(&self, host: &Path, owner: Owner) {
        self.map.lock().insert(host.to_path_buf(), owner);
        self.append(|i| relative(i, host).map(|rel| format!("S\t{:o}\t{}\t{}\t{rel}\n", owner.mode, owner.uid, owner.gid)));
    }

    /// A file removed.
    pub fn forget(&self, host: &Path) {
        if self.map.lock().remove(host).is_some() {
            self.append(|i| relative(i, host).map(|rel| format!("D\t{rel}\n")));
        }
    }

    /// A file or directory renamed: it, and everything under it, keeps its owner.
    pub fn rename(&self, from: &Path, to: &Path) {
        move_tree(&mut self.map.lock(), from, to);
        self.append(|i| Some(format!("R\t{}\t{}\n", relative(i, from)?, relative(i, to)?)));
    }
}

fn move_tree(map: &mut HashMap<PathBuf, Owner>, from: &Path, to: &Path) {
    let moved: Vec<(PathBuf, Owner)> = map.iter().filter(|(p, _)| p.starts_with(from)).map(|(p, o)| (p.clone(), *o)).collect();
    // What the target replaces goes.
    map.retain(|p, _| !p.starts_with(to));
    for (p, o) in moved {
        map.remove(&p);
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
}
