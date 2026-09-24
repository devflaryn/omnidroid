//! Which APK a run uses: the one it was told, or the newest one it can find.
//!
//! **The APK is input, not code.** Nothing that runs an app names a version: a launcher passes
//! `--apk`, an environment passes [`APK_ENV`], and with neither the newest APK in the directory is
//! used -- so dropping a newer APK next to the old one is the whole of an update.

use std::path::{Path, PathBuf};

use crate::apk::Apk;
use crate::error::{ApkError, ApkResult};
use crate::manifest::AppManifest;

/// The environment variable that names the APK to run, when no `--apk` was given.
pub const APK_ENV: &str = "OMNI_APK";

/// An APK a run will use, with what it says it is and why it was the one.
#[derive(Debug, Clone)]
pub struct ChosenApk {
    /// Where it is.
    pub path: PathBuf,
    /// Its `<manifest>`: package, version name, version code.
    pub manifest: AppManifest,
    /// How it was chosen, for the line a launcher prints: `"--apk"`, `"OMNI_APK"`, or
    /// `"the newest in <dir>"`.
    pub chosen_by: String,
}

/// Choose the APK: `explicit` (a launcher's `--apk`) if given, else [`APK_ENV`] if set, else the
/// `*.apk` in `search_dir` with the highest `versionCode` -- the number the store orders releases
/// by, so no version *string* has to be compared.
///
/// # Errors
///
/// [`ApkError::Choice`] when a named APK does not open or declare a manifest, or when `search_dir`
/// holds no APK that does -- naming every candidate and why it was passed over.
pub fn choose_apk(explicit: Option<&Path>, search_dir: &Path) -> ApkResult<ChosenApk> {
    if let Some(path) = explicit {
        return named(path, "--apk".to_string());
    }
    if let Some(path) = std::env::var_os(APK_ENV).filter(|value| !value.is_empty()) {
        return named(Path::new(&path), APK_ENV.to_string());
    }
    let entries = std::fs::read_dir(search_dir).map_err(|error| ApkError::Choice {
        detail: format!("could not list {} for an APK: {error}", search_dir.display()),
    })?;
    let mut candidates: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("apk")))
        .collect();
    candidates.sort();
    let mut best: Option<(PathBuf, AppManifest)> = None;
    let mut passed_over = Vec::new();
    for path in candidates {
        match manifest_of(&path) {
            Ok(manifest) => {
                let newer = best.as_ref().is_none_or(|(_, held)| {
                    (manifest.version_code, &manifest.version_name)
                        > (held.version_code, &held.version_name)
                });
                if newer {
                    best = Some((path, manifest));
                }
            }
            Err(error) => passed_over.push(format!("{}: {error}", path.display())),
        }
    }
    match best {
        Some((path, manifest)) => Ok(ChosenApk {
            path,
            manifest,
            chosen_by: format!("the newest in {}", search_dir.display()),
        }),
        None => Err(ApkError::Choice {
            detail: format!(
                "no APK to run: none was given (--apk, or {APK_ENV}), and {} holds no APK that \
                 opens{}",
                search_dir.display(),
                if passed_over.is_empty() {
                    String::new()
                } else {
                    format!(" -- passed over: {}", passed_over.join("; "))
                }
            ),
        }),
    }
}

/// The manifest of the APK at `path`.
///
/// # Errors
///
/// Opening the APK, reading its manifest entry, or decoding it.
pub fn manifest_of(path: &Path) -> ApkResult<AppManifest> {
    AppManifest::parse(&Apk::open(path)?.read_manifest()?)
}

fn named(path: &Path, chosen_by: String) -> ApkResult<ChosenApk> {
    let manifest = manifest_of(path).map_err(|error| ApkError::Choice {
        detail: format!("{chosen_by} names {}, which is not a usable APK: {error}", path.display()),
    })?;
    Ok(ChosenApk { path: path.to_path_buf(), manifest, chosen_by })
}
