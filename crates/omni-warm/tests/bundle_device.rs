//! Bundles installed on a running device (ignored: needs one). `OMNI_WARM_DIR=<a warm device's
//! directory> cargo test -p omni-warm --test bundle_device -- --ignored --test-threads 1`: each
//! fixture of `tests/fixtures/bundles` installed through the control channel as the pool installs
//! it, what `pm` holds checked, and its launcher started.
use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_warm::{Apk, Device};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundles").join(name)
}

fn device() -> Device {
    let dir = std::env::var_os("OMNI_WARM_DIR").expect("OMNI_WARM_DIR: a running device's directory");
    let device = Device { dir: PathBuf::from(dir) };
    assert!(device.alive(), "its control channel answers");
    device
}

fn root(device: &Device, command: &str) -> String {
    let (_, out) = device.shell_as(command, Some(0), Duration::from_secs(60)).expect("the control channel");
    out
}

#[test]
#[ignore = "needs a running device (OMNI_WARM_DIR)"]
fn an_xapk_installs_with_its_splits_and_obb_and_starts() {
    let device = device();
    let apk = Apk::read(&fixture("splittest.xapk")).expect("read");
    let _ = root(&device, &format!("pm uninstall {}", apk.package));
    let installed = omni_warm::install_beside(&device, &apk).expect("installed");
    assert_eq!(installed.action, "installed");
    let paths = root(&device, &format!("pm path {}", apk.package));
    let mut held: Vec<&str> = paths.lines().filter_map(|l| l.trim().rsplit('/').next()).collect();
    held.sort_unstable();
    assert_eq!(held, ["base.apk", "split_config.arm64_v8a.apk", "split_config.xxhdpi.apk"], "{paths}");
    let obb = root(&device, &format!("ls -ln /data/media/0/Android/obb/{}", apk.package));
    assert!(obb.contains("main.7.com.omnidroid.splittest.obb") && obb.contains(" 1079 "), "{obb}");
    let started = omni_warm::start(&device, &apk, Duration::from_secs(60)).expect("started");
    assert!(started.component.ends_with("com.omnidroid.probe.MainActivity"), "{started:?}");
    assert_eq!(omni_warm::install_beside(&device, &apk).expect("again").action, "reused");
    let _ = root(&device, &format!("pm uninstall {}", apk.package));
}

#[test]
#[ignore = "needs a running device (OMNI_WARM_DIR)"]
fn a_package_named_from_a_zip_of_two_installs() {
    let device = device();
    let apk = Apk::read_package(&fixture("two-apps.zip"), Some("com.omnidroid.splittest")).expect("named");
    let _ = root(&device, &format!("pm uninstall {}", apk.package));
    omni_warm::install_beside(&device, &apk).expect("installed");
    let paths = root(&device, &format!("pm path {}", apk.package));
    assert_eq!(paths.lines().filter(|l| l.contains(".apk")).count(), 3, "{paths}");
    let _ = root(&device, &format!("pm uninstall {}", apk.package));
}
