//! Sub-project C1's gate: binder between two guest processes. The real `servicemanager` becomes
//! the context manager, and `/system/bin/service` in another process asks it over `/dev/binder`
//! what is registered and whether `manager` is there.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str]) -> (Arc<Process>, Arc<parking_lot::Mutex<Vec<u8>>>, Arc<parking_lot::Mutex<Vec<u8>>>) {
    // servicemanager runs as `system`, as init.rc starts it; the rest as an app.
    let uid = if argv[0].ends_with("servicemanager") { 1000 } else { 10000 };
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn_as(SpawnConfig {
        sysroot: sysroot.to_path_buf(),
        instance_dir: instance.to_path_buf(),
        argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
        envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()],
        stdout: Output::Capture(Arc::clone(&out)),
        stderr: Output::Capture(Arc::clone(&err)),
        trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
    }, uid)
    .expect("spawn");
    (p, out, err)
}

fn text(b: &Arc<parking_lot::Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&b.lock()).into_owned()
}

#[test]
fn c1_service_asks_servicemanager_over_binder() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-c1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let (_, _, err) = {
        let (p, o, e) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"]);
        assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&e));
        (p, o, e)
    };
    let _ = err;

    let (sm, _, sm_err) = spawn(&sysroot, &instance, &["/system/bin/servicemanager"]);
    let server = {
        let sm = Arc::clone(&sm);
        std::thread::spawn(move || sm.run())
    };
    // Give it a moment to become the context manager.
    std::thread::sleep(Duration::from_millis(1500));

    let (list, out, err) = spawn(&sysroot, &instance, &["/system/bin/service", "list"]);
    let status = {
        let (tx, rx) = std::sync::mpsc::channel();
        let list = Arc::clone(&list);
        std::thread::spawn(move || tx.send(list.run()));
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(s) => s,
            Err(_) => panic!(
                "service list did not finish
stdout: {}
stderr: {}
servicemanager: {}",
                text(&out),
                text(&err),
                text(&sm_err)
            ),
        }
    };
    let listed = text(&out);
    assert_eq!(status, ExitStatus::Exited(0), "service list: {listed}\n{}\nservicemanager: {}", text(&err), text(&sm_err));
    assert!(listed.contains("manager"), "service list: {listed}\n{}\nservicemanager: {}", text(&err), text(&sm_err));

    let (check, out, err) = spawn(&sysroot, &instance, &["/system/bin/service", "check", "manager"]);
    assert_eq!(check.run(), ExitStatus::Exited(0), "{}", text(&err));
    assert!(text(&out).contains("found"), "service check: {}", text(&out));

    sm.end(ExitStatus::Exited(0));
    let _ = server.join();
}
