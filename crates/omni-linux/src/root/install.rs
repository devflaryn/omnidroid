//! The host-side staging a launcher does before booting a rooted device: the profile (host-only,
//! the trust root), the selected modules installed under `<instance>/data/adb/modules/<id>/`, the
//! Magisk installer assets under `<instance>/data/adb/magisk/`, and the `enabled` marker that lets
//! `/vendor/bin/omni_root.sh` run the modules' scripts at boot.
use std::path::Path;

use super::module::{is_safe_component, Module};
use super::profile::PROFILE_FILE;
use super::{Catalog, MagiskAssets, Profile};

fn io<T>(r: std::io::Result<T>, what: &Path) -> Result<T, String> {
    r.map_err(|e| format!("{}: {e}", what.display()))
}

/// Copy `rel` of `module` (a directory when it lists children, else a file) under `dest`.
fn copy_tree(module: &Module, rel: &str, dest: &Path) -> Result<(), String> {
    io(std::fs::create_dir_all(dest), dest)?;
    for name in module.list(rel) {
        // A zip entry can name `..` or carry a separator: never a path out of `dest`.
        if !super::module::is_listable(&name) {
            continue;
        }
        let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        let target = dest.join(&name);
        if !module.list(&child).is_empty() {
            copy_tree(module, &child, &target)?;
        } else if let Some(bytes) = module.read(&child) {
            io(std::fs::write(&target, bytes), &target)?;
        } else if module.has(&child) {
            io(std::fs::create_dir_all(&target), &target)?; // an empty directory
        }
    }
    Ok(())
}

/// Stage `instance` as the device `profile` describes. A profile that is not rooted un-stages it
/// (no profile, no marker): the device boots as an ordinary one. Safe to run again.
pub fn stage(instance: &Path, profile: &Profile, catalog: &Catalog, assets: &MagiskAssets) -> Result<(), String> {
    let profile_path = instance.join(PROFILE_FILE);
    let omni = instance.join("data/adb/omni");
    let enabled = omni.join("enabled");
    let remove = |p: &Path| match std::fs::remove_file(p) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", p.display())),
        _ => Ok(()),
    };
    // The marker goes first and comes back last: a failure part-way never leaves it beside a
    // half-staged device.
    remove(&enabled)?;
    if !profile.is_rooted() {
        return remove(&profile_path);
    }
    // Every id becomes a host path: check them all before anything is touched.
    for id in profile.modules() {
        if !is_safe_component(id) {
            return Err(format!("unsafe module id {id:?}"));
        }
    }
    let modules_dir = instance.join("data/adb/modules");
    let magisk_dir = instance.join("data/adb/magisk");
    for d in [&omni, &modules_dir, &magisk_dir] {
        io(std::fs::create_dir_all(d), d)?;
    }
    // Modules: each selected one installed afresh; every other installed one disabled.
    for id in profile.modules().iter().filter(|m| !super::module::is_builtin(m)) {
        let module = catalog.find(id).ok_or_else(|| format!("module `{id}` is not in the catalog"))?;
        if !is_safe_component(&module.prop.id) {
            return Err(format!("unsafe module id {:?}", module.prop.id));
        }
        let dest = modules_dir.join(id);
        let _ = std::fs::remove_dir_all(&dest);
        copy_tree(module, "", &dest)?;
    }
    if let Ok(rd) = std::fs::read_dir(&modules_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.path().is_dir() && !profile.modules().iter().any(|m| *m == name) {
                io(std::fs::write(e.path().join("disable"), b""), &e.path())?;
            }
        }
    }
    // The installer environment the modules' customize.sh runs under.
    io(std::fs::copy(&assets.util_functions, magisk_dir.join("util_functions.sh")), &assets.util_functions)?;
    io(std::fs::copy(&assets.busybox, magisk_dir.join("busybox")), &assets.busybox)?;
    let version = assets
        .util_functions
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().trim_start_matches("magisk-").to_string())
        .unwrap_or_default();
    io(std::fs::write(omni.join("version"), format!("{version}\n")), &omni)?;
    io(std::fs::write(omni.join("version_code"), format!("{}\n", assets.version_code)), &omni)?;
    // The profile is host-only (the trust root); the marker is guest-visible and grants nothing.
    io(std::fs::write(&profile_path, profile.serialize()), &profile_path)?;
    io(std::fs::write(&enabled, b""), &enabled)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unrooted_profile_unstages() {
        let dir = std::env::temp_dir().join(format!("omni-stage-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data/adb/omni")).unwrap();
        std::fs::write(dir.join(PROFILE_FILE), "root=1\n").unwrap();
        std::fs::write(dir.join("data/adb/omni/enabled"), "").unwrap();
        let catalog = Catalog { modules: Vec::new() };
        let assets = MagiskAssets { util_functions: "u".into(), busybox: "b".into(), version_code: 1 };
        stage(&dir, &Profile::parse(""), &catalog, &assets).unwrap();
        assert!(!dir.join(PROFILE_FILE).exists() && !dir.join("data/adb/omni/enabled").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = !0u32;
        for &b in data {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
            }
        }
        !c
    }

    /// A stored (uncompressed) zip of `entries`.
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, data) in entries {
            let off = out.len() as u32;
            let mut common = Vec::new();
            common.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, stored, time, date
            common.extend_from_slice(&crc32(data).to_le_bytes());
            common.extend_from_slice(&(data.len() as u32).to_le_bytes());
            common.extend_from_slice(&(data.len() as u32).to_le_bytes());
            common.extend_from_slice(&(name.len() as u16).to_le_bytes());
            common.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&[0x50, 0x4b, 3, 4]);
            out.extend_from_slice(&common);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
            central.extend_from_slice(&[0x50, 0x4b, 1, 2, 20, 0]);
            central.extend_from_slice(&common);
            central.extend_from_slice(&[0; 10]); // comment, disk, attributes
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&[0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("omni-stage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_unsafe_module_id_is_rejected_and_nothing_outside_the_instance_is_touched() {
        for id in ["..", "../x", "/abs", "a/b", ".hidden", "a\\b", "C:evil", "C:", "C:\\x", ""] {
            assert!(super::super::ModuleProp::parse(&format!("id={id}\n")).is_err() || id.is_empty(), "{id}");
            assert!(!super::super::module::is_safe_component(id), "{id}");
        }
        assert!(!super::super::module::is_listable("C:evil") && !super::super::module::is_listable("C:") && !super::super::module::is_listable(".."));
        for ok in ["omni-test", "mod_a", "a.b"] {
            assert!(super::super::module::is_safe_component(ok), "{ok}");
        }
        for ok in ["system", "ok.txt", ".replace"] {
            assert!(super::super::module::is_listable(ok), "{ok}");
        }
        // A catalog built by hand (bypassing parse): stage() must reject before any remove or copy.
        let root = scratch("trav");
        let instance = root.join("inst");
        std::fs::create_dir_all(instance.join("data/adb/modules")).unwrap();
        // Where each unsafe id would resolve from data/adb/modules if the guard were absent.
        for (id, victim_dir) in [("..", instance.join("data/adb")), ("../victim", instance.join("data/adb/victim")), ("../../x", instance.join("data/x")), ("C:evil", instance.join("data/adb/modules/C:evil"))] {
            if std::fs::create_dir_all(&victim_dir).is_err() {
                continue; // `C:evil` is not a legal directory name on Windows
            }
            let keep = victim_dir.join("keep-me");
            std::fs::write(&keep, "1").unwrap();
            let module = Module {
                prop: super::super::ModuleProp { id: id.into(), name: String::new(), version: String::new(), version_code: 0, author: String::new(), description: String::new() },
                source: super::super::ModuleSource::Dir(root.join("src")),
            };
            let catalog = Catalog { modules: vec![module] };
            let mut profile = Profile::parse("root=1\n");
            profile.module_ids.push(id.to_string());
            let assets = MagiskAssets { util_functions: "u".into(), busybox: "b".into(), version_code: 1 };
            let err = stage(&instance, &profile, &catalog, &assets).unwrap_err();
            assert!(err.contains("unsafe module id"), "{id}: {err}");
            assert!(keep.exists(), "{id}: the guard must reject before any remove or copy");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_zip_entry_named_dotdot_does_not_escape_the_module_dir() {
        let root = scratch("slip");
        let z = root.join("evil.zip");
        std::fs::write(&z, zip(&[("module.prop", b"id=evil\n"), ("../escaped.txt", b"x"), ("system/ok.txt", b"y")])).unwrap();
        let catalog = Catalog::discover(&root, None).unwrap();
        let module = catalog.find("evil").expect("module");
        let dest = root.join("out/mod");
        copy_tree(module, "", &dest).unwrap();
        assert!(dest.join("system/ok.txt").exists());
        assert!(!root.join("out/escaped.txt").exists() && !root.join("escaped.txt").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
