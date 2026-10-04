//! DenyList hiding: a process of a denylisted app gets no root layer (no `su`) and `omni_root`
//! answers `ENOSYS` (even to a planted `su`); any other process on the same device keeps root.
//!
//! Not a whole-system boot: `Process::spawn_as` decides the view from the program's argv
//! (`--nice-name=<package>`, as a zygote-launched app host process carries it), so a guest shell is
//! started as an app uid with that argument -- the real code path, a real staged rooted device, no
//! Android boot. (`sh -c <script> --nice-name=...`: the extra argument is `$0`.)
mod common;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_linux::fd::Output;
use omni_linux::root::{self, Catalog, MagiskAssets, Profile};
use omni_linux::vfs::Node;
use omni_linux::{Process, SpawnConfig};

fn fresh_instance(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omni-hiding-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("instance dir");
    dir
}

/// Stage `instance` as a rooted device (su for every app) hiding root from `com.denytest`.
fn stage_denylisted(instance: &Path) -> Option<()> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let catalog = Catalog::discover(&data, None).expect("the test catalog");
    let Ok(assets) = MagiskAssets::find(&repo) else {
        eprintln!("SKIPPED: no Magisk assets (python tools/fetch_magisk.py)");
        return None;
    };
    let mut profile = Profile::parse("root=1\nsu=all\ndenylist=com.denytest\n");
    profile.magisk_code = assets.version_code;
    root::install::stage(instance, &profile, &catalog, &assets).expect("stage");
    Some(())
}

/// Run `script` in a shell as app uid 10234 whose argv carries `--nice-name=<nice>`: the process
/// and its captured output.
fn run_as_app(sysroot: &Path, instance: &Path, nice: &str, script: &str) -> (Arc<Process>, String) {
    run_as(sysroot, instance, nice, script, 10234)
}

fn run_as(sysroot: &Path, instance: &Path, nice: &str, script: &str, uid: u32) -> (Arc<Process>, String) {
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let argv = ["/system/bin/sh", "-c", script, &format!("--nice-name={nice}")];
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.to_path_buf(),
            instance_dir: instance.to_path_buf(),
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&out)),
            trace: false,
        },
        uid,
    )
    .expect("spawn");
    let _ = p.run();
    let text = String::from_utf8_lossy(&out.lock()).into_owned();
    (p, text)
}

#[test]
fn a_denylisted_process_sees_no_root_and_others_keep_it() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = fresh_instance("deny");
    if stage_denylisted(&instance).is_none() {
        return;
    }
    // A planted su: the shell (uid 2000, never hidden from) copies the real tool where a hidden
    // app could run it from.
    std::fs::create_dir_all(instance.join("data/local/tmp")).expect("/data/local/tmp");
    let (_, plant) = run_as(&sysroot, &instance, "com.shell", "cp /system/bin/su /data/local/tmp/su && chmod 755 /data/local/tmp/su; echo PLANT=$?", 2000);
    assert!(plant.contains("PLANT=0"), "plant su:
{plant}");
    let script = "echo SU=$(command -v su || echo none); echo PLANTED=$(/data/local/tmp/su -c id -u 2>&1 | tail -n 1); echo DONE";

    let (hidden, text) = run_as_app(&sysroot, &instance, "com.denytest:gl", script);
    eprintln!("hidden:\n{text}");
    assert!(hidden.view.hidden, "com.denytest:gl is on the DenyList");
    assert!(text.contains("SU=none"), "a denylisted process sees no su:\n{text}");
    assert!(!text.contains("PLANTED=0"), "a planted su gets nothing from omni_root (ENOSYS):\n{text}");

    let (open, text) = run_as_app(&sysroot, &instance, "com.other", script);
    eprintln!("other:\n{text}");
    assert!(!open.view.hidden);
    assert!(text.contains("SU=/system/bin/su"), "another app still sees su:\n{text}");
    assert!(text.contains("PLANTED=0"), "another app still gets root:\n{text}");
}

#[test]
fn a_denylisted_process_has_no_root_layer() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = fresh_instance("layer");
    if stage_denylisted(&instance).is_none() {
        return;
    }
    let (hidden, _) = run_as_app(&sysroot, &instance, "com.denytest", "true");
    assert!(hidden.view.hidden);
    assert!(matches!(hidden.vfs.resolve(b"/", b"/system/bin/su", true).unwrap().node, Node::Missing { .. }), "su must not resolve for a hidden process");
    let (open, _) = run_as_app(&sysroot, &instance, "com.other", "true");
    assert!(!open.view.hidden);
    assert!(matches!(open.vfs.resolve(b"/", b"/system/bin/su", true).unwrap().node, Node::HostFile { .. }));
}

#[test]
fn a_denylisted_process_maps_mounts_and_props_show_no_root() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance = fresh_instance("maps");
    if stage_denylisted(&instance).is_none() {
        return;
    }
    let script = "cat /proc/self/maps; cat /proc/self/mounts; getprop; echo DONE";
    let leaks = |t: &str| {
        t.lines().any(|l| {
            l.contains("/data/adb") || l.contains("/debug_ramdisk") || l.ends_with("/su") || l.to_ascii_lowercase().contains("magisk")
        })
    };
    let (hidden, text) = run_as_app(&sysroot, &instance, "com.denytest", script);
    assert!(hidden.view.hidden);
    assert!(text.contains("DONE"), "{text}");
    assert!(!leaks(&text), "a denylisted process reveals root in maps/mounts/props:\n{text}");

    let (open, text) = run_as_app(&sysroot, &instance, "com.other", script);
    assert!(!open.view.hidden);
    assert!(text.contains("DONE"), "{text}");
}
