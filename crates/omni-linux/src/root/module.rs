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
                    p.version_code = v.parse().map_err(|_| format!("module.prop: versionCode `{v}` is not a number"))?;
                }
                "author" => p.author = v.to_string(),
                "description" => p.description = v.to_string(),
                _ => {}
            }
        }
        if p.id.is_empty() {
            return Err("module.prop: missing required field `id`".to_string());
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
                        if !first.is_empty() {
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
            let apk = omni_apk::Apk::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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
}
