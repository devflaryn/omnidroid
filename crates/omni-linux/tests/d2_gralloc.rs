//! Sub-project D2's gate: a guest process allocates a graphics buffer through the real libui
//! (`AHardwareBuffer_allocate` -> gralloc 5 -> the host `IAllocator` -> `mapper.omni.so`), writes
//! it through its mapping, and the host reads the same memory through its own -- then writes into
//! it, and the guest sees that write.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::binder::{broker, Context};
use omni_linux::hal::gralloc::{Allocator, PIXELS_AT};
use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

type Buf = Arc<parking_lot::Mutex<Vec<u8>>>;

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

fn text(b: &Buf) -> String {
    String::from_utf8_lossy(&b.lock()).into_owned()
}

/// The pattern the fixture writes: pixel (x, y) is `0xFF000000 | y << 8 | x`.
fn pattern(x: u64, y: u64) -> u32 {
    0xFF00_0000 | (y as u32) << 8 | x as u32
}

#[test]
fn d2_a_guest_allocates_a_graphics_buffer_the_host_maps() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py): the gate cannot run");
    let instance: PathBuf = std::env::temp_dir().join(format!("omni-linux-d2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    {
        let (p, _, e) = spawn(&sysroot, &instance, &["/apex/com.android.runtime/bin/linkerconfig", "--target", "/linkerconfig"], 0);
        assert_eq!(p.run(), ExitStatus::Exited(0), "linkerconfig: {}", text(&e));
    }
    let (sm, _, sm_err) = spawn(&sysroot, &instance, &["/system/bin/servicemanager"], 1000);
    let server = {
        let sm = Arc::clone(&sm);
        std::thread::spawn(move || sm.run())
    };
    std::thread::sleep(Duration::from_millis(1500));

    let allocator = Allocator::new();
    allocator.register(&broker(Context::Binder)).unwrap_or_else(|e| panic!("register the allocator: {e}\nservicemanager: {}", text(&sm_err)));

    let tmp = instance.join("data/local/tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/gralloc");
    std::fs::copy(&fixture, tmp.join("gralloc")).unwrap_or_else(|e| panic!("{}: {e}", fixture.display()));
    let (app, out, err) = spawn(&sysroot, &instance, &["/data/local/tmp/gralloc"], 10_000);
    let (tx, rx) = std::sync::mpsc::channel();
    {
        let app = Arc::clone(&app);
        std::thread::spawn(move || tx.send(app.run()));
    }
    let report = || format!("stdout: {}\nstderr: {}\nservicemanager: {}", text(&out), text(&err), text(&sm_err));

    // The fixture prints its buffer's id once it has written the pattern, then waits for the host.
    // (Its id is libui's `GraphicBuffer` id, not the allocator's.)
    let deadline = Instant::now() + Duration::from_secs(90);
    let stride = loop {
        let o = text(&out);
        if let Some(line) = o.lines().find(|l| l.starts_with("id=")) {
            break line.split("stride=").nth(1).and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
        }
        if let Ok(status) = rx.try_recv() {
            panic!("the fixture ended ({status:?}) before allocating\n{}", report());
        }
        assert!(Instant::now() < deadline, "no buffer within 90 s\n{}", report());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(stride, 64, "a 64-pixel-wide RGBA buffer's stride\n{}", report());

    // The host's view of the same memory: the allocator's own region, the one buffer the fixture
    // holds.
    let live = allocator.live();
    assert_eq!(live.len(), 1, "the fixture allocated one buffer and holds it\n{}", report());
    let (id, shm) = live.into_iter().next().expect("one");
    assert!(allocator.buffer(id).is_some_and(|b| Arc::ptr_eq(&b, &shm)));
    let mut pixels = vec![0u8; (stride * 32 * 4) as usize];
    assert_eq!(shm.read_at(&mut pixels, PIXELS_AT).unwrap(), pixels.len());
    for y in 0..32u64 {
        for x in 0..64u64 {
            let at = ((y * stride + x) * 4) as usize;
            let got = u32::from_le_bytes(pixels[at..at + 4].try_into().unwrap());
            assert_eq!(got, pattern(x, y), "pixel ({x}, {y}) as the host reads it\n{}", report());
        }
    }
    // The host writes the last pixel; the guest must see it through its mapping.
    shm.write_at(&0x1234_5678u32.to_le_bytes(), PIXELS_AT + (31 * stride + 63) * 4).unwrap();
    std::fs::write(tmp.join("gralloc.go"), b"").unwrap();

    let status = rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| panic!("the fixture did not finish\n{}", report()));
    assert_eq!(status, ExitStatus::Exited(0), "{}", report());
    let o = text(&out);
    assert!(o.contains("host write seen") && o.contains("verified"), "{}", report());

    sm.end(ExitStatus::Exited(0));
    let _ = server.join();
}
