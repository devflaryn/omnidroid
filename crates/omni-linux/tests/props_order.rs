//! Property changes reach a process's mapped area in the order they were made. Two setters that
//! published unordered let an older snapshot land last: a property went back to its old value and
//! serial in a process's mapping, and a waiter on that serial slept for good -- vold's
//! `WaitForProperty(selinux.restorecon_recursive)`, then system_server's Watchdog killed it after
//! 65 s in `IVold.prepareUserStorage` (r14).
mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use omni_linux::props::PropertyService;
use omni_linux::vfs::Sysroot;
use omni_linux::{Output, Process, SpawnConfig};

#[test]
fn concurrent_setters_leave_every_mapping_current() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-props-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.clone(),
            instance_dir: instance,
            argv: vec![b"/system/bin/sh".to_vec(), b"-c".to_vec(), b"sleep 20".to_vec()],
            envp: vec![b"PATH=/system/bin".to_vec()],
            stdout: Output::Capture(Arc::default()),
            stderr: Output::Capture(Arc::default()),
            trace: false,
        },
        0,
    )
    .expect("spawn");
    let runner = Arc::clone(&p);
    std::thread::spawn(move || runner.run());
    // The shell's bionic maps the property areas at start.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mapping = loop {
        if let Some(m) = p.mm.file_mappings().into_iter().find(|(_, _, name, _)| name.starts_with(b"/dev/__properties__/u:object_r:")) {
            break m;
        }
        assert!(Instant::now() < deadline, "the shell never mapped its property areas");
        std::thread::sleep(Duration::from_millis(20));
    };
    let service = PropertyService::global(&Sysroot::open(&sysroot).expect("sysroot"));
    // A consistency check, not a reproduction: 16 x 1,500 concurrent sets did not produce the
    // stale ordering on the old code in 4 runs (its window is a thread descheduled between snapshot
    // and write); the fix (`PropertyService::publish`) is argued in props.rs.
    let setters: Vec<_> = (0..8)
        .map(|k| {
            let service = Arc::clone(&service);
            std::thread::spawn(move || {
                for i in 0..300 {
                    assert_eq!(service.set(&format!("omni.order.{k}"), &i.to_string()), 0);
                }
            })
        })
        .collect();
    for s in setters {
        s.join().unwrap();
    }
    let want = service.area_bytes();
    let (start, len, _, _) = mapping;
    let n = want.len().min(len as usize);
    let got = p.mem.read(start, n).expect("the mapping");
    let first = got.iter().zip(&want[..n]).position(|(a, b)| a != b);
    assert_eq!(first, None, "the process's mapped area differs from the service's at byte {first:?}: a stale snapshot landed last");
}
