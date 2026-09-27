//! Sub-project C2's gate: an APK's code, loaded as an app process loads it. `app_process64` (the
//! framework's JNI registered, a binder thread pool, `servicemanager` beside it) runs `ApkLoad`,
//! which puts every `classes*.dex` of the APK into one `PathClassLoader`, links every class, and
//! loads every native library through that loader -- each in the loader's own linker namespace,
//! each `JNI_OnLoad` run.
//!
//! The APK is the owner's: set `OMNI_TEST_APK` to any APK to run this; without it the test is
//! skipped. Nothing about the APK is written here: its dex files
//! and libraries are found in it.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

type Buf = Arc<parking_lot::Mutex<Vec<u8>>>;

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str], env: &[String], uid: u32) -> (Arc<Process>, Buf, Buf) {
    let out: Buf = Arc::default();
    let err: Buf = Arc::default();
    let mut envp = vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()];
    envp.extend(env.iter().map(|e| e.as_bytes().to_vec()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.to_path_buf(),
            instance_dir: instance.to_path_buf(),
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp,
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::clone(&err)),
            trace: false,
        },
        uid,
    )
    .expect("spawn");
    (p, out, err)
}

fn text(b: &Buf) -> String {
    String::from_utf8_lossy(&b.lock()).into_owned()
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "macOS arm64 maps nothing below 4 GiB, where ART's heap must be (C design: Mac)")]
fn c2_an_apk_loads_in_an_app_process() {
    let Some(sysroot) = common::sysroot() else { return };
    let Some(apk) = std::env::var_os("OMNI_TEST_APK").map(PathBuf::from) else {
        eprintln!("SKIPPED: set OMNI_TEST_APK to an APK");
        return;
    };
    let instance = std::env::temp_dir().join(format!("omni-linux-c2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);

    // Boot: linker namespaces and class paths, as init does.
    // Each ends (and gives back its address space) before the next: the app process needs the
    // low 4 GiB.
    for argv in [
        &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"][..],
        &["/apex/com.android.sdkext/bin/derive_classpath", "/data/system/environ/classpath"][..],
    ] {
        std::fs::create_dir_all(instance.join("data/system/environ")).unwrap();
        let (p, _, e) = spawn(&sysroot, &instance, argv, &[], 0);
        assert_eq!(p.run(), ExitStatus::Exited(0), "{}: {}", argv[0], text(&e));
    }
    let classpath = std::fs::read_to_string(instance.join("data/system/environ/classpath")).unwrap();
    let mut env: Vec<String> = classpath
        .lines()
        .filter_map(|l| l.strip_prefix("export ").and_then(|kv| kv.split_once(' ')).map(|(k, v)| format!("{k}={v}")))
        .collect();
    env.extend(
        ["ANDROID_ART_ROOT=/apex/com.android.art", "ANDROID_I18N_ROOT=/apex/com.android.i18n", "ANDROID_TZDATA_ROOT=/apex/com.android.tzdata"]
            .map(String::from),
    );

    // Install: the APK read-only, its arm64 libraries extracted, as the package manager does.
    let app = instance.join("data/app/com.roblox.client");
    let lib = app.join("lib/arm64");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::copy(&apk, app.join("base.apk")).unwrap();
    let mut libs = Vec::new();
    {
        let mut zip = zip_entries(&apk);
        for (name, bytes) in zip.drain(..) {
            if let Some(file) = name.strip_prefix("lib/arm64-v8a/") {
                std::fs::write(lib.join(file), bytes).unwrap();
                libs.push(file.trim_start_matches("lib").trim_end_matches(".so").to_string());
            }
        }
    }
    for f in [app.join("base.apk")] {
        let mut perms = std::fs::metadata(&f).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&f, perms).unwrap();
    }
    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let harness = tmp.join("apkload.dex");
    std::fs::copy(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dex/apkload.dex"), &harness).unwrap();
    let mut perms = std::fs::metadata(&harness).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&harness, perms).unwrap();

    // The app process (first: its address space goes below 4 GiB), then servicemanager.
    let mut argv = vec![
        "/system/bin/app_process64",
        "-Xgc:CMC",
        "-Xhidden-api-policy:disabled",
        "-Djava.class.path=/data/local/tmp/apkload.dex",
        "/system/bin",
        "ApkLoad",
        "/data/app/com.roblox.client/base.apk",
        "/data/app/com.roblox.client/lib/arm64",
    ];
    argv.extend(libs.iter().map(String::as_str));
    let (app_process, out, err) = spawn(&sysroot, &instance, &argv, &env, 10_000);
    let (sm, _, _) = spawn(&sysroot, &instance, &["/system/bin/servicemanager"], &[], 1000);
    let sm_run = Arc::clone(&sm);
    std::thread::spawn(move || sm_run.run());
    std::thread::sleep(Duration::from_millis(1000));

    let status = app_process.run();
    let printed = text(&out);
    assert_eq!(status, ExitStatus::Exited(0), "{printed}\n{}", text(&err));
    assert!(printed.contains("dex files in the APK: 3"), "{printed}");
    for l in &libs {
        assert!(printed.contains(&format!("loaded lib{l}.so")), "lib{l}.so\n{printed}\n{}", text(&err));
    }
    assert!(text(&err).contains("using isolated ns clns-"), "the app's own linker namespace\n{}", text(&err));
    sm.end(ExitStatus::Exited(0));
}

/// Every entry of a zip (stored or deflated), by name.
fn zip_entries(path: &Path) -> Vec<(String, Vec<u8>)> {
    let data = std::fs::read(path).unwrap();
    let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
    // End of central directory.
    let eocd = (0..data.len() - 21).rev().find(|&i| data[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]).unwrap();
    let (count, mut at) = (u16_at(eocd + 10), u32_at(eocd + 16));
    let mut out = Vec::new();
    for _ in 0..count {
        let method = u16_at(at + 10);
        let (csize, usize_) = (u32_at(at + 20), u32_at(at + 24));
        let (nlen, xlen, clen) = (u16_at(at + 28), u16_at(at + 30), u16_at(at + 32));
        let local = u32_at(at + 42);
        let name = String::from_utf8_lossy(&data[at + 46..at + 46 + nlen]).into_owned();
        at += 46 + nlen + xlen + clen;
        if !name.starts_with("lib/arm64-v8a/") {
            continue;
        }
        let body = local + 30 + u16_at(local + 26) + u16_at(local + 28);
        let raw = &data[body..body + csize];
        let bytes = match method {
            0 => raw.to_vec(),
            8 => {
                let mut v = Vec::with_capacity(usize_);
                std::io::Read::read_to_end(&mut flate2::read::DeflateDecoder::new(raw), &mut v).unwrap();
                v
            }
            m => panic!("{name}: compression {m}"),
        };
        out.push((name, bytes));
    }
    out
}
