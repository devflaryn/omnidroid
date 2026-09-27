//! A property set after a process was spawned but before it maps the property area is what it reads
//! (hwservicemanager sets `hwservicemanager.ready` while SurfaceFlinger, spawned first, has yet to
//! start: SurfaceFlinger must see it, or it waits forever).
mod common;

use std::sync::Arc;

use omni_linux::props::PropertyService;
use omni_linux::vfs::Sysroot;
use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

#[test]
fn a_property_set_between_spawn_and_start_is_read() {
    let sysroot = common::sysroot().expect("no sysroot (tools/make_sysroot.py)");
    let instance = std::env::temp_dir().join(format!("omni-linux-props-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&instance);
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn_as(
        SpawnConfig {
            sysroot: sysroot.clone(),
            instance_dir: instance,
            argv: vec![b"/system/bin/getprop".to_vec(), b"omni.test.late".to_vec()],
            envp: vec![b"PATH=/system/bin".to_vec()],
            stdout: Output::Capture(Arc::clone(&out)),
            stderr: Output::Capture(Arc::default()),
            trace: false,
        },
        10_000,
    )
    .expect("spawn");
    // Set after the process exists, before it has run a single instruction.
    let service = PropertyService::global(&Sysroot::open(&sysroot).expect("sysroot"));
    assert_eq!(service.set("omni.test.late", "seen"), 0);
    assert_eq!(p.run(), ExitStatus::Exited(0));
    assert_eq!(String::from_utf8_lossy(&out.lock()).trim(), "seen");
}
