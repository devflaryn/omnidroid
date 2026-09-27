//! C5: an installed APK's launcher Activity is started through the framework -- `am start` to
//! ActivityManager, which asks the zygote's socket for a process (answered by launching the
//! image's `app_process64 ... android.app.ActivityThread` in a host process of its own, whose
//! binder is the system's), the app attaches, and the framework's own `bindApplication` and
//! Activity launch run its `onCreate`. The probe APK (tests/fixtures/probe-app) is installed as on
//! a device: pushed to `/data/local/tmp`, then `pm install` once the system has booted.
//!
//! Minutes long: `cargo test -p omni-linux --test c5_app_launch -- --ignored`.
mod common;

use std::path::PathBuf;
use std::time::Duration;

#[test]
#[ignore = "boots the whole system and starts an app: minutes"]
fn the_launcher_activity_of_an_installed_apk_is_created() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-c5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("/data/local/tmp");
    let apk = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/probe-app/probe.apk");
    std::fs::copy(&apk, tmp.join("probe.apk")).expect("the probe APK");

    let then = "i=0; until [ \"$(getprop sys.boot_completed)\" = 1 ] || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                echo \"[c5] boot_completed=$(getprop sys.boot_completed)\"; \
                pm install -r /data/local/tmp/probe.apk; echo \"[c5] pm install: $?\"; \
                am start -W -n com.omnidroid.probe/.MainActivity; echo \"[c5] am start: $?\"";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &["--zygote"], then);
    let (mut launched, mut created, mut resumed) = (false, None, false);
    boot.watch(Duration::from_secs(2400), |line| {
        launched |= line.contains("[zygote] launching com.omnidroid.probe");
        if let Some(at) = line.find("OmniProbe") {
            let said = &line[at..];
            if said.contains("onCreate com.omnidroid.probe") {
                created = Some(said.to_string());
            }
            resumed |= said.contains("onResume");
        }
        created.is_some() && resumed
    });
    let tail = boot.tail();
    assert!(launched, "ActivityManager never asked the zygote for the app's process\n{tail}");
    let created = created.unwrap_or_else(|| panic!("the launcher Activity's onCreate never ran\n{tail}"));
    assert!(resumed, "{created}, but onResume never ran\n{tail}");
}
