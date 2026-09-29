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

/// A named guest mapping seen while the program ran: start, end, name (`[anon:...]` or a path),
/// and whether its host address is its guest address (false in a low window, D41).
type Seen = (u64, u64, String, bool);

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str], env: &[String]) -> (ExitStatus, String, String) {
    let (status, out, err, _) = spawn_watching(sysroot, instance, argv, env);
    (status, out, err)
}

/// As `spawn`, and every named mapping the process had while it ran, looked at every 5 ms.
fn spawn_watching(sysroot: &Path, instance: &Path, argv: &[&str], env: &[String]) -> (ExitStatus, String, String, Vec<Seen>) {
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
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let (p, done) = (Arc::clone(&p), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut seen = std::collections::BTreeSet::new();
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                for (start, len, name, _) in p.mm.file_mappings() {
                    let identity = p.mem.space().host_addr(start as usize) == start as usize;
                    seen.insert((start, start + len, String::from_utf8_lossy(&name).into_owned(), identity));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            seen.into_iter().collect::<Vec<Seen>>()
        })
    };
    let status = p.run();
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    let seen = watcher.join().expect("the watcher");
    let s = |b: &Arc<parking_lot::Mutex<Vec<u8>>>| String::from_utf8_lossy(&b.lock()).into_owned();
    (status, s(&out), s(&err), seen)
}

#[test]
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
    let (status, out, err, seen) = spawn_watching(
        &sysroot,
        &instance,
        &["/apex/com.android.art/bin/dalvikvm64", "-Xgc:CMC", "-Xusejit:false", "-cp", "/data/local/tmp/hello.dex", "Hello"],
        &env,
    );
    assert_eq!(out, "Hello from real ART on omnidroid, sum=500500, 2.1.0\n", "stderr:\n{err}");
    assert_eq!(status, ExitStatus::Exited(0), "stderr:\n{err}");

    // Where ART put its heap, its boot image and the boot image's compiled code: below 4 GiB, as
    // 32-bit references need -- the host's own addresses on Windows and Linux, a based window's on
    // macOS (D41), where nothing can be mapped that low. (A `boot.art` or `.oat` *file* mapping
    // elsewhere is ART reading a header before it maps the image into its reservation.)
    let low_4gb = |(start, end, _, _): &&Seen| *end <= 1 << 32 && *start < *end;
    let named = |f: fn(&str) -> bool| seen.iter().filter(|(_, _, n, _)| f(n)).collect::<Vec<&Seen>>();
    let heap = named(|n| n.starts_with("[anon:dalvik-main space"));
    let image = named(|n| n.starts_with("[anon:dalvik-/system/framework/boot") && n.ends_with(".art]"));
    let code: Vec<&Seen> =
        named(|n| n.starts_with("/system/framework/arm64/boot") && n.ends_with(".oat")).into_iter().filter(|m| low_4gb(m)).collect();
    assert!(!code.is_empty(), "the boot image's code was not mapped below 4 GiB; named mappings:\n{seen:#x?}");
    for (what, maps) in [("the Java heap (main space)", &heap), ("the boot image", &image)] {
        assert!(!maps.is_empty(), "ART mapped no {what}; named mappings:\n{seen:#x?}");
        for m in maps {
            assert!(low_4gb(m), "{what}: {m:#x?} is not below 4 GiB");
        }
    }
    let based = omni_platform::vm::lowest_mappable_address() > 0x1000_0000;
    for (start, _, name, identity) in heap.iter().chain(&image).chain(&code) {
        assert_eq!(!identity, based, "{name} at {start:#x}: based exactly where the host maps nothing that low");
    }
    eprintln!(
        "ART's heap {:#x?}, boot image from {:#x}, boot code from {:#x}, {}",
        heap.iter().map(|m| (m.0, m.1)).collect::<Vec<_>>(),
        image.iter().map(|m| m.0).min().unwrap_or(0),
        code.iter().map(|m| m.0).min().unwrap_or(0),
        if based { "in the low window (D41)" } else { "at the host's own addresses (D4)" }
    );
}
