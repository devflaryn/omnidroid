//! What the milestone gates share: the pinned sysroot and a run of a real AOSP program.
#![allow(dead_code)]
use std::path::PathBuf;
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::{ExitStatus, Process, SpawnConfig};

pub fn sysroot() -> Option<PathBuf> {
    let dir = std::env::var_os("OMNI_SYSROOT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35"),
        PathBuf::from,
    );
    dir.join("sysroot.manifest").exists().then_some(dir)
}

/// An instance directory of its own for each run: tests run in parallel.
fn instance_dir() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("omni-linux-gate-{}-{n}", std::process::id()))
}

pub fn run(args: &[&str]) -> Option<(ExitStatus, String, String)> {
    run_in(instance_dir(), args)
}

/// Run an NDK-built fixture (`tests/fixtures/<name>`) from the instance's `/data/local/tmp`, as
/// `adb push` and a shell would.
pub fn run_fixture(name: &str, args: &[&str]) -> Option<(ExitStatus, String, String)> {
    let instance = instance_dir();
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).expect("the instance's /data/local/tmp");
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name);
    std::fs::copy(&fixture, tmp.join(name)).unwrap_or_else(|e| panic!("{}: {e}", fixture.display()));
    let guest = format!("/data/local/tmp/{name}");
    let mut argv = vec![guest.as_str()];
    argv.extend_from_slice(args);
    run_in(instance, &argv)
}

fn run_in(instance: PathBuf, args: &[&str]) -> Option<(ExitStatus, String, String)> {
    let Some(sysroot) = sysroot() else {
        eprintln!("SKIPPED: no sysroot (tools/make_sysroot.py, plan Task 1)");
        return None;
    };
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
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
