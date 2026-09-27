//! Milestone A3: the real `getprop` finds, maps and reads the property area this layer writes.
mod common;

use common::run;
use omni_linux::ExitStatus;

#[test]
fn a3_getprop_reads_the_sdk_level() {
    let Some((status, out, err)) = run(&["/system/bin/getprop", "ro.build.version.sdk"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert_eq!(out, "35\n");
}

#[test]
fn a3_getprop_reads_the_overlay_and_what_init_derives() {
    let Some((status, out, err)) = run(&["/system/bin/getprop", "ro.product.cpu.abi"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert_eq!(out, "arm64-v8a\n");
    let Some((status, out, err)) = run(&["/system/bin/getprop", "ro.build.fingerprint"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert!(out.contains(":15/") && out.matches('/').count() == 5, "a fingerprint of init's shape: {out}");
}

#[test]
fn a3_libc_initializes_its_properties_without_complaint() {
    let Some((status, _out, err)) = run(&["/system/bin/toybox", "true"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    assert!(!err.contains("propert"), "no property warning from libc: {err}");
}

/// What a bootloader hands init (`androidboot.*`, as `ro.boot.*`): the verified-boot state of an
/// unlocked device, and the pinned image's own vbmeta digest (its `VerifiedBootParams.textproto`).
/// KeyMint waits for these before it registers.
#[test]
fn a3_the_bootloader_properties_are_the_images() {
    for (name, want) in [
        ("ro.boot.verifiedbootstate", "orange"),
        ("ro.boot.vbmeta.device_state", "unlocked"),
        ("ro.boot.vbmeta.digest", "836f26adcab3883794ba405c6bf019f74afbdc3c9d76bdb26cb1ea1672ffa8e8"),
        ("ro.boot.vbmeta.hash_alg", "sha256"),
        ("ro.boot.vbmeta.size", "6720"),
        // The vendor partition's own (its build.prop), which its HALs read.
        ("ro.vendor.build.security_patch", "2024-09-05"),
    ] {
        let Some((status, out, err)) = run(&["/system/bin/getprop", name]) else { return };
        assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
        assert_eq!(out.trim(), want, "{name}");
    }
}
