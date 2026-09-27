//! Sub-project D3b's gate: the display. The real SurfaceFlinger composes on the host GPU (D3a)
//! through the host composer, and the real bootanimation -- an AOSP client with a surface, EGL
//! through ANGLE, the default Android logo -- reaches the host framebuffer. The screenshot is
//! written to `target/d3b_bootanimation.png`.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use omni_linux::binder::{broker, Context};
use omni_linux::hal::composer::{Composer, HEIGHT, WIDTH};
use omni_linux::hal::framebuffer::Framebuffer;
use omni_linux::hal::gralloc::Allocator;
use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

type Buf = Arc<parking_lot::Mutex<Vec<u8>>>;

fn text(b: &Buf) -> String {
    String::from_utf8_lossy(&b.lock()).into_owned()
}

fn spawn(sysroot: &Path, instance: &Path, argv: &[&str], uid: u32) -> (Arc<Process>, Buf) {
    // OMNI_TRACE_PROGRAM=<name>: that program's syscalls traced (all of them: OMNI_SYSCALL_TRACE=1).
    let traced = std::env::var("OMNI_TRACE_PROGRAM").is_ok_and(|n| argv[0].ends_with(&n));
    let log = Buf::default();
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.to_path_buf(),
            instance_dir: instance.to_path_buf(),
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            envp: vec![b"PATH=/system/bin".to_vec(), b"ANDROID_ROOT=/system".to_vec(), b"ANDROID_DATA=/data".to_vec()],
            stdout: Output::Capture(Arc::clone(&log)),
            stderr: Output::Capture(Arc::clone(&log)),
            trace: traced || std::env::var("OMNI_SYSCALL_TRACE").as_deref() == Ok("1"),
        },
        uid,
    )
    .expect("spawn");
    (p, log)
}

fn start(p: &Arc<Process>) {
    let p = Arc::clone(p);
    std::thread::spawn(move || p.run());
}

/// Whether a frame is more than one colour.
fn drawn(pixels: &[u8]) -> bool {
    pixels.chunks_exact(4).any(|px| px != &pixels[0..4])
}

#[test]
fn d3b_bootanimation_reaches_the_host_display() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py): the gate cannot run");
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-d3b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let (p, log) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], 0);
    assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&log));

    let (sm, sm_log) = spawn(&sysroot, &instance, &["/system/bin/servicemanager"], 1000);
    start(&sm);
    let (hwsm, _) = spawn(&sysroot, &instance, &["/system/bin/hwservicemanager"], 1000);
    start(&hwsm);
    // The image's own power HAL (`power-default.rc`, AOSP's example service): SurfaceFlinger waits
    // for the declared `IPower/default` before it composes.
    let (power, _) = spawn(&sysroot, &instance, &["/vendor/bin/hw/android.hardware.power-service.example"], 9999);
    start(&power);
    let binder = broker(Context::Binder);
    let allocator = Allocator::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while let Err(e) = allocator.register(&binder) {
        assert!(std::time::Instant::now() < deadline, "the allocator: {e}\n{}", text(&sm_log));
        std::thread::sleep(Duration::from_millis(100));
    }
    let framebuffer = Arc::new(Framebuffer::new(WIDTH, HEIGHT));
    let composer = Composer::new(Arc::clone(&binder), Arc::clone(&framebuffer));
    composer.register().unwrap_or_else(|e| panic!("the composer: {e}\n{}", text(&sm_log)));

    let (sf, sf_log) = spawn(&sysroot, &instance, &["/system/bin/surfaceflinger"], 1000);
    start(&sf);
    // SurfaceFlinger is up once it has turned the display on (it composes nothing until a client
    // has a layer).
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while !text(&sf_log).contains("Finished setting power mode 2") {
        assert!(std::time::Instant::now() < deadline, "SurfaceFlinger did not turn the display on\nsurfaceflinger: {}", text(&sf_log));
        std::thread::sleep(Duration::from_millis(100));
    }

    let (anim, anim_log) = spawn(&sysroot, &instance, &["/system/bin/bootanimation"], 1003);
    start(&anim);
    let report = || format!("bootanimation: {}\nsurfaceflinger: {}", text(&anim_log), text(&sf_log));
    let wait = std::env::var("OMNI_D3B_WAIT_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(180);
    let deadline = std::time::Instant::now() + Duration::from_secs(wait);
    loop {
        let n = framebuffer.frames();
        if drawn(&framebuffer.pixels()) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // What SurfaceFlinger itself says it is doing, for the failure's record.
            let (dump, dump_log) = spawn(&sysroot, &instance, &["/system/bin/dumpsys", "SurfaceFlinger"], 0);
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || tx.send(dump.run()));
            let _ = rx.recv_timeout(Duration::from_secs(60));
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/d3b_dumpsys.txt");
            let _ = std::fs::write(&path, text(&dump_log));
            panic!("no drawn frame in {wait} s ({n} frames presented); dumpsys SurfaceFlinger in {}\n{}", path.display(), report());
        }
        framebuffer.wait_frame(n + 1, Duration::from_secs(5));
    }
    // A few more frames of the animation, then the picture.
    let n = framebuffer.frames();
    framebuffer.wait_frame(n + 30, Duration::from_secs(30));
    let png = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/d3b_bootanimation.png");
    std::fs::write(&png, framebuffer.png()).unwrap();
    eprintln!("{} frames presented; screenshot {}", framebuffer.frames(), png.display());
    assert!(drawn(&framebuffer.pixels()), "the animation's frame is drawn\n{}", report());
}
