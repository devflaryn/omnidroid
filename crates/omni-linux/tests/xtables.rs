//! The kernel's xtables, as the image's own iptables reads and writes them: a chain made and a
//! rule appended are what the table then lists (netd builds its firewall so at boot).
mod common;

use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::process::Process;
use omni_linux::{ExitStatus, SpawnConfig};

fn iptables(instance: &std::path::Path, args: &[&str]) -> (ExitStatus, String) {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut argv = vec![b"/system/bin/iptables".to_vec(), b"-w".to_vec()];
    argv.extend(args.iter().map(|a| a.as_bytes().to_vec()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot,
            instance_dir: instance.to_path_buf(),
            argv,
            envp: vec![b"PATH=/system/bin".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&out)),
            trace: false,
        },
        0,
    )
    .expect("spawn");
    let status = p.run();
    let text = String::from_utf8_lossy(&out.lock()).into_owned();
    (status, text)
}

#[test]
fn a_chain_and_its_rule_are_what_the_table_lists() {
    let instance = std::env::temp_dir().join(format!("omni-linux-xtables-{}", std::process::id()));
    let (status, out) = iptables(&instance, &["-N", "omni_test"]);
    assert_eq!(status, ExitStatus::Exited(0), "{out}");
    let (status, out) = iptables(&instance, &["-A", "omni_test", "-j", "RETURN"]);
    assert_eq!(status, ExitStatus::Exited(0), "{out}");
    let (status, out) = iptables(&instance, &["-A", "OUTPUT", "-j", "omni_test"]);
    assert_eq!(status, ExitStatus::Exited(0), "{out}");
    let (status, out) = iptables(&instance, &["-S"]);
    assert_eq!(status, ExitStatus::Exited(0), "{out}");
    for want in ["-P INPUT ACCEPT", "-N omni_test", "-A OUTPUT -j omni_test", "-A omni_test -j RETURN"] {
        assert!(out.contains(want), "{want:?} in:\n{out}");
    }
}
