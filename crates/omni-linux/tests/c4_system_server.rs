//! C4: system_server, started as the zygote would start it (the image's `app_process64` with
//! `com.android.server.SystemServer`, the class path the image's `derive_classpath` writes), over
//! the boot init brings up below it, publishes the framework's own services -- activity, package,
//! window -- in the real servicemanager, and ActivityManager reaches systemReady. Nothing of the
//! framework is replaced; what it asks of the kernel and the daemons below it is answered.
//!
//! Minutes long: `cargo test -p omni-linux --test c4_system_server -- --ignored`.
mod common;

use std::time::Duration;

#[test]
#[ignore = "boots the whole system: minutes"]
fn system_server_publishes_activity_package_and_window() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = std::env::temp_dir().join(format!("omni-linux-c4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let then = "i=0; until service check window | grep -q 'window: found' || [ $i -ge 240 ]; do sleep 5; i=$((i+1)); done; \
                for s in activity package window; do service check $s; done; echo '[c4] checked'";
    let mut boot = common::boot::Boot::start(&sysroot, instance, &[], then);
    let (mut ready, mut checked, mut found) = (false, false, Vec::new());
    boot.watch(
        Duration::from_secs(1800),
        |line| {
            ready |= line.contains("System now ready");
            checked |= line.contains("[c4] checked");
            if let Some(rest) = line.strip_prefix("Service ") {
                found.push(rest.to_string());
            }
            ready && checked
        },
    );
    let tail = boot.tail();
    for s in ["activity", "package", "window"] {
        assert!(found.iter().any(|f| f == &format!("{s}: found")), "{s} not published: {found:?}\n{tail}");
    }
    assert!(ready, "ActivityManager never reached systemReady\n{tail}");
}
