//! Bundles of APKs read as Android installs them (`omni_warm::bundle`): the fixtures are
//! `tests/fixtures/bundles` (`build.sh` there makes them).
use std::path::{Path, PathBuf};

use omni_warm::bundle::{self, Bundle};
use omni_warm::Apk;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bundles").join(name)
}

const PACKAGE: &str = "com.omnidroid.splittest";

#[test]
fn an_xapk_is_its_base_its_splits_for_arm64_and_its_obb() {
    let path = fixture("splittest.xapk");
    assert!(bundle::is_bundle(&path).expect("a zip"));
    let b = Bundle::open(&path).expect("read");
    assert_eq!(b.packages.len(), 1);
    let p = &b.packages[0];
    assert_eq!(p.package, PACKAGE);
    assert_eq!(p.version_code, Some(7));
    assert_eq!(p.launcher.as_deref(), Some("com.omnidroid.probe.MainActivity"));
    let splits: Vec<&str> = p.splits.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(splits, ["config.arm64_v8a", "config.xxhdpi"], "another ABI's split left out");
    assert_eq!(p.left_out, ["config.x86"]);
    assert_eq!(p.obbs.iter().map(|o| bundle::own_name(o)).collect::<Vec<_>>(), ["main.7.com.omnidroid.splittest.obb"]);
    assert!(p.base.exists() && p.splits.iter().all(|(_, f)| f.exists()));

    let apk = Apk::read(&path).expect("its only package");
    assert_eq!((apk.package.as_str(), apk.splits.len(), apk.obbs.len()), (PACKAGE, 2, 1));
    assert_eq!(apk.bundle.as_deref(), Some(path.as_path()));
    assert_eq!(apk.sha256, omni_warm::sha256_file(&apk.path).expect("hash"), "known by its base APK's bytes");
    assert_eq!(apk.describe(), "com.omnidroid.splittest (2 splits, 1 OBB file)");
}

#[test]
fn apkm_and_apks_read_alike_and_are_found_extracted_again() {
    for name in ["splittest.apkm", "splittest.apks"] {
        let first = Apk::read(&fixture(name)).expect(name);
        assert_eq!((first.package.as_str(), first.splits.len(), first.obbs.len()), (PACKAGE, 2, 0), "{name}");
        let again = Apk::read(&fixture(name)).expect(name);
        assert_eq!(again.path, first.path, "{name}: the same extraction");
    }
}

#[test]
fn a_bundle_of_several_packages_asks_which() {
    let path = fixture("two-apps.zip");
    let err = Apk::read(&path).expect_err("two packages, none named");
    assert!(err.contains("2 packages") && err.contains(PACKAGE) && err.contains("com.omnidroid.probe") && err.contains("package"), "{err}");
    let probe = Apk::read_package(&path, Some("com.omnidroid.probe")).expect("named");
    assert_eq!((probe.package.as_str(), probe.splits.len()), ("com.omnidroid.probe", 0));
    let split = Apk::read_package(&path, Some(PACKAGE)).expect("named");
    assert_eq!(split.splits.len(), 2);
    let err = Apk::read_package(&path, Some("com.example.none")).expect_err("not there");
    assert!(err.contains("no package com.example.none"), "{err}");
}

#[test]
fn an_apk_is_not_a_bundle() {
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("../omni-linux/tests/fixtures/probe-app/probe.apk");
    assert!(!bundle::is_bundle(&probe).expect("a zip"));
    let apk = Apk::read_package(&probe, Some("com.omnidroid.probe")).expect("its own package");
    assert!(apk.splits.is_empty() && apk.bundle.is_none());
    assert!(Apk::read_package(&probe, Some("com.other")).is_err());
}
