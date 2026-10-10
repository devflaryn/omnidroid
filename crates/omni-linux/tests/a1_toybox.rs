//! Milestone A1: the real AOSP toybox, through the real linker64 and libc.so, prints "hello".
mod common;

use common::{run, sysroot};
use omni_linux::fd::Output;
use omni_linux::{ExitStatus, Process, SpawnConfig};

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

/// An image file is as old as Android's build stamps it (2009-01-01), and a file the instance
/// writes is newer: PackageManager's parse cache holds an entry only while the package is older
/// than it (every file read 0 before, so no entry ever held).
#[test]
fn an_image_file_is_older_than_what_the_instance_writes() {
    let Some((status, out, err)) = run(&[
        "/system/bin/sh",
        "-c",
        "stat -c %Y /system/framework/framework.jar; echo x > /data/local/tmp/mtime-probe; stat -c %Y /data/local/tmp/mtime-probe",
    ]) else {
        return;
    };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    let times: Vec<i64> = out.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert_eq!(times.len(), 2, "{out}");
    assert_eq!(times[0], omni_linux::fd::IMAGE_MTIME);
    assert!(times[1] > times[0] && times[1] > 1_700_000_000, "{times:?}");
}
