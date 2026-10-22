//! **App bundles**: an app shipped as several APKs in one file -- APKMirror's `.apkm`, `.xapk`,
//! `.apks` (bundletool's and SAI's), or any zip of APKs -- installed as Android installs them: the
//! base APK and its splits in one session (`pm install base.apk split.apk ...`).
//!
//! - **Known by its contents, not its name.** A file with `AndroidManifest.xml` at its root is an
//!   APK; a zip holding `*.apk` entries is a bundle, whatever its extension. Each APK in it says
//!   what it is in its own manifest: its `package`, and `split` for a split (`config.arm64_v8a`,
//!   `config.xxhdpi`, `config.en`, a feature).
//! - **Several packages.** A bundle may hold more than one app (a zip of APKs): its APKs are grouped
//!   by package, and the caller names the one to install when there is more than one
//!   ([`Bundle::choose`]).
//! - **Splits for this device.** The ABI splits of other ABIs (`config.armeabi_v7a`, `config.x86`,
//!   ...) are left out: the device is arm64-v8a only. Every other split (densities, languages,
//!   features) goes in, as a device that has them all would take them.
//! - **OBB files** (`.xapk`'s `Android/obb/<package>/*.obb`) go with their package: placed in its
//!   OBB directory once it is installed.
//! - **Extracted once.** A bundle's APKs are written to `<temp>/omni-bundles/<hash>/`, keyed by the
//!   bundle's SHA-256, and found there again the next time (a count of the files written is checked:
//!   a temp cleanup that took some of them has them extracted again).
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// The ABI split names (`config.<abi>`) of the ABIs this device does not run.
const OTHER_ABIS: [&str; 5] = ["armeabi_v7a", "armeabi", "x86", "x86_64", "mips"];
/// What the device runs.
const ABI: &str = "arm64_v8a";
/// Written last in an extraction's directory: the number of files extracted.
const DONE: &str = ".done";

/// One package's APKs (and OBB files) in a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSet {
    pub package: String,
    pub version_code: Option<u32>,
    pub launcher: Option<String>,
    /// The base APK (extracted).
    pub base: PathBuf,
    /// The splits to install with it, by name (extracted).
    pub splits: Vec<(String, PathBuf)>,
    /// Splits left out: another ABI's.
    pub left_out: Vec<String>,
    /// OBB files for its OBB directory (extracted).
    pub obbs: Vec<PathBuf>,
}

/// One package's APKs (each with its manifest) and OBB files, as found in a bundle.
type Found = (Vec<(omni_apk::LaunchInfo, PathBuf)>, Vec<PathBuf>);

/// A bundle's packages.
#[derive(Debug, Clone)]
pub struct Bundle {
    /// The bundle file.
    pub path: PathBuf,
    /// Its packages, by name.
    pub packages: Vec<PackageSet>,
}

/// Whether `path` is a bundle of APKs (a zip of `*.apk` with no manifest of its own), not an APK.
///
/// # Errors
/// The file is not a zip, or is neither an APK nor a bundle.
pub fn is_bundle(path: &Path) -> Result<bool, String> {
    let zip = omni_apk::Apk::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if zip.entry("AndroidManifest.xml").is_some() {
        return Ok(false);
    }
    if zip.entries().iter().any(|e| is_apk_name(e.name())) {
        return Ok(true);
    }
    Err(format!("{}: neither an APK (no AndroidManifest.xml) nor a bundle of APKs (no .apk in it)", path.display()))
}

fn is_apk_name(name: &str) -> bool {
    !name.ends_with('/') && name.to_ascii_lowercase().ends_with(".apk")
}

fn is_obb_name(name: &str) -> bool {
    !name.ends_with('/') && name.to_ascii_lowercase().ends_with(".obb")
}

/// An extracted file's own name: its `<n>-` (and an OBB's `<dir>__`) prefix off.
#[must_use]
pub fn own_name(extracted: &Path) -> String {
    let name = extracted.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match name.rsplit_once("__") {
        Some((_, own)) => own.to_string(),
        None => name.split_once('-').map_or(name.as_str(), |(_, own)| own).to_string(),
    }
}

/// Where bundles are extracted.
fn cache_root() -> PathBuf {
    std::env::temp_dir().join("omni-bundles")
}

/// A name safe as a file name: the entry's last component, other characters as `_`.
fn file_name(entry: &str) -> String {
    let last = entry.rsplit(['/', '\\']).next().unwrap_or(entry);
    last.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' }).collect()
}

impl Bundle {
    /// Read the bundle at `path`: its APKs extracted (or found extracted), grouped by package.
    ///
    /// # Errors
    /// Not a bundle, an entry that cannot be read (an encrypted `.apkm`), an APK whose manifest
    /// cannot be read, or a package with splits but no base APK.
    pub fn open(path: &Path) -> Result<Self, String> {
        let sha = crate::sha256_file(path)?;
        let dir = cache_root().join(&sha[..24]);
        let files = extracted(&dir).map_or_else(|| extract(path, &dir), Ok)?;
        let mut by_package: BTreeMap<String, Found> = BTreeMap::new();
        let mut obbs = Vec::new();
        for file in files {
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if is_apk_name(&name) {
                let info = omni_apk::launch_info_of(&file).map_err(|e| format!("{} in {}: {e}", name, path.display()))?;
                by_package.entry(info.package.clone()).or_default().0.push((info, file));
            } else if is_obb_name(&name) {
                obbs.push(file);
            }
        }
        // An OBB file is its package's when its directory (`Android/obb/<package>/`, kept in the
        // extracted name) or its own name (`main.<version>.<package>.obb`) says so.
        for obb in obbs {
            let name = obb.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let owner = by_package.keys().filter(|p| name.contains(&format!("{p}.obb")) || name.contains(&format!("{p}__"))).max_by_key(|p| p.len()).cloned();
            match owner.or_else(|| (by_package.len() == 1).then(|| by_package.keys().next().cloned()).flatten()) {
                Some(p) => by_package.get_mut(&p).expect("a key").1.push(obb),
                None => return Err(format!("{}: an OBB file ({name}) of no package in the bundle", path.display())),
            }
        }
        let mut packages = Vec::new();
        for (package, (apks, obbs)) in by_package {
            let (bases, splits): (Vec<_>, Vec<_>) = apks.into_iter().partition(|(info, _)| info.split.is_none());
            let Some((info, base)) = bases.into_iter().next() else {
                return Err(format!("{}: {package} has splits but no base APK", path.display()));
            };
            let (mut keep, mut left_out) = (Vec::new(), Vec::new());
            for (split, file) in splits {
                let name = split.split.unwrap_or_default();
                let abi = name.strip_prefix("config.").filter(|a| OTHER_ABIS.contains(a) || *a == ABI);
                if abi.is_some_and(|a| a != ABI) {
                    left_out.push(name);
                } else {
                    keep.push((name, file));
                }
            }
            keep.sort();
            packages.push(PackageSet { package, version_code: info.version_code, launcher: info.launcher, base, splits: keep, left_out, obbs });
        }
        Ok(Self { path: path.to_path_buf(), packages })
    }

    /// The package to install: `package` when given (it must be in the bundle), else the bundle's
    /// only one.
    ///
    /// # Errors
    /// `package` is not in the bundle, or none was given and the bundle holds several (the error
    /// names them: the caller asks which).
    pub fn choose(&self, package: Option<&str>) -> Result<&PackageSet, String> {
        match package {
            Some(p) => self.packages.iter().find(|s| s.package == p).ok_or_else(|| format!("{} holds no package {p}; it holds {}", self.path.display(), self.names())),
            None if self.packages.len() == 1 => Ok(&self.packages[0]),
            None => Err(format!("{} holds {} packages: {}. Say which one to install (`package`; on the command line `--package <name>`)", self.path.display(), self.packages.len(), self.names())),
        }
    }

    /// Its packages, as the error that asks for one lists them (with their launchers' presence).
    #[must_use]
    pub fn names(&self) -> String {
        self.packages
            .iter()
            .map(|s| format!("{}{}", s.package, if s.launcher.is_some() { "" } else { " (no launcher Activity)" }))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The files of a finished extraction in `dir`, when it is whole.
fn extracted(dir: &Path) -> Option<Vec<PathBuf>> {
    let want: usize = std::fs::read_to_string(dir.join(DONE)).ok()?.trim().parse().ok()?;
    let files: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.file_name().is_some_and(|n| n != DONE)).collect();
    (files.len() == want).then_some(files)
}

/// Extract `bundle`'s APK and OBB entries into `dir`, each under a name of its own: written beside
/// it first and renamed into place, so two launches of one bundle at once never see half of one.
fn extract(bundle: &Path, dir: &Path) -> Result<Vec<PathBuf>, String> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let zip = omni_apk::Apk::open(bundle).map_err(|e| format!("{}: {e}", bundle.display()))?;
    let tmp = dir.with_extension(format!("tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let written = (|| -> Result<usize, String> {
        let mut count = 0;
        for (i, entry) in zip.entries().iter().enumerate() {
            let name = entry.name();
            if !is_apk_name(name) && !is_obb_name(name) {
                continue;
            }
            // The OBB's directory names its package: kept in its name (`<package>__<file>`).
            let stem = file_name(name);
            let kept = match name.rsplit_once('/') {
                Some((parent, _)) if is_obb_name(name) => format!("{i:03}-{}__{stem}", file_name(parent)),
                _ => format!("{i:03}-{stem}"),
            };
            let at = tmp.join(kept);
            let mut out = std::io::BufWriter::new(std::fs::File::create(&at).map_err(|e| format!("{}: {e}", at.display()))?);
            zip.copy_entry_to(entry, &mut out, &at.display().to_string()).map_err(|e| format!("{name} in {}: {e}", bundle.display()))?;
            out.flush().map_err(|e| format!("{}: {e}", at.display()))?;
            count += 1;
        }
        std::fs::write(tmp.join(DONE), count.to_string()).map_err(|e| format!("{}: {e}", tmp.display()))?;
        Ok(count)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    // An older, partial extraction makes way; one finished meanwhile by another launch is kept.
    if extracted(dir).is_none() {
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::rename(&tmp, dir);
    }
    let _ = std::fs::remove_dir_all(&tmp);
    extracted(dir).ok_or_else(|| format!("{}: the bundle's APKs could not be put in {}", bundle.display(), dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_flat_and_safe() {
        assert_eq!(file_name("splits/config.arm64_v8a.apk"), "config.arm64_v8a.apk");
        assert_eq!(file_name("Android/obb/com.a/main 1.obb"), "main_1.obb");
        assert!(is_apk_name("base.APK"));
        assert!(!is_apk_name("dir.apk/"));
        assert!(is_obb_name("Android/obb/com.a/main.1.com.a.obb"));
        assert_eq!(own_name(Path::new("/x/007-com.a__main.1.com.a.obb")), "main.1.com.a.obb");
        assert_eq!(own_name(Path::new("/x/002-split_config.en.apk")), "split_config.en.apk");
    }
}
