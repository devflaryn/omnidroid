//! Sub-project B: the guest space below 4 GiB. Its own test binary: the space is one per host
//! process at that address, and tests in one binary run in parallel.
mod common;

use std::sync::Arc;

use omni_linux::{ExitStatus, Output, Process, SpawnConfig};

/// ART keeps its heap and boot image below 4 GiB (compressed references), so the guest space
/// starts there, and mappings made without a hint stay out of the low 4 GiB -- as on Linux, where
/// the top-down `mmap_base` puts them high -- leaving it for what ART asks for by address.
#[test]
fn the_space_starts_below_4_gib_and_unhinted_mappings_leave_it_free() {
    let Some(sysroot) = common::sysroot() else { return };
    let out = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let p = Process::spawn(SpawnConfig {
        sysroot,
        instance_dir: std::env::temp_dir().join(format!("omni-linux-low-{}", std::process::id())),
        argv: vec![b"/system/bin/toybox".to_vec(), b"cat".to_vec(), b"/proc/self/maps".to_vec()],
        envp: vec![b"PATH=/system/bin".to_vec()],
        stdout: Output::Capture(Arc::clone(&out)),
        stderr: Output::Capture(Arc::default()),
        trace: false,
    })
    .expect("spawn");
    let base = p.mem.space().base() as u64;
    assert!(base < 1 << 32, "the guest space starts at {base:#x}");
    assert_eq!(p.run(), ExitStatus::Exited(0));
    let maps = String::from_utf8_lossy(&out.lock()).into_owned();
    let low: Vec<&str> = maps
        .lines()
        // The CPU backend's TLS pool, made before the program starts, sits at the base; what the
        // host holds inside the range (Windows' KUSER_SHARED_DATA) shows as taken, and is the host's.
        .filter(|l| {
            l.split('-').next().and_then(|a| u64::from_str_radix(a, 16).ok()).is_some_and(|a| {
                let host = p.mem.space().region_at(a as usize).is_some_and(|r| matches!(r.kind, omni_mem::RegionKind::Host));
                a < 1 << 32 && a != base && !host
            })
        })
        .collect();
    assert!(low.is_empty(), "mapped below 4 GiB:\n{}", low.join("\n"));
}
