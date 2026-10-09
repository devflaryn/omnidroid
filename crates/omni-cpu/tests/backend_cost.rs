//! **Measurement** (`--ignored`): what one guest process's CPU backend costs the host before it
//! runs anything -- the private memory `DynarmicBackend::new` adds with the options the system's
//! host process gives every guest process (512 guest threads, a shared code cache). The system's
//! host process runs ~65 of them.
#![cfg(target_os = "windows")]

use std::sync::Arc;

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_mem::{GuestSpace, GuestSpaceConfig};

fn regions() -> Vec<(usize, usize)> {
    omni_platform::vm::process_regions()
        .expect("the regions")
        .into_iter()
        .filter(|r| r.committed > 0 && matches!(r.kind, omni_platform::vm::HostRegionKind::Private))
        .map(|r| (r.start, r.len))
        .collect()
}

#[test]
#[ignore = "measurement, not a test"]
fn what_a_guest_process_backend_costs() {
    // One first: one-time costs (dynarmic's statics, the vectored handler) paid before measuring.
    let warm = Arc::new(GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, ..GuestSpaceConfig::default() }).unwrap());
    let _w = DynarmicBackend::new(warm, DynarmicOptions { max_threads: 512, ..DynarmicOptions::default() }).unwrap();

    let before: std::collections::HashSet<(usize, usize)> = regions().into_iter().collect();
    let mem0 = omni_platform::vm::process_memory().unwrap();
    let space = Arc::new(GuestSpace::with_config(GuestSpaceConfig { size: 1 << 30, ..GuestSpaceConfig::default() }).unwrap());
    let backend = DynarmicBackend::new(space, DynarmicOptions { max_threads: 512, ..DynarmicOptions::default() }).unwrap();
    let mem1 = omni_platform::vm::process_memory().unwrap();
    let mut new: Vec<(usize, usize)> = regions().into_iter().filter(|r| !before.contains(r)).collect();
    new.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("{mem0:?}\n{mem1:?}");
    for (start, len) in new.iter().take(12) {
        eprintln!("  new region {start:#x} {} KiB", len >> 10);
    }
    drop(backend);
}
