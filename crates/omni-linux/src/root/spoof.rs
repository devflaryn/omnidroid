//! The `emu-hide` device spoof: the prop set of a real Pixel 8, applied (per host process, at build
//! time, in `PropertyService::build_from`) over what the image and omnidroid's overlay set, so an
//! app reads a phone and not an emulator. An existing `ro.*` cannot be re-set at runtime, so these
//! replace the values in the `Properties` before the area is frozen.
//!
//! The keys are the finals an app reads (`Build.*` reads `ro.product.*`, `ro.build.fingerprint`,
//! `ro.hardware`, `ro.bootloader`, `ro.build.characteristics`) plus the per-partition variants
//! init derives them from (`ro.product.{system,vendor,product,odm,system_ext}.*` and the partitions'
//! build fingerprints), so no copy of the real values leaks through a partition key.

const FINGERPRINT: &str = "google/shiba/shiba:14/UQ1A.240205.002/11224170:user/release-keys";

const OVERRIDES: &[(&str, &str)] = &[
    ("ro.product.brand", "google"),
    ("ro.product.manufacturer", "Google"),
    ("ro.product.model", "Pixel 8"),
    ("ro.product.device", "shiba"),
    ("ro.product.name", "shiba"),
    ("ro.product.system.brand", "google"),
    ("ro.product.system.manufacturer", "Google"),
    ("ro.product.system.model", "Pixel 8"),
    ("ro.product.system.device", "generic"),
    ("ro.product.system.name", "shiba"),
    ("ro.product.vendor.brand", "google"),
    ("ro.product.vendor.manufacturer", "Google"),
    ("ro.product.vendor.model", "Pixel 8"),
    ("ro.product.vendor.device", "shiba"),
    ("ro.product.vendor.name", "shiba"),
    ("ro.product.product.brand", "google"),
    ("ro.product.product.manufacturer", "Google"),
    ("ro.product.product.model", "Pixel 8"),
    ("ro.product.product.device", "shiba"),
    ("ro.product.product.name", "shiba"),
    ("ro.product.system_ext.brand", "google"),
    ("ro.product.system_ext.manufacturer", "Google"),
    ("ro.product.system_ext.model", "Pixel 8"),
    ("ro.product.system_ext.device", "shiba"),
    ("ro.product.system_ext.name", "shiba"),
    ("ro.product.odm.brand", "google"),
    ("ro.product.odm.manufacturer", "Google"),
    ("ro.product.odm.model", "Pixel 8"),
    ("ro.product.odm.device", "shiba"),
    ("ro.product.odm.name", "shiba"),
    ("ro.build.fingerprint", FINGERPRINT),
    ("ro.system.build.fingerprint", FINGERPRINT),
    ("ro.vendor.build.fingerprint", FINGERPRINT),
    ("ro.product.build.fingerprint", FINGERPRINT),
    ("ro.odm.build.fingerprint", FINGERPRINT),
    ("ro.system_ext.build.fingerprint", FINGERPRINT),
    ("ro.bootloader", "ripcurrent-1.2-9825984"),
    ("ro.build.characteristics", "nosdcard"),
    ("ro.hardware", "zuma"),
    ("ro.boot.hardware", "zuma"),
    ("ro.hardware.chipname", "Tensor G3"),
    ("ro.boot.verifiedbootstate", "green"),
    ("ro.boot.vbmeta.device_state", "locked"),
    ("ro.boot.flash.locked", "1"),
    ("ro.boot.veritymode", "enforcing"),
    ("ro.build.tags", "release-keys"),
    ("ro.build.type", "user"),
    ("ro.build.display.id", "UQ1A.240205.002"),
    ("ro.build.description", "shiba-user 14 UQ1A.240205.002 11224170 release-keys"),
    ("ro.build.product", "shiba"),
    ("ro.product.board", "shiba"),
    ("ro.board.platform", "zuma"),
    ("ro.soc.manufacturer", "Google"),
    ("ro.soc.model", "Tensor G3"),
];

/// The Pixel 8 prop values, `(name, value)`.
#[must_use]
pub fn pixel_overrides() -> &'static [(&'static str, &'static str)] {
    OVERRIDES
}

/// Key prefixes dropped from a spoofed process's set (emulator-only: qemu props).
#[must_use]
pub fn removals() -> &'static [&'static str] {
    &["ro.kernel.qemu", "ro.boot.qemu"]
}

/// `/proc/cpuinfo` of a Pixel 8 (Tensor G3 "zuma"): 4x A520, 3x A720, 1x X3, ending in
/// the `Hardware` line real arm64 Android kernels print. No emulator strings.
#[must_use]
pub fn spoofed_cpuinfo() -> &'static str {
    const FEAT: &str = "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp";
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        // CPU part per core: 0-3 A520, 4-6 A720, 7 X3.
        let parts = [0xd80, 0xd80, 0xd80, 0xd80, 0xd81, 0xd81, 0xd81, 0xd82];
        let mut out = String::new();
        for (n, part) in parts.iter().enumerate() {
            let variant = if *part == 0xd82 { 2 } else { 1 };
            out.push_str(&format!(
                "processor\t: {n}\nBogoMIPS\t: 49.152\nFeatures\t: {FEAT}\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x{variant}\nCPU part\t: {part:#x}\nCPU revision\t: 1\n\n"
            ));
        }
        out.push_str("Hardware\t: Zuma\n");
        out
    })
}

/// `/proc/version` of a generic Android GKI kernel.
#[must_use]
pub fn spoofed_version() -> &'static str {
    "Linux version 5.15.131-android13-8-00055-g4f5025129fe8-ab11150573 (kleaf@build-host) (Android (11368308, based on r510928) clang version 17.0.2, LLD 17.0.2) #1 SMP PREEMPT Mon Dec 18 10:21:17 UTC 2023\n"
}

#[cfg(test)]
mod tests {
    #[test]
    fn spoofed_cpuinfo_and_version_have_no_tells() {
        let c = super::spoofed_cpuinfo();
        let l = c.to_lowercase();
        assert!(!l.contains("omnidroid") && !l.contains("goldfish") && !l.contains("ranchu"));
        assert!(c.contains("Hardware"));
        assert!(!super::spoofed_version().to_lowercase().contains("omnidroid"));
    }

    #[test]
    fn pixel_overrides_cover_the_emulator_tells() {
        let ov: std::collections::HashMap<_, _> = super::pixel_overrides().iter().copied().collect();
        for k in [
            "ro.product.model",
            "ro.product.brand",
            "ro.product.manufacturer",
            "ro.product.device",
            "ro.build.fingerprint",
            "ro.bootloader",
            "ro.build.characteristics",
            "ro.hardware",
        ] {
            assert!(ov.contains_key(k), "spoof missing {k}");
        }
        assert_ne!(ov["ro.hardware"], "omnidroid");
        assert_ne!(ov["ro.build.characteristics"], "emulator");
        assert_eq!(ov["ro.build.characteristics"], "nosdcard");
    }
}
