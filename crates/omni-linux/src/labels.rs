//! **SELinux labels of an instance's directories, kept across boots.** Extended attributes live in
//! a host process's memory (`crate::xattr`), so every label `restorecon` set was gone at the next
//! boot of a saved device: installd then found every app's data directory "unlabeled" (`Detected
//! label change from u:object_r:unlabeled:s0 ... running recursive restorecon`) and relabelled each
//! app's whole tree -- Roblox's thousands of cache files with it -- at every boot, inside
//! system_server's `AppDataPrepare` (2.1 s of a saved device's boot, s19). installd looks at a data
//! directory's own label only, so directories' labels are what is kept: in
//! `<instance>/.omni-labels`, a log replayed (and compacted) when the instance is first used, as
//! [`crate::owners`] keeps owners. `OMNI_KEEP_LABELS=0`: not kept.
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

/// The kept labels of an instance's directories, by host path.
pub struct Labels {
    instance: PathBuf,
    map: Mutex<BTreeMap<PathBuf, Vec<u8>>>,
    log: Mutex<Option<std::fs::File>>,
}

const LOG: &str = ".omni-labels";

/// Whether labels are kept (`OMNI_KEEP_LABELS=0`: not).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_KEEP_LABELS").as_deref() != Ok("0"))
}

impl Labels {
    /// The labels of the instance at `instance` (one table per instance directory, loaded from its
    /// log the first time); `None` when off.
    #[must_use]
    pub fn of(instance: &Path) -> Option<Arc<Self>> {
        if !on() {
            return None;
        }
        static TABLES: OnceLock<Mutex<HashMap<PathBuf, Arc<Labels>>>> = OnceLock::new();
        let mut tables = TABLES.get_or_init(Default::default).lock();
        Some(Arc::clone(tables.entry(instance.to_path_buf()).or_insert_with(|| Arc::new(Self::load(instance)))))
    }

    fn load(instance: &Path) -> Self {
        let path = instance.join(LOG);
        let mut map = BTreeMap::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                if let Some((label, rel)) = line.split_once('\t') {
                    let mut value = label.as_bytes().to_vec();
                    value.push(0);
                    map.insert(instance.join(rel), value);
                }
            }
        }
        // Compacted: the last label of each directory still there, one line each.
        map.retain(|host: &PathBuf, _| host.is_dir());
        let mut out = String::new();
        for (host, label) in &map {
            if let (Some(rel), Some(label)) = (relative(instance, host), printable(label)) {
                out.push_str(&format!("{label}\t{rel}\n"));
            }
        }
        if !out.is_empty() || path.exists() {
            let _ = std::fs::write(&path, out);
        }
        Self { instance: instance.to_path_buf(), map: Mutex::new(map), log: Mutex::new(None) }
    }

    /// The label kept for the directory at `host`, NUL-terminated as the kernel reports it.
    #[must_use]
    pub fn get(&self, host: &Path) -> Option<Vec<u8>> {
        self.map.lock().get(host).cloned()
    }

    /// `label` (NUL-terminated) set on `host`: kept if `host` is a directory of the instance.
    pub fn set(&self, host: &Path, label: &[u8]) {
        if !host.is_dir() {
            return;
        }
        let (Some(rel), Some(text)) = (relative(&self.instance, host), printable(label)) else { return };
        let mut map = self.map.lock();
        if map.get(host).is_some_and(|old| old == label) {
            return;
        }
        map.insert(host.to_path_buf(), label.to_vec());
        drop(map);
        let mut log = self.log.lock();
        if log.is_none() {
            *log = std::fs::OpenOptions::new().append(true).create(true).open(self.instance.join(LOG)).ok();
        }
        if let Some(file) = log.as_mut() {
            let _ = file.write_all(format!("{text}\t{rel}\n").as_bytes());
        }
    }
}

/// A label as one line can hold it: without its NUL, no tab or newline, UTF-8.
fn printable(label: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(label.strip_suffix(&[0]).unwrap_or(label)).ok()?;
    (!text.is_empty() && !text.contains(['\t', '\n', '\0'])).then_some(text)
}

/// `host` relative to the instance, `/`-separated; `None` outside it or for a name a line cannot hold.
fn relative(instance: &Path, host: &Path) -> Option<String> {
    let rel = host.strip_prefix(instance).ok()?;
    let parts: Vec<&str> = rel.components().map(|c| c.as_os_str().to_str()).collect::<Option<_>>()?;
    let rel = parts.join("/");
    (!rel.is_empty() && !rel.contains(['\t', '\n'])).then_some(rel)
}

#[cfg(test)]
mod tests {
    use super::Labels;

    #[test]
    fn a_directory_s_label_outlives_the_process_and_a_file_s_is_not_kept() {
        let dir = std::env::temp_dir().join(format!("omni-labels-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("data/data/com.example.app");
        std::fs::create_dir_all(app.join("cache")).unwrap();
        std::fs::write(app.join("cache/f"), b"x").unwrap();
        let label = b"u:object_r:app_data_file:s0:c512,c768\0";
        {
            let t = Labels::load(&dir);
            t.set(&app, label);
            t.set(&app.join("cache"), b"u:object_r:app_data_file:s0\0");
            t.set(&app.join("cache"), label);
            t.set(&app.join("cache/f"), label);
        }
        let t = Labels::load(&dir);
        assert_eq!(t.get(&app).as_deref(), Some(&label[..]));
        assert_eq!(t.get(&app.join("cache")).as_deref(), Some(&label[..]), "the last one");
        assert_eq!(t.get(&app.join("cache/f")), None, "files are not kept");
        // A directory removed is dropped when the log is next compacted.
        std::fs::remove_dir_all(app.join("cache")).unwrap();
        let t = Labels::load(&dir);
        assert_eq!(t.get(&app.join("cache")), None);
        assert_eq!(std::fs::read_to_string(dir.join(".omni-labels")).unwrap().lines().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
