//! Binder across host processes (`crate::remote`; the C5 design): the real `servicemanager` runs
//! here, and `/system/bin/service` in **another host process** (`omni-linux-run
//! --binder-server`) asks it over its `/dev/binder` what is registered -- the transaction, its
//! reply, and the reply's delivery into the caller's receive area all cross.
mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

#[test]
fn a_process_in_another_host_process_asks_servicemanager() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-c3r-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let spawn = |argv: &[&str], uid: u32| {
        let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let p = Process::spawn_as(
            SpawnConfig {
                sysroot: sysroot.clone(),
                instance_dir: instance.clone(),
                argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
                envp: vec![b"PATH=/system/bin".to_vec()],
                stdout: Output::Capture(Arc::default()),
                stderr: Output::Capture(Arc::clone(&err)),
                trace: false,
            },
            uid,
        )
        .expect("spawn");
        (p, err)
    };
    let (lc, _) = spawn(&["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], 0);
    assert_eq!(lc.run(), ExitStatus::Exited(0));
    let (sm, sm_err) = spawn(&["/system/bin/servicemanager"], 1000);
    std::thread::spawn(move || sm.run());
    std::thread::sleep(Duration::from_millis(1500));

    let addr = omni_linux::remote::serve(omni_linux::vfs::Sysroot::open(&sysroot).expect("sysroot")).expect("serve");
    let pid = omni_linux::process::reserve_pid();
    let cred = omni_linux::remote::issue_credential(pid, 10_000);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_omni-linux-run"))
        .args(["--sysroot", &sysroot.to_string_lossy(), "--instance", &instance.to_string_lossy()])
        .args(["--binder-server", &addr.to_string(), "--binder-credential-stdin", "--pid", &pid.to_string(), "--uid", "10000", "--", "/system/bin/service", "list"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("omni-linux-run");
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "{}", omni_linux::remote::credential_hex(&cred)).expect("the credential");
    }
    let out = child.wait_with_output().expect("omni-linux-run");
    let listed = String::from_utf8_lossy(&out.stdout);
    let err = "";
    assert!(out.status.success(), "{listed}\n{err}\nservicemanager: {}", String::from_utf8_lossy(&sm_err.lock()));
    assert!(listed.contains("manager"), "{listed}\n{err}");
}
