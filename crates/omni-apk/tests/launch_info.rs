//! The launcher Activity read from real APKs: the probe app (built from source, in the repository)
//! and, when one is named, any APK (`OMNI_TEST_APK`; skipped without it).
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
    assert_eq!(info.native_lib, None, "a Java app names no native library");
}

#[test]
fn any_apk_names_its_package_and_whatever_launches_it() {
    let Some(apk) = std::env::var_os("OMNI_TEST_APK").map(PathBuf::from) else {
        eprintln!("SKIPPED: OMNI_TEST_APK names no APK");
        return;
    };
    let info = launch_info_of(&apk).expect("the APK's manifest");
    eprintln!("{}: package {}, launcher {:?}, native library {:?}", apk.display(), info.package, info.launcher, info.native_lib);
    assert!(!info.package.is_empty() && info.launcher.is_some(), "an app with a launcher icon");
}
