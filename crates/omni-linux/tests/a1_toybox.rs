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
