//! A rooted device boots to a root shell with a module applied; a device with no profile is not rooted.
mod common;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_linux::root::{self, Catalog, MagiskAssets, Profile};

fn fresh_instance(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omni-root-boot-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("instance dir");
    dir
}

/// Stage `instance` as a rooted device with the omni-test module (root=1, module=omni-test).
fn stage_test_root(instance: &Path) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let catalog = Catalog::discover(&data, None).expect("the test catalog");
    let assets = MagiskAssets::find(&repo).expect("Magisk assets: run `python tools/fetch_magisk.py`");
    let mut profile = Profile::parse("root=1\nmodule=omni-test\n");
    profile.magisk_code = assets.version_code;
    root::install::stage(instance, &profile, &catalog, &assets).expect("stage");
}

#[test]
#[ignore = "boots the whole system to a root shell: minutes"]
fn a_rooted_device_grants_root_and_applies_a_module() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = fresh_instance("rooted");
    stage_test_root(&instance);
    // The profile is host-only; the guest sees only the marker.
    assert!(instance.join(".omni-root-profile").is_file());
    assert!(!instance.join("data/adb/omni/profile").exists());
    let then = "\
        for i in $(seq 1 120); do [ -f /data/local/tmp/omni-test-svc ] && break; sleep 1; done; \
        echo RB_UID=$(su -c id -u); \
        echo RB_UID2000=$(su 2000 -c id -u); \
        echo RB_FILE=$(cat /system/etc/omni-test.txt); \
        echo RB_PROP=$(getprop ro.omni.test); \
        su -c 'resetprop ro.build.tags omni-test'; echo RB_RESET=$(getprop ro.build.tags); \
        echo RB_PFD=$(cat /data/local/tmp/omni-test-pfd 2>/dev/null); \
        echo RB_SVC=$(cat /data/local/tmp/omni-test-svc 2>/dev/null); \
        echo RB_VER=$(magisk -v); \
        echo RB_LOG=$(cat /data/adb/omni/install.log 2>&1 | tr '\n' '|'); \
        echo RB_DONE";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let mut seen: HashMap<&str, String> = HashMap::new();
    boot.watch(Duration::from_secs(600), |line| {
        for key in ["RB_UID=", "RB_UID2000=", "RB_FILE=", "RB_PROP=", "RB_RESET=", "RB_PFD=", "RB_SVC=", "RB_VER=", "RB_LOG="] {
            if let Some(v) = line.strip_prefix(key) {
                eprintln!("{line}");
                seen.insert(key, v.trim().to_string());
            }
        }
        line.contains("RB_DONE")
    });
    let tail = boot.tail();
    let get = |k: &str| seen.get(k).cloned();
    assert_eq!(get("RB_UID="), Some("0".into()), "{tail}");
    assert_eq!(get("RB_UID2000="), Some("2000".into()), "{tail}");
    assert_eq!(get("RB_FILE="), Some("omni-test".into()), "{tail}");
    assert_eq!(get("RB_PROP="), Some("1".into()), "{tail}");
    assert_eq!(get("RB_RESET="), Some("omni-test".into()), "{tail}");
    assert_eq!(get("RB_PFD="), Some("ok".into()), "{tail}");
    assert_eq!(get("RB_SVC="), Some("ok".into()), "{tail}");
    assert!(get("RB_VER=").is_some_and(|v| v.contains(":MAGISK:R")), "{tail}");
}

#[test]
#[ignore = "boots the whole system without a profile: minutes"]
fn a_device_without_a_profile_is_not_rooted() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = fresh_instance("plain");
    // A guest-planted marker, with no host-only profile: it must grant nothing.
    std::fs::create_dir_all(instance.join("data/adb/omni")).expect("data/adb/omni");
    std::fs::write(instance.join("data/adb/omni/enabled"), b"").expect("planted marker");
    let then = "echo RB_SU=$(command -v su || echo none); \
                echo RB_DBG=$(ls /debug_ramdisk 2>/dev/null || echo none); \
                echo RB_FILE=$(cat /system/etc/omni-test.txt 2>/dev/null || echo none); echo RB_DONE";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let mut seen: HashMap<&str, String> = HashMap::new();
    boot.watch(Duration::from_secs(600), |line| {
        for key in ["RB_SU=", "RB_DBG=", "RB_FILE="] {
            if let Some(v) = line.strip_prefix(key) {
                eprintln!("{line}");
                seen.insert(key, v.trim().to_string());
            }
        }
        line.contains("RB_DONE")
    });
    let tail = boot.tail();
    assert_eq!(seen.get("RB_SU="), Some(&"none".to_string()), "{tail}");
    assert_eq!(seen.get("RB_DBG="), Some(&"none".to_string()), "{tail}");
    assert_eq!(seen.get("RB_FILE="), Some(&"none".to_string()), "{tail}");
}
