//! Sub-project D3a's gate: the guest has a GPU. Vulkan through the real Android loader and
//! omnidroid's driver (`/vendor/lib64/hw/vulkan.omni.so`) runs on the host's GPU; GLES through the
//! image's own ANGLE runs on that Vulkan.
mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use omni_linux::binder::{broker, Context};
use omni_linux::hal::gralloc::Allocator;
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

/// What every process here finds, as on a device: the real `servicemanager` and the graphics
/// allocator (D2), which libui -- under EGL and ANGLE -- asks for. Once per test binary: the binder
/// broker is per host process.
fn device(sysroot: &Path) -> &'static Arc<Allocator> {
    static DEVICE: OnceLock<(Arc<Process>, Arc<Allocator>)> = OnceLock::new();
    &DEVICE
        .get_or_init(|| {
            let instance = std::env::temp_dir().join(format!("omni-linux-d3a-device-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&instance);
            let (p, _, e) = spawn(sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], 0);
            assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&e));
            let (sm, _, sm_err) = spawn(sysroot, &instance, &["/system/bin/servicemanager"], 1000);
            {
                let sm = Arc::clone(&sm);
                std::thread::spawn(move || sm.run());
            }
            // HIDL's service manager: libhidl waits for it before any lookup (EGL looks up the
            // SurfaceFlinger configstore, which is not declared and so not found).
            let (hwsm, _, _) = spawn(sysroot, &instance, &["/system/bin/hwservicemanager"], 1000);
            std::thread::spawn(move || hwsm.run());
            let allocator = Allocator::new();
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            // servicemanager takes the context manager once it has started.
            while let Err(e) = allocator.register(&broker(Context::Binder)) {
                assert!(std::time::Instant::now() < deadline, "the allocator: {e}\nservicemanager: {}", text(&sm_err));
                std::thread::sleep(Duration::from_millis(100));
            }
            (sm, allocator)
        })
        .1
}

/// An instance with its linker configuration, and a fixture copied to its `/data/local/tmp`.
fn prepare(name: &str, fixture: &str) -> (PathBuf, PathBuf) {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py): the gate cannot run");
    device(&sysroot);
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

#[test]
fn angle_clears_a_pbuffer_on_the_host_gpu() {
    let (sysroot, instance) = prepare("gl", "glclear");
    let (status, out, err) = run_fixture(&sysroot, &instance, "glclear");
    assert_eq!(status, ExitStatus::Exited(0), "glclear\nstdout: {out}\nstderr: {err}");
    let line = out.lines().find(|l| l.starts_with("gles ok ")).unwrap_or_else(|| panic!("stdout: {out}\nstderr: {err}"));
    assert!(line.contains("ANGLE"), "the renderer is ANGLE's: {line}");
    eprintln!("{line}");
}

/// GLES renders into a gralloc buffer through an `EGLImage` -- what SurfaceFlinger's RenderEngine
/// does with every output -- and the colour is in the buffer's `shm` region, where the host reads
/// it (and so the host composer will).
#[test]
fn an_egl_image_on_a_gralloc_buffer_renders_where_the_host_reads_it() {
    let (sysroot, instance) = prepare("ahb", "ahbrender");
    let allocator = device(&sysroot);
    let before: Vec<u64> = allocator.live().into_iter().map(|(id, _)| id).collect();
    let (p, out, err) = spawn(&sysroot, &instance, &["/data/local/tmp/ahbrender"], 10_000);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(p.run()));
    let report = || format!("stdout: {}\nstderr: {}", text(&out), text(&err));
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let stride = loop {
        if let Some(line) = text(&out).lines().find(|l| l.starts_with("ahb render ok")) {
            break line.split("stride=").nth(1).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
        }
        if let Ok(status) = rx.try_recv() {
            panic!("ahbrender ended ({status:?})\n{}", report());
        }
        assert!(std::time::Instant::now() < deadline, "no render within 120 s\n{}", report());
        std::thread::sleep(Duration::from_millis(20));
    };
    // The one buffer this fixture allocated, read by the host.
    let mine: Vec<_> = allocator.live().into_iter().filter(|(id, _)| !before.contains(id)).collect();
    assert_eq!(mine.len(), 1, "one new gralloc buffer\n{}", report());
    let shm = &mine[0].1;
    let mut px = vec![0u8; (stride * 32 * 4) as usize];
    shm.read_at(&mut px, omni_linux::hal::gralloc::PIXELS_AT).unwrap();
    for y in 0..32u64 {
        for x in 0..64u64 {
            let at = ((y * stride + x) * 4) as usize;
            let q = &px[at..at + 4];
            assert!((63..=65).contains(&q[0]) && (127..=129).contains(&q[1]) && (190..=192).contains(&q[2]) && q[3] == 255, "pixel {x},{y} as the host reads it: {q:?}\n{}", report());
        }
    }
    std::fs::write(instance.join("data/local/tmp/ahbrender.go"), b"").unwrap();
    let status = rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("ahbrender did not finish\n{}", report()));
    assert_eq!(status, ExitStatus::Exited(0), "{}", report());
}
