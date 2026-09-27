//! Sub-project B's gate: a `.dex` runs on the real ART. A fresh instance is booted the way init
//! boots a device -- `linkerconfig` writes `/linkerconfig`, `derive_classpath` writes the class
//! paths -- and then `dalvikvm64` loads `Hello.dex` through a `PathClassLoader` over the whole
//! boot class path and its boot image, and runs it.
//!
//! Its own test binary: the guest space sits at a fixed low address (ART's heap must be below
//! 4 GiB), one per host process.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str], env: &[String]) -> (ExitStatus, String, String) {
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let err = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let mut envp = vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()];
    envp.extend(env.iter().map(|e| e.as_bytes().to_vec()));
    let p = Process::spawn(SpawnConfig {
        sysroot: sysroot.to_path_buf(),
        instance_dir: instance.to_path_buf(),
        argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
        envp,
        stdout: Output::Capture(Arc::clone(&out)),
        stderr: Output::Capture(Arc::clone(&err)),
        trace: false,
    })
    .expect("spawn");
    let status = p.run();
    let s = |b: &Arc<parking_lot::Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock()).into_owned();
    (status, s(&out), s(&err))
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "macOS arm64 maps nothing below 4 GiB, where ART's heap must be (C design: Mac)")]
fn b_hello_dex_runs_on_art() {
    let Some(sysroot) = common::sysroot() else { return };
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);

    // What init does before any app process: the linker's namespaces, then the class paths.
    let (status, _, err) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], &[]);
    assert_eq!(status, ExitStatus::Exited(0), "linkerconfig: {err}");
    // init.rc makes the directory derive_classpath writes into.
    std::fs::create_dir_all(instance.join("data/system/environ")).unwrap();
    let (status, _, err) =
        spawn(&sysroot, &instance, &["/apex/com.android.sdkext/bin/derive_classpath", "/data/system/environ/classpath"], &[]);
    assert_eq!(status, ExitStatus::Exited(0), "derive_classpath: {err}");
    let classpath = std::fs::read_to_string(instance.join("data/system/environ/classpath")).expect("the class paths");
    let mut env: Vec<String> = classpath
        .lines()
        .filter_map(|l| l.strip_prefix("export ").and_then(|kv| kv.split_once(' ')).map(|(k, v)| format!("{k}={v}")))
        .collect();
    assert!(env.iter().any(|e| e.starts_with("BOOTCLASSPATH=")), "{classpath}");
    env.extend(
        ["ANDROID_ART_ROOT=/apex/com.android.art", "ANDROID_I18N_ROOT=/apex/com.android.i18n", "ANDROID_TZDATA_ROOT=/apex/com.android.tzdata"]
            .map(String::from),
    );

    // The app's code: a read-only dex, as Android requires of loaded code.
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let dex = tmp.join("hello.dex");
    std::fs::copy(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dex/hello.dex"), &dex).unwrap();
    let mut perms = std::fs::metadata(&dex).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&dex, perms).unwrap();

    // CMC: the boot image is compiled for it. No JIT yet (its code cache wants memfd and W+X).
    let (status, out, err) = spawn(
        &sysroot,
        &instance,
        &["/apex/com.android.art/bin/dalvikvm64", "-Xgc:CMC", "-Xusejit:false", "-cp", "/data/local/tmp/hello.dex", "Hello"],
        &env,
    );
    assert_eq!(out, "Hello from real ART on omnidroid, sum=500500, 2.1.0\n", "stderr:\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "stderr:\n{err}");
}
