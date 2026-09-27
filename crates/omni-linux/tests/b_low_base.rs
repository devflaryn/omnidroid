//! Sub-project B: the guest space below 4 GiB. Its own test binary: the space is one per host
//! process at that address, and tests in one binary run in parallel.
mod common;

/// Sub-project B: ART keeps its heap below 4 GiB (compressed references), so the guest space
/// starts there and the first mappings land there.
#[test]
fn the_guest_space_starts_below_4_gib() {
    let Some((status, out, err)) = common::run(&["/system/bin/toybox", "cat", "/proc/self/maps"]) else { return };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "stderr: {err}");
    let lowest = out.lines().filter_map(|l| u64::from_str_radix(l.split('-').next()?, 16).ok()).min().unwrap();
    assert!(lowest < 1 << 32, "lowest mapping {lowest:#x}\n{out}");
}
