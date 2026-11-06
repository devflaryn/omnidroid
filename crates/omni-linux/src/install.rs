//! Where omnidroid runs from: a checkout it was built in, or an **install** -- a plain folder of
//! prebuilt programs and the sysroot, with no cargo, no Python and no repository beside it (the
//! bundle `tools/make_bundle.py` lays out, which the Omni Executor desktop app ships and runs
//! `omnidroid serve` from).
//!
//! ```text
//! <home>/omnidroid(.exe)            the launcher and its pool
//! <home>/window/Omnidroid(.exe)    the device runner (`--sysroot ... --instance ...`): named so
//!                                   because the menu bar, the Dock and Task Manager show its file
//!                                   name; a folder of its own, as macOS's disks ignore case
//! <home>/omni-r-app(.exe)           the prebuilt `r_app` gate: what boots a device
//! <home>/omni-plant-cookie(.exe)    `tools/plant_cookie.py`, ported (crate `omni-cookie`)
//! <home>/sysroot/aosp-35/...        the pinned sysroot (`vfs::SYSROOT_MANIFEST_SHA256`)
//! <home>/plugins/<name>/...         the plugins it comes with
//! ```
//!
//! **What says it is an install.** `OMNI_HOME`, when set, names it outright. Else the directory of
//! the running program -- or that directory's parent, for a helper kept one level down -- is the
//! install when it holds `sysroot/aosp-35/sysroot.manifest`. A checkout never does (its programs
//! are under `target/<profile>/`, two or three levels below the repository's `sysroot/`), so a
//! session run from the repository with cargo is exactly what it was before: [`home`] is `None`,
//! [`repo_root`] the checkout this crate was compiled in, and every tool is found there.
//!
//! What runs differently in an install is said where it runs: the device's boot (the prebuilt gate,
//! not `cargo test`, in `omnidroid`'s `aosp`), the runner the gate spawns (`OMNI_LINUX_RUN`, read by
//! the gate's `common::boot`) and the cookie store ([`plant_cookie`]).
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The variable that names the install outright (an installer, a test, a program kept elsewhere).
pub const HOME_ENV: &str = "OMNI_HOME";

/// The sysroot under an install (and under a checkout: the same relative path).
pub const SYSROOT: &str = "sysroot/aosp-35";

/// The prebuilt `r_app` gate in an install: the program `omnidroid aosp` runs to boot a device.
pub const R_APP: &str = "omni-r-app";

/// The device runner in an install: `omni-linux-run`, under the name people see it by.
pub const RUNNER: &str = "window/Omnidroid";

/// The ported cookie planter, beside the launcher in an install (`crates/omni-cookie`).
pub const PLANT_COOKIE: &str = "omni-plant-cookie";

/// `name` as an executable's file name on this host: `.exe` on Windows, bare elsewhere.
#[must_use]
pub fn exe_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Whether `dir` is an install: it holds the sysroot's manifest.
#[must_use]
pub fn is_install(dir: &Path) -> bool {
    dir.join(SYSROOT).join("sysroot.manifest").is_file()
}

/// The directories a program's own helpers are looked for in: the running program's directory,
/// then its parent (a helper kept one level down, as a test binary is under `target/<profile>/deps`).
fn exe_dirs() -> Vec<PathBuf> {
    let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) else { return Vec::new() };
    let parent = dir.parent().map(Path::to_path_buf);
    std::iter::once(dir).chain(parent).collect()
}

/// The install this process runs from, if it runs from one: `OMNI_HOME`, else the running
/// program's directory or its parent when it holds the sysroot. Looked at once a process.
#[must_use]
pub fn home() -> Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(|| {
        if let Some(dir) = std::env::var_os(HOME_ENV).filter(|d| !d.is_empty()) {
            return Some(std::path::absolute(&dir).unwrap_or_else(|_| PathBuf::from(dir)));
        }
        exe_dirs().into_iter().find(|d| is_install(d))
    })
    .clone()
}

/// The checkout this crate was compiled in (`crates/omni-linux`'s two ancestors). Only ever a
/// checkout's answer: an install has none, and asks [`repo_root`] instead.
#[must_use]
pub fn built_in() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/omni-linux has two ancestors")
        .to_path_buf()
}

/// Where the launcher's files are -- the plugins, the root modules, the Magisk assets, `tools/`:
/// the install when it runs from one ([`home`]), else the checkout it was built in.
#[must_use]
pub fn repo_root() -> PathBuf {
    home().unwrap_or_else(built_in)
}

/// The pinned sysroot's directory under [`repo_root`].
#[must_use]
pub fn sysroot() -> PathBuf {
    repo_root().join(SYSROOT)
}

/// The program `name` (bare: `.exe` is added on Windows) beside the running program or one level
/// up, if it is there.
#[must_use]
pub fn sibling(name: &str) -> Option<PathBuf> {
    let file = exe_name(name);
    exe_dirs().into_iter().map(|d| d.join(&file)).find(|p| p.is_file())
}

/// The program that makes or changes a WebView cookie store, ready for its own arguments
/// (`[--new] <Cookies database> <cookie file>`): the ported `omni-plant-cookie` beside the running
/// program (an install always has it; a checkout once it is built), else -- a checkout that has not
/// built it -- `tools/plant_cookie.py` under Python, as before. Either says what it did on stdout
/// and why it failed on stderr, and neither ever prints the cookie's value.
#[must_use]
pub fn plant_cookie() -> Command {
    if let Some(bin) = sibling(PLANT_COOKIE) {
        return Command::new(bin);
    }
    // Windows installs `python`; macOS and most Linux hosts have no `python`, only `python3`
    // (Ubuntu ships no `python` at all; MEASURED on the M1). On Windows, `python3` can be the
    // Store's stub, so prefer `python` there.
    let mut python = Command::new(if cfg!(windows) { "python" } else { "python3" });
    python.arg(repo_root().join("tools").join("plant_cookie.py"));
    python
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory is an install by its sysroot's manifest, and nothing else: the checkout this
    /// crate is built in (whose `target/` the test binary runs from) is not one beside the binary.
    #[test]
    fn an_install_is_known_by_its_sysroot_manifest() {
        let dir = std::env::temp_dir().join(format!("omni-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SYSROOT)).unwrap();
        assert!(!is_install(&dir), "a sysroot directory with no manifest is not one");
        std::fs::write(dir.join(SYSROOT).join("sysroot.manifest"), b"").unwrap();
        assert!(is_install(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        if std::env::var_os(HOME_ENV).is_none() {
            assert_eq!(home(), None, "a test binary under target/ runs from no install");
            assert_eq!(repo_root(), built_in());
        }
    }

    #[test]
    fn an_executable_is_named_as_the_host_names_it() {
        assert_eq!(exe_name("omni-r-app"), if cfg!(windows) { "omni-r-app.exe" } else { "omni-r-app" });
    }
}
