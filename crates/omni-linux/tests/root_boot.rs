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

/// Stage `instance` from `profile_text`, with `user_dir` (when given) as the user module directory.
fn stage_profile(instance: &Path, profile_text: &str, user_dir: Option<&Path>) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let catalog = Catalog::discover(&data, user_dir).expect("the catalog");
    let assets = MagiskAssets::find(&repo).expect("Magisk assets: run `python tools/fetch_magisk.py`");
    let mut profile = Profile::parse(profile_text);
    profile.magisk_code = assets.version_code;
    root::install::stage(instance, &profile, &catalog, &assets).expect("stage");
}

/// A real community Magisk module installs and leaves a working root. Generic on purpose: the
/// module is whatever zip `OMNI_ROOT_SMOKE_MODULE` names (MagiskHide Props Config is the reference
/// one; there `su -c props` prints its menu, which is checked by eye in the log, not here).
///
/// ```text
/// OMNI_ROOT_SMOKE_MODULE=/path/to/module.zip \
///   cargo test -p omni-linux --test root_boot community_module_installs -- --ignored --nocapture
/// ```
/// Skips (returns) when the variable is unset or there is no sysroot.
#[test]
#[ignore = "installs a real community module; needs OMNI_ROOT_SMOKE_MODULE set to a module zip"]
fn community_module_installs_and_root_works() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(zip) = std::env::var_os("OMNI_ROOT_SMOKE_MODULE").map(PathBuf::from) else {
        eprintln!("SKIPPED: OMNI_ROOT_SMOKE_MODULE is not set");
        return;
    };
    assert!(zip.is_file(), "OMNI_ROOT_SMOKE_MODULE is not a file: {}", zip.display());
    let instance = fresh_instance("smoke");
    let user = instance.with_extension("modules");
    let _ = std::fs::remove_dir_all(&user);
    std::fs::create_dir_all(&user).expect("user module dir");
    let copy = user.join(zip.file_name().expect("zip file name"));
    std::fs::copy(&zip, &copy).expect("copy the module zip");
    // The module's id is the catalog's, read from its module.prop: a catalog of the zip alone.
    let alone = Catalog::discover(&user, None).expect("the catalog of the module");
    let id = alone
        .list()
        .first()
        .map(|m| m.prop.id.clone())
        .unwrap_or_else(|| panic!("{} is not a module (no module.prop?)", zip.display()));
    eprintln!("smoke module id: {id}");
    stage_profile(&instance, &format!("root=1\nmodule={id}\n"), Some(&user));

    let then = "\
        for i in $(seq 1 120); do [ -s /data/adb/omni/install.log ] && break; sleep 1; done; sleep 20; \
        echo SM_UID=$(su -c id -u); \
        echo SM_LOG=$(cat /data/adb/omni/install.log 2>&1 | tr '\\n' '|'); \
        echo SM_DONE";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let mut seen: HashMap<&str, String> = HashMap::new();
    boot.watch(Duration::from_secs(600), |line| {
        for key in ["SM_UID=", "SM_LOG="] {
            if let Some(v) = line.strip_prefix(key) {
                eprintln!("{line}");
                seen.insert(key, v.trim().to_string());
            }
        }
        line.contains("SM_DONE")
    });
    let tail = boot.tail();
    let _ = std::fs::remove_dir_all(&user);
    assert_eq!(seen.get("SM_UID=").map(String::as_str), Some("0"), "{tail}");
    let log = seen.get("SM_LOG=").unwrap_or_else(|| panic!("no install.log\n{tail}"));
    for entry in log.split('|') {
        let e = entry.trim_start();
        assert!(!e.starts_with("! ") && !e.to_lowercase().contains("abort"), "install.log has an error line {entry:?}: {log}\n{tail}");
    }
}

/// Build `su-probe.apk` (tests/fixtures/su-probe-app/build.sh) into the temp dir: `None` (skip)
/// when the Android SDK's build-tools 36.0.0 / android-36, a JDK or bash is missing.
fn su_probe_apk() -> Option<PathBuf> {
    // Both su-probe tests run in parallel: one build.
    static BUILT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    BUILT.get_or_init(build_su_probe).clone()
}

fn build_su_probe() -> Option<PathBuf> {
    let sdk = std::env::var_os("ANDROID_SDK")
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|l| Path::new(&l).join("Android/Sdk").into_os_string()))
        .map(PathBuf::from)?;
    if !sdk.join("build-tools/36.0.0").is_dir() || !sdk.join("platforms/android-36/android.jar").is_file() {
        eprintln!("SKIPPED: no Android SDK build-tools 36.0.0 / android-36 under {}", sdk.display());
        return None;
    }
    // Git Bash on Windows (`bash` on PATH may be WSL's, which cannot run the SDK's .exe tools).
    let bash = ["C:/Program Files/Git/bin/bash.exe", "C:/Program Files (x86)/Git/bin/bash.exe"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("bash"));
    let tool = |t: &std::ffi::OsStr| std::process::Command::new(t).arg("--version").output().is_ok();
    if !tool(std::ffi::OsStr::new("javac")) || !tool(bash.as_os_str()) {
        eprintln!("SKIPPED: javac or bash is not on PATH");
        return None;
    }
    let out = std::env::temp_dir().join(format!("su-probe-{}.apk", std::process::id()));
    // Run from the fixture's directory with forward slashes: bash does not take a mixed Windows path.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/su-probe-app");
    let built = std::process::Command::new(&bash)
        .env("ANDROID_SDK", sdk.to_string_lossy().replace('\\', "/"))
        .current_dir(&dir)
        .arg("build.sh")
        .arg(out.to_string_lossy().replace('\\', "/"))
        .output()
        .expect("run build.sh");
    assert!(built.status.success() && out.is_file(), "su-probe build failed:\n{}", String::from_utf8_lossy(&built.stderr));
    Some(out)
}

/// Boot a device staged from `profile_text`, install and launch su-probe, and return the line the
/// probe logged under `omni-su-probe` that is its verdict (`uid=<n>` or `refused: ...`).
fn run_su_probe(tag: &str, profile_text: &str, apk: &Path, sysroot: &Path) -> String {
    let instance = fresh_instance(tag);
    stage_profile(&instance, profile_text, None);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    std::fs::copy(apk, tmp.join("su-probe.apk")).expect("the su-probe APK");
    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                pm install -r /data/local/tmp/su-probe.apk; echo \"[su-probe] pm install: $?\"; \
                am start -W -n com.omnidroid.suprobe/.MainActivity; echo \"[su-probe] am start: $?\"";
    let mut boot = common::boot::Boot::start(sysroot, instance, &["--zygote"], then);
    let mut verdict = None;
    boot.watch(Duration::from_secs(2400), |line| {
        if let Some(at) = line.find("omni-su-probe") {
            // logcat's `I/omni-su-probe(<pid>): <message>`
            let said = line[at..].split_once("): ").map_or("", |(_, m)| m).trim();
            if said.starts_with("uid=") || said.starts_with("refused") {
                verdict = Some(said.to_string());
            }
        }
        verdict.is_some()
    });
    let tail = boot.tail();
    verdict.unwrap_or_else(|| panic!("su-probe never logged a verdict\n{tail}"))
}

/// An app (uid 10xxx) runs `su -c id -u` and is told 0 when the profile lets apps use su.
///
/// `cargo test -p omni-linux --test root_boot su_probe -- --ignored --nocapture`
#[test]
#[ignore = "builds su-probe.apk (Android SDK) and boots the whole system with an app: minutes"]
fn su_probe_app_gets_root_when_su_allows_it() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(apk) = su_probe_apk() else { return };
    let verdict = run_su_probe("suprobe-all", "root=1\nsu=all\n", &apk, &sysroot);
    assert_eq!(verdict, "uid=0", "the app's su -c id -u");
}

/// With `su=com.other.pkg` (not the probe) the probe is refused: no uid 0.
#[test]
#[ignore = "builds su-probe.apk (Android SDK) and boots the whole system with an app: minutes"]
fn su_probe_app_is_refused_under_a_restrictive_policy() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(apk) = su_probe_apk() else { return };
    let verdict = run_su_probe("suprobe-other", "root=1\nsu=com.other.pkg\n", &apk, &sysroot);
    assert!(verdict.starts_with("refused"), "the app must be refused su, got {verdict:?}");
}
