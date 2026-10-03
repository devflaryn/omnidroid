//! The host-side staging a launcher does before booting a rooted device: the profile (host-only,
//! the trust root), the selected modules installed under `<instance>/data/adb/modules/<id>/`, the
//! Magisk installer assets under `<instance>/data/adb/magisk/`, and the `enabled` marker that lets
//! `/vendor/bin/omni_root.sh` run the modules' scripts at boot.
use std::path::Path;

use super::module::Module;
use super::profile::PROFILE_FILE;
use super::{Catalog, MagiskAssets, Profile};

fn io<T>(r: std::io::Result<T>, what: &Path) -> Result<T, String> {
    r.map_err(|e| format!("{}: {e}", what.display()))
}

/// Copy `rel` of `module` (a directory when it lists children, else a file) under `dest`.
fn copy_tree(module: &Module, rel: &str, dest: &Path) -> Result<(), String> {
    io(std::fs::create_dir_all(dest), dest)?;
    for name in module.list(rel) {
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
    if !profile.is_rooted() {
        let _ = std::fs::remove_file(&profile_path);
        let _ = std::fs::remove_file(omni.join("enabled"));
        return Ok(());
    }
    let modules_dir = instance.join("data/adb/modules");
    let magisk_dir = instance.join("data/adb/magisk");
    for d in [&omni, &modules_dir, &magisk_dir] {
        io(std::fs::create_dir_all(d), d)?;
    }
    // Modules: each selected one installed afresh; every other installed one disabled.
    for id in profile.modules() {
        let module = catalog.find(id).ok_or_else(|| format!("module `{id}` is not in the catalog"))?;
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
    io(std::fs::write(omni.join("enabled"), b""), &omni)?;
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
}
