//! Milestone A1: the real AOSP toybox, through the real linker64 and libc.so, prints "hello".
use std::path::PathBuf;
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::{ExitStatus, Process, SpawnConfig};

fn sysroot() -> Option<PathBuf> {
    let dir = std::env::var_os("OMNI_SYSROOT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35"),
        PathBuf::from,
    );
    dir.join("sysroot.manifest").exists().then_some(dir)
}

fn run(args: &[&str]) -> Option<(ExitStatus, String, String)> {
    let Some(sysroot) = sysroot() else {
        eprintln!("SKIPPED: no sysroot (tools/make_sysroot.py, plan Task 1)");
        return None;
    };
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let instance = std::env::temp_dir().join(format!("omni-linux-a1-{}", std::process::id()));
    let p = Process::spawn(SpawnConfig {
        sysroot,
        instance_dir: instance,
        argv: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec()],
        stdout: Output::Capture(Arc::clone(&out)),
        stderr: Output::Capture(Arc::clone(&err)),
        trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
    })
    .expect("spawn");
    let status = p.run();
    eprintln!("{}", p.report());
    let s = |b: &Arc<parking_lot::Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock()).into_owned();
    Some((status, s(&out), s(&err)))
}

#[test]
fn a1_toybox_echo_hello() {
    let Some((status, out, err)) = run(&["/system/bin/toybox", "echo", "hello"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert_eq!(out, "hello\n");
}

#[test]
fn a_missing_program_is_a_named_error_not_a_crash() {
    let Some(sysroot) = sysroot() else { return };
    let r = Process::spawn(SpawnConfig {
        sysroot,
        instance_dir: std::env::temp_dir().join("omni-linux-a1-missing"),
        argv: vec![b"/system/bin/no-such-program".to_vec()],
        envp: vec![],
        stdout: Output::Capture(Default::default()),
        stderr: Output::Capture(Default::default()),
        trace: false,
    });
    let e = r.err().expect("refused");
    assert!(e.contains("no-such-program"), "{e}");
}
