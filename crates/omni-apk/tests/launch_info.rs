//! The launcher Activity read from real APKs: the probe app (built from source, in the repository)
//! and, when present, the stock Roblox APK (git-ignored; skipped without it).
use std::path::PathBuf;

use omni_apk::launch_info_of;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn the_probe_app_names_its_package_and_launcher() {
    let info = launch_info_of(&repo().join("crates/omni-linux/tests/fixtures/probe-app/probe.apk")).expect("the probe's manifest");
    assert_eq!(info.package, "com.omnidroid.probe");
    assert_eq!(info.launcher.as_deref(), Some("com.omnidroid.probe.MainActivity"));
}

#[test]
fn roblox_launches_through_its_alias() {
    let apk = repo().join("Roblox-2.738.1397.apk");
    if !apk.exists() {
        eprintln!("SKIPPED: no {}", apk.display());
        return;
    }
    let info = launch_info_of(&apk).expect("Roblox's manifest");
    assert_eq!(info.package, "com.roblox.client");
    assert_eq!(info.launcher.as_deref(), Some("com.roblox.client.startup.LauncherAliasMain"));
    assert!(info.version_code.is_some_and(|v| v > 0));
}
