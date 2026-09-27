//! Sub-project D3a's gate: the guest has a GPU. Vulkan through the real Android loader and
//! omnidroid's driver (`/vendor/lib64/hw/vulkan.omni.so`) runs on the host's GPU; GLES through the
//! image's own ANGLE runs on that Vulkan.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

type Buf = Arc<parking_lot::Mutex<Vec<u8>>>;

fn text(b: &Buf) -> String {
    String::from_utf8_lossy(&b.lock()).into_owned()
}

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str], uid: u32) -> (Arc<Process>, Buf, Buf) {
    let out = Buf::default();
    let err = Buf::default();
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.to_path_buf(),
            instance_dir: instance.to_path_buf(),
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&err)),
            trace: std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
        },
        uid,
    )
    .expect("spawn");
    (p, out, err)
}

/// An instance with its linker configuration, and a fixture copied to its `/data/local/tmp`.
fn prepare(name: &str, fixture: &str) -> (PathBuf, PathBuf) {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py): the gate cannot run");
    let instance = std::env::temp_dir().join(format!("omni-linux-d3a-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let (p, _, e) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], 0);
    assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&e));
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture);
    std::fs::copy(&src, tmp.join(fixture)).unwrap_or_else(|e| panic!("{}: {e}", src.display()));
    (sysroot, instance)
}

/// Run `/data/local/tmp/<fixture>` to its end (at most 3 minutes): its status, stdout, stderr.
fn run_fixture(sysroot: &Path, instance: &Path, fixture: &str) -> (ExitStatus, String, String) {
    let guest = format!("/data/local/tmp/{fixture}");
    let (p, out, err) = spawn(sysroot, instance, &[guest.as_str()], 10_000);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(p.run()));
    match rx.recv_timeout(Duration::from_secs(180)) {
        Ok(status) => (status, text(&out), text(&err)),
        Err(_) => panic!("{fixture} did not finish\nstdout: {}\nstderr: {}", text(&out), text(&err)),
    }
}

#[test]
fn vulkan_clears_an_image_on_the_host_gpu() {
    let (sysroot, instance) = prepare("vk", "vkclear");
    let (status, out, err) = run_fixture(&sysroot, &instance, "vkclear");
    assert_eq!(status, ExitStatus::Exited(0), "vkclear\nstdout: {out}\nstderr: {err}");
    let line = out.lines().find(|l| l.starts_with("vulkan ok ")).unwrap_or_else(|| panic!("stdout: {out}\nstderr: {err}"));
    eprintln!("{line}");
}
