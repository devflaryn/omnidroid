//! **A plugin's options on an app launch**: `run <apk> --cookie farm4 --place 8737899170` (at the
//! prompt, as `omnidroid <apk> ...`, or the MCP server's `start_instance` `options`) does on the
//! pool's instance what `omnidroid aosp` does with them on a session of its own. The options are
//! the plugins' `aosp` ones, and the plugins' `session-start` hooks answer as they do for `aosp`;
//! what they name (`omni_linux::app_session`'s variables) becomes the launch's
//! [`omni_pool::Extras`]:
//!
//! * `OMNI_R_COOKIE` (a cookie file): a WebView cookie store made from it on the host
//!   (`tools/plant_cookie.py --new`), planted for each instance's user before its start;
//! * `OMNI_R_LINK` with `OMNI_R_LINK_ID`: the link with its id, opened on the instance's display
//!   once `OMNI_R_LINK_READY` or `OMNI_R_SIGNED_IN` is logged, again until `OMNI_R_LINK_TAKEN` is.
//!
//! A restart of the instance asks the plugins again ([`Extras::remake`]): the cookie store is gone
//! once the first start has taken its copy, so the restarted app signs in again from a new one, and
//! follows the link again.
//!
//! The device's language (`OMNI_R_LOCALE`) is not applied: a pool device has booted already.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use omni_pool::extras::TempFile;
use omni_pool::Extras;

use crate::plugin;

/// A launch's options split from the words after its APK: the count (`x3`, `3`) and the rest.
#[must_use]
pub fn split_count(words: &[String]) -> (Option<String>, Vec<String>) {
    match words.first() {
        Some(w) if !w.starts_with("--") => (Some(w.clone()), words[1..].to_vec()),
        _ => (None, words.to_vec()),
    }
}

/// `--package <name>` taken out of a launch's options: which package of a bundle of several to
/// launch (`omni_warm::bundle`). The launcher's own option, not a plugin's.
///
/// # Errors
/// `--package` with no name after it.
pub fn take_package(options: &mut Vec<String>) -> Result<Option<String>, String> {
    let Some(at) = options.iter().position(|o| o == "--package") else { return Ok(None) };
    if at + 1 >= options.len() || options[at + 1].starts_with("--") {
        return Err("--package needs a package's name".into());
    }
    let name = options.remove(at + 1);
    options.remove(at);
    Ok(Some(name))
}

/// Launch `count` instances of `apk` (an APK or a bundle of APKs) with `options`: the launcher's
/// `--package`, and the installed plugins' options ([`extras`]).
///
/// # Errors
/// As [`extras`] and `Pool::launch_package`.
pub fn launch(pool: &Arc<omni_pool::Pool>, apk: &Path, count: u32, options: &[String]) -> Result<Vec<omni_pool::InstanceId>, String> {
    let mut options = options.to_vec();
    let package = take_package(&mut options)?;
    let extras = extras(apk, package.as_deref(), &options)?;
    pool.launch_package(apk, package.as_deref(), count, extras)
}

/// What the installed plugins make of `options` for a launch of `apk` (of `package`, from a bundle
/// of several). Nothing to do (and no plugin run) without options.
///
/// # Errors
/// An option no installed plugin adds, a plugin's refusal (a cookie not found, an APK it is not
/// for), or the cookie store could not be made.
pub fn extras(apk: &Path, package: Option<&str>, options: &[String]) -> Result<Extras, String> {
    if options.is_empty() {
        return Ok(Extras::default());
    }
    let registry = plugin::Registry::installed();
    let plugins = registry.active()?;
    let (rest, given) = plugin::take_args(&plugins, "aosp", options.to_vec())?;
    if let Some(unknown) = rest.first() {
        return Err(crate::with_hint(format!("unknown option `{unknown}` (a plugin's: `plugins` lists them)")));
    }
    let app = omni_warm::Apk::read_package(apk, package)?;
    let info = plugin::SessionInfo { command: "aosp", fresh: false, given_data_dir: None, apk: Some(apk), package: Some(&app.package) };
    let session = plugin::start(&plugins, &given, &info, &crate::plugin_base())?;
    let var = |key: &str| session.var(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let cookie_store = match var("OMNI_R_COOKIE") {
        Some(cookie) => Some(Arc::new(cookie_store(Path::new(&cookie))?)),
        None => None,
    };
    let link = match (var("OMNI_R_LINK"), var("OMNI_R_LINK_ID")) {
        (Some(link), Some(id)) => Some(link.replace("{id}", &id)),
        _ => None,
    };
    // The same APK and options again, for a restart: the plugin's session-start answers anew.
    let (apk, package, options) = (apk.to_path_buf(), package.map(str::to_string), options.to_vec());
    Ok(Extras {
        cookie_store,
        link,
        link_ready: var("OMNI_R_LINK_READY"),
        signed_in: var("OMNI_R_SIGNED_IN"),
        link_taken: var("OMNI_R_LINK_TAKEN"),
        said: session.said,
        remake: Some(Arc::new(move || extras(&apk, package.as_deref(), &options))),
    })
}

/// A WebView cookie store holding the cookie in the file `cookie`, made in the host's temporary
/// directory; it holds an account's session, so it is removed when the launch is done with it.
fn cookie_store(cookie: &Path) -> Result<TempFile, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let at: PathBuf = std::env::temp_dir().join(format!("omni-pool-cookies-{}-{}.db", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_file(&at);
    let store = TempFile::new(at);
    let python = if cfg!(windows) { "python" } else { "python3" };
    let out = Command::new(python)
        .arg(crate::repo_root().join("tools/plant_cookie.py"))
        .arg("--new")
        .arg(store.path())
        .arg(cookie)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("the cookie store: {python}: {e}"))?;
    if !out.status.success() {
        return Err(format!("the cookie store: {}{}", String::from_utf8_lossy(&out.stdout).trim(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_comes_before_the_options() {
        let w = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(split_count(&w(&["x3", "--place", "1"])), (Some("x3".into()), w(&["--place", "1"])));
        assert_eq!(split_count(&w(&["--cookie", "a"])), (None, w(&["--cookie", "a"])));
        assert_eq!(split_count(&[]), (None, vec![]));
    }

    #[test]
    fn no_options_run_no_plugin() {
        let e = extras(Path::new("/nowhere.apk"), None, &[]).expect("nothing to do");
        assert!(!e.any());
    }

    #[test]
    fn package_is_the_launchers_own_option() {
        let w = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let mut o = w(&["--cookie", "a", "--package", "com.a", "--place", "1"]);
        assert_eq!(take_package(&mut o), Ok(Some("com.a".to_string())));
        assert_eq!(o, w(&["--cookie", "a", "--place", "1"]), "the plugins' options left");
        let mut none = w(&["--place", "1"]);
        assert_eq!(take_package(&mut none), Ok(None));
        assert!(take_package(&mut w(&["--package"])).is_err() && take_package(&mut w(&["--package", "--place"])).is_err());
    }
}
