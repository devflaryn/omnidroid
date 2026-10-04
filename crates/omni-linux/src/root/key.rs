//! What a launcher needs to turn root on: the profile a request describes, the catalog and assets
//! it stages from, and the short hash that keeps rooted saved/warm devices apart from the rest.
use std::path::Path;

use sha2::{Digest, Sha256};

use super::module::{is_listable, Module};
use super::profile::SuPolicy;
use super::{Catalog, MagiskAssets, Profile};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn walk(module: &Module, rel: &str, h: &mut Sha256) {
    for name in module.list(rel) {
        if !is_listable(&name) {
            continue;
        }
        let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if !module.list(&child).is_empty() {
            walk(module, &child, h);
        } else if let Some(bytes) = module.read(&child) {
            h.update(format!("f {child} {}\n", bytes.len()).as_bytes());
            h.update(&bytes);
        } else if module.has(&child) {
            h.update(format!("d {child}\n").as_bytes());
        }
    }
}

/// A module's content hash: its files' relative paths and bytes, in sorted order (the same walk
/// `install::stage` copies by), so a folder and a zip of the same files agree.
#[must_use]
pub fn module_sha(module: &Module) -> String {
    let mut h = Sha256::new();
    walk(module, "", &mut h);
    hex(&h.finalize())
}

/// The first 8 hex digits of the SHA-256 of the profile text, each module's `(id, sha)`, the Magisk
/// version code and the `magisk` binary's sha: the device's root identity.
#[must_use]
pub fn root_hash(profile_text: &str, modules: &[(&str, &str)], magisk_code: u32, magisk_sha: &str) -> String {
    let mut h = Sha256::new();
    h.update(format!("profile\n{profile_text}\n").as_bytes());
    for (id, sha) in modules {
        h.update(format!("module {id} {sha}\n").as_bytes());
    }
    h.update(format!("magisk {magisk_code}\nbinary {magisk_sha}\n").as_bytes());
    hex(&h.finalize())[..8].to_string()
}

/// The `magisk` tool binary's sha-256, as hex.
#[must_use]
pub fn magisk_binary_sha() -> String {
    hex(&Sha256::digest(super::tools::magisk_binary()))
}

/// A rooted device as asked for: what to stage and the hash that names it.
pub struct Request {
    pub profile: Profile,
    pub catalog: Catalog,
    pub assets: MagiskAssets,
    pub hash: String,
}

/// Build the request for `modules` and `su` (`all`, or comma-separated packages; `None`: only
/// root/shell may su). Errors say what is wrong: an unknown module, the assets not fetched.
///
/// # Errors
/// A catalog that cannot be read, an unknown module id, or the Magisk assets absent.
pub fn request(repo: &Path, modules: &[String], su: Option<&str>) -> Result<Request, String> {
    request_with(repo, modules, su, &[])
}

/// As [`request`], with the packages the root is hidden from (`denylist`): part of the profile text,
/// so part of the hash. Built-in module ids (`emu-hide`, `shamiko`, ...) need no catalog entry;
/// `shamiko` turns on whitelist hiding.
///
/// # Errors
/// As [`request`].
pub fn request_with(repo: &Path, modules: &[String], su: Option<&str>, denylist: &[String]) -> Result<Request, String> {
    let catalog = Catalog::discover(&super::module::builtin_dir(repo), super::module::user_dir().as_deref())?;
    let assets = MagiskAssets::find(repo)?;
    let mut profile = build_profile(modules, su, denylist);
    profile.magisk_code = assets.version_code;
    let mut shas = Vec::new();
    for id in modules.iter().filter(|m| !super::module::is_builtin(m)) {
        let m = catalog.find(id).ok_or_else(|| format!("module `{id}` is not in the catalog"))?;
        shas.push((id.as_str(), module_sha(m)));
    }
    let pairs: Vec<(&str, &str)> = shas.iter().map(|(i, s)| (*i, s.as_str())).collect();
    let hash = root_hash(&profile.serialize(), &pairs, assets.version_code, &magisk_binary_sha());
    Ok(Request { profile, catalog, assets, hash })
}

/// The request the `OMNI_R_ROOT` / `OMNI_R_MODULES` / `OMNI_R_SU` / `OMNI_R_DENYLIST` environment makes, if any.
///
/// # Errors
/// As [`request`].
pub fn from_env(repo: &Path) -> Result<Option<Request>, String> {
    let modules: Vec<String> = std::env::var("OMNI_R_MODULES")
        .unwrap_or_default()
        .split(',')
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    let denylist: Vec<String> = std::env::var("OMNI_R_DENYLIST")
        .unwrap_or_default()
        .split(',')
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect();
    if std::env::var("OMNI_R_ROOT").as_deref() != Ok("1") && modules.is_empty() && denylist.is_empty() {
        return Ok(None);
    }
    let su = std::env::var("OMNI_R_SU").ok().filter(|s| !s.is_empty());
    request_with(repo, &modules, su.as_deref(), &denylist).map(Some)
}

/// The profile `request_with` builds, before the catalog/assets are consulted.
fn build_profile(modules: &[String], su: Option<&str>, denylist: &[String]) -> Profile {
    let mut profile = Profile::parse("");
    profile.rooted = true;
    profile.module_ids = modules.to_vec();
    for pkg in denylist {
        profile.denylist_add(pkg);
    }
    if modules.iter().any(|m| m == "shamiko") {
        profile.shamiko = super::profile::Shamiko::Whitelist;
    }
    match su {
        Some("all") => profile.su = SuPolicy::All,
        Some(list) => {
            profile.su = SuPolicy::Packages(list.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect());
        }
        None => {}
    }
    profile
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_and_denylist_become_the_profile() {
        let p = build_profile(&["emu-hide".to_string(), "shamiko".to_string()], None, &["com.roblox.client".to_string()]);
        assert!(p.is_rooted());
        assert_eq!(p.denylist, vec!["com.roblox.client".to_string()]);
        assert!(matches!(p.shamiko, super::super::profile::Shamiko::Whitelist));
        assert_eq!(p.modules(), &["emu-hide".to_string(), "shamiko".to_string()]);
        assert!(p.spoofed(Some("com.roblox.client")));
        let text = p.serialize();
        assert!(text.contains("denylist=com.roblox.client") && text.contains("shamiko=whitelist"), "{text}");
        let none = build_profile(&[], None, &[]);
        assert!(!none.spoofed(None) && matches!(none.shamiko, super::super::profile::Shamiko::Off));
    }

    #[test]
    fn the_hash_follows_every_input() {
        let base = root_hash("root=1\n", &[("a", "s")], 1, "b");
        assert_eq!(base.len(), 8);
        assert_eq!(base, root_hash("root=1\n", &[("a", "s")], 1, "b"));
        assert_ne!(base, root_hash("root=1\nsu=all\n", &[("a", "s")], 1, "b"));
        assert_ne!(base, root_hash("root=1\n", &[("a", "t")], 1, "b"));
        assert_ne!(base, root_hash("root=1\n", &[("a", "s")], 2, "b"));
        assert_ne!(base, root_hash("root=1\n", &[("a", "s")], 1, "c"));
    }
}
