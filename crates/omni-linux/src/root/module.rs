//! The module catalog: standard Magisk modules (`module.prop` plus a file tree), as a folder or a
//! `.zip`, read the same way either form. Discovery only; installing a module is a later task.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// A module's `module.prop`. `id` is required; every other field defaults to empty / 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleProp {
    pub id: String,
    pub name: String,
    pub version: String,
    pub version_code: i64,
    pub author: String,
    pub description: String,
}

/// Whether `name` is one safe path component: not empty, not starting with `.` (so not `.`/`..`),
/// and with no separator or NUL. Module ids become host paths.
#[must_use]
pub fn is_safe_component(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}

impl ModuleProp {
    /// Parse `key=value` lines. A missing or empty `id` is an error naming it.
    pub fn parse(text: &str) -> Result<ModuleProp, String> {
        let mut p = ModuleProp {
            id: String::new(),
            name: String::new(),
            version: String::new(),
            version_code: 0,
            author: String::new(),
            description: String::new(),
        };
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "id" => p.id = v.to_string(),
                "name" => p.name = v.to_string(),
                "version" => p.version = v.to_string(),
                "versionCode" => {
                    // Lenient, as Magisk is: a bad versionCode is 0, not a reason to drop the module.
                    p.version_code = v.parse().unwrap_or(0);
                }
                "author" => p.author = v.to_string(),
                "description" => p.description = v.to_string(),
                _ => {}
            }
        }
        if p.id.is_empty() {
            return Err("module.prop: missing required field `id`".to_string());
        }
        if !is_safe_component(&p.id) {
            return Err(format!("module.prop: unsafe `id` {:?}", p.id));
        }
        Ok(p)
    }
}

/// Where a module's files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleSource {
    Dir(PathBuf),
    Zip(PathBuf),
}

impl ModuleSource {
    fn path(&self) -> &Path {
        match self {
            ModuleSource::Dir(p) | ModuleSource::Zip(p) => p,
        }
    }
}

/// A discovered module.
#[derive(Debug, Clone)]
pub struct Module {
    pub prop: ModuleProp,
    pub source: ModuleSource,
}

/// A name `Module::list` may return: never empty, `.`/`..`, or with a separator or NUL (a zip entry
/// can say anything; dotfiles such as `.replace` are fine).
fn is_listable(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains(['/', '\\', '\0'])
}

fn norm(rel: &str) -> &str {
    rel.trim_matches('/')
}

impl Module {
    /// The bytes of `rel` (forward-slash path inside the module), or `None`.
    #[must_use]
    pub fn read(&self, rel: &str) -> Option<Vec<u8>> {
        let rel = norm(rel);
        match &self.source {
            ModuleSource::Dir(d) => std::fs::read(d.join(rel)).ok(),
            ModuleSource::Zip(z) => omni_apk::Apk::open(z).ok()?.read_named(rel).ok(),
        }
    }

    /// Whether `rel` exists (a file, or a directory in a folder module / a path prefix in a zip).
    #[must_use]
    pub fn has(&self, rel: &str) -> bool {
        let rel = norm(rel);
        match &self.source {
            ModuleSource::Dir(d) => d.join(rel).exists(),
            ModuleSource::Zip(z) => {
                let Ok(apk) = omni_apk::Apk::open(z) else { return false };
                let dir = format!("{rel}/");
                apk.entry(rel).is_some() || apk.entries().iter().any(|e| e.name().starts_with(&dir))
            }
        }
    }

    /// The names directly under `rel_dir`, sorted (`""` is the module root).
    #[must_use]
    pub fn list(&self, rel_dir: &str) -> Vec<String> {
        let rel_dir = norm(rel_dir);
        match &self.source {
            ModuleSource::Dir(d) => {
                let Ok(rd) = std::fs::read_dir(d.join(rel_dir)) else { return Vec::new() };
                let mut v: Vec<String> = rd.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
                v.retain(|n| is_listable(n));
                v.sort();
                v
            }
            ModuleSource::Zip(z) => {
                let Ok(apk) = omni_apk::Apk::open(z) else { return Vec::new() };
                let prefix = if rel_dir.is_empty() { String::new() } else { format!("{rel_dir}/") };
                let mut seen = BTreeSet::new();
                for e in apk.entries() {
                    if let Some(rest) = e.name().strip_prefix(prefix.as_str()) {
                        let first = rest.split('/').next().unwrap_or("");
                        if is_listable(first) {
                            seen.insert(first.to_string());
                        }
                    }
                }
                seen.into_iter().collect()
            }
        }
    }
}

/// Every module found in the user and built-in directories.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub modules: Vec<Module>,
}

fn scan(dir: &Path, out: &mut Vec<Module>) -> Result<(), String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Ok(()) };
    let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let (source, text) = if path.is_dir() {
            let Ok(text) = std::fs::read_to_string(path.join("module.prop")) else { continue };
            (ModuleSource::Dir(path.clone()), text)
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
            // A zip that is unreadable or has no module.prop is not a module: skipped, like a dir.
            let Ok(apk) = omni_apk::Apk::open(&path) else { continue };
            if apk.entry("module.prop").is_none() {
                continue;
            }
            let bytes = apk.read_named("module.prop").map_err(|e| format!("{}: module.prop: {e}", path.display()))?;
            (ModuleSource::Zip(path.clone()), String::from_utf8_lossy(&bytes).into_owned())
        } else {
            continue;
        };
        let prop = ModuleProp::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        out.push(Module { prop, source });
    }
    Ok(())
}

impl Catalog {
    /// Scan `user_dir` (first) then `builtin_dir`. Each subdirectory holding a `module.prop`, and
    /// each `*.zip`, is a module; other subdirectories are skipped. A duplicate id is an error naming
    /// both sources.
    pub fn discover(builtin_dir: &Path, user_dir: Option<&Path>) -> Result<Catalog, String> {
        let mut found = Vec::new();
        if let Some(u) = user_dir {
            scan(u, &mut found)?;
        }
        scan(builtin_dir, &mut found)?;
        for (i, m) in found.iter().enumerate() {
            if let Some(a) = found[..i].iter().find(|a| a.prop.id == m.prop.id) {
                return Err(format!(
                    "module id {} is in both {} and {}",
                    m.prop.id,
                    a.source.path().display(),
                    m.source.path().display()
                ));
            }
        }
        Ok(Catalog { modules: found })
    }

    #[must_use]
    pub fn find(&self, id: &str) -> Option<&Module> {
        self.modules.iter().find(|m| m.prop.id == id)
    }

    #[must_use]
    pub fn list(&self) -> &[Module] {
        &self.modules
    }
}

/// The modules shipped with the repo.
#[must_use]
pub fn builtin_dir(repo_root: &Path) -> PathBuf {
    repo_root.join("modules")
}

/// `$OMNI_MODULES`, else `<home>/.omnidroid/modules` (home from `USERPROFILE` / `HOME`, std only).
#[must_use]
pub fn user_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("OMNI_MODULES").or_else(|| var("USERPROFILE").or_else(|| var("HOME")).map(|h| h.join(".omnidroid").join("modules")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn data(rel: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(rel) }

    #[test]
    fn parses_a_module_prop() {
        let p = ModuleProp::parse("id=mod_a\nname=Mod A\nversion=v1\nversionCode=1\nauthor=me\ndescription=x\n").unwrap();
        assert_eq!(p.id, "mod_a");
        assert_eq!(p.version_code, 1);
    }

    #[test]
    fn rejects_a_module_with_no_id() {
        assert!(ModuleProp::parse("name=No Id\n").is_err());
    }

    #[test]
    fn discovers_a_dir_module_and_reads_a_file() {
        let cat = Catalog::discover(&data(""), None).unwrap();
        let m = cat.find("mod_a").expect("mod-a present");
        assert_eq!(m.read("system/etc/a.txt").as_deref(), Some(&b"hello\n"[..]));
        assert!(m.has("system/etc/a.txt"));
        assert_eq!(m.list("system/etc"), vec!["a.txt".to_string()]);
    }

    fn crc32(d: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in d {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    /// A STORED (method 0) zip of `files`.
    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let (mut out, mut cd) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let off = out.len() as u32;
            let crc = crc32(data);
            let n = name.len() as u16;
            let len = data.len() as u32;
            out.extend([0x50, 0x4b, 3, 4, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            for v in [crc, len, len] { out.extend(v.to_le_bytes()); }
            out.extend(n.to_le_bytes());
            out.extend([0, 0]);
            out.extend(name.as_bytes());
            out.extend(*data);
            cd.extend([0x50, 0x4b, 1, 2, 10, 0, 10, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            for v in [crc, len, len] { cd.extend(v.to_le_bytes()); }
            cd.extend(n.to_le_bytes());
            cd.extend([0u8; 12]);
            cd.extend(off.to_le_bytes());
            cd.extend(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_len = cd.len() as u32;
        out.extend(cd);
        out.extend([0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
        out.extend((files.len() as u16).to_le_bytes());
        out.extend((files.len() as u16).to_le_bytes());
        out.extend(cd_len.to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("omni-module-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn dir_module(root: &Path, dir: &str, id: &str) {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("module.prop"), format!("id={id}
")).unwrap();
    }

    #[test]
    fn discovers_a_zip_module_and_reads_it_like_a_dir() {
        let d = temp("zip");
        let zip = make_zip(&[
            ("module.prop", b"id=mod_z
name=Z
"),
            ("system/etc/z.txt", b"zed
"),
        ]);
        std::fs::write(d.join("mod-z.zip"), zip).unwrap();
        let cat = Catalog::discover(&d, None).unwrap();
        let m = cat.find("mod_z").expect("zip module present");
        assert!(matches!(m.source, ModuleSource::Zip(_)));
        assert_eq!(m.read("system/etc/z.txt").as_deref(), Some(&b"zed
"[..]));
        assert!(m.has("system/etc/z.txt"));
        assert!(m.has("system"));
        assert!(!m.has("system/nope"));
        assert_eq!(m.list(""), vec!["module.prop".to_string(), "system".to_string()]);
        assert_eq!(m.list("system/etc"), vec!["z.txt".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn duplicate_id_names_both_sources() {
        let (a, b) = (temp("dup-a"), temp("dup-b"));
        dir_module(&a, "one", "same");
        dir_module(&b, "two", "same");
        let e = Catalog::discover(&b, Some(&a)).unwrap_err();
        assert!(e.contains(&a.join("one").display().to_string()), "{e}");
        assert!(e.contains(&b.join("two").display().to_string()), "{e}");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn skips_non_module_dirs_and_junk_zips() {
        let d = temp("skip");
        std::fs::create_dir_all(d.join("not-a-module/sub")).unwrap();
        dir_module(&d, "good", "good_mod");
        std::fs::write(d.join("junk.zip"), b"this is not a zip").unwrap();
        std::fs::write(d.join("noprop.zip"), make_zip(&[("a.txt", b"x")])).unwrap();
        let cat = Catalog::discover(&d, None).unwrap();
        assert_eq!(cat.list().len(), 1);
        assert!(cat.find("good_mod").is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bad_version_code_is_zero_not_an_error() {
        assert_eq!(ModuleProp::parse("id=x
versionCode=abc
").unwrap().version_code, 0);
        assert_eq!(ModuleProp::parse("id=x
versionCode=
").unwrap().version_code, 0);
    }
}
