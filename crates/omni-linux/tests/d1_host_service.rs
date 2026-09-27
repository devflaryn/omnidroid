//! Sub-project D1's gate: a binder service implemented on the host. Host Rust registers an "echo"
//! service with the real `servicemanager`, and `/system/bin/service` in a guest process finds it
//! and calls it over `/dev/binder`.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::binder::{broker, Context};
use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

/// `IBinder::INTERFACE_TRANSACTION` (`'_NTF'`): `service call` asks it before calling.
const INTERFACE_TRANSACTION: u32 = 0x5f4e_5446;

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str]) -> (Arc<Process>, Arc<parking_lot::Mutex<Vec<u8>>>, Arc<parking_lot::Mutex<Vec<u8>>>) {
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

/// Run a guest program to its end, at most a minute.
fn run(sysroot: &Path, instance: &Path, argv: &[&str], sm_err: &Arc<parking_lot::Mutex<Vec<u8>>>) -> (ExitStatus, String, String) {
    let (p, out, err) = spawn(sysroot, instance, argv);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(p.run()));
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(status) => (status, text(&out), text(&err)),
        Err(_) => panic!("{argv:?} did not finish\nstdout: {}\nstderr: {}\nservicemanager: {}", text(&out), text(&err), text(sm_err)),
    }
}

/// A parcel's `String16`: its length in UTF-16 units, the units, a NUL, padded to 4 bytes.
fn string16(s: &str) -> Vec<u8> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut b = (units.len() as i32).to_le_bytes().to_vec();
    for u in units.iter().chain([0u16].iter()) {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b.resize((b.len() + 3) & !3, 0);
    b
}

#[test]
fn d1_guest_calls_a_host_service_through_servicemanager() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-d1-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    {
        let (p, _, e) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"]);
        assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&e));
    }

    let (sm, _, sm_err) = spawn(&sysroot, &instance, &["/system/bin/servicemanager"]);
    let server = {
        let sm = Arc::clone(&sm);
        std::thread::spawn(move || sm.run())
    };
    std::thread::sleep(Duration::from_millis(1500));

    // The host's echo service: its interface name when asked, otherwise the request back.
    let binder = broker(Context::Binder);
    let echo = binder.create_host_service(|code, data| if code == INTERFACE_TRANSACTION { string16("omni.IEcho") } else { data.to_vec() });
    binder.add_service("omni.echo", echo).unwrap_or_else(|e| panic!("addService: {e}\nservicemanager: {}", text(&sm_err)));

    let (status, out, err) = run(&sysroot, &instance, &["/system/bin/service", "check", "omni.echo"], &sm_err);
    assert_eq!(status, ExitStatus::Exited(0), "service check: {out}\n{err}");
    assert!(out.contains("found") && !out.contains("not found"), "service check: {out}\n{err}");

    let (status, out, err) = run(&sysroot, &instance, &["/system/bin/service", "call", "omni.echo", "1", "i32", "42"], &sm_err);
    assert_eq!(status, ExitStatus::Exited(0), "service call: {out}\n{err}");
    // The reply is the request: its interface token, then the 42.
    assert!(out.contains("Result: Parcel(") && out.contains("0000002a"), "service call: {out}\n{err}");

    sm.end(ExitStatus::Exited(0));
    let _ = server.join();
}
