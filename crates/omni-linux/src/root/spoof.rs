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
    ("ro.soc.manufacturer", "Google"),
    ("ro.soc.model", "Tensor G3"),
];

/// The Pixel 8 prop values, `(name, value)`.
#[must_use]
pub fn pixel_overrides() -> &'static [(&'static str, &'static str)] {
    OVERRIDES
}

/// Emulator-only keys dropped from a spoofed process's set (defensive: the overlay sets none).
#[must_use]
pub fn removals() -> &'static [&'static str] {
    &["ro.kernel.qemu", "ro.kernel.qemu.gles", "ro.boot.qemu", "ro.boot.qemu.avd_name", "ro.boot.qemu.gltransport", "init.svc.qemu-props", "qemu.hw.mainkeys", "qemu.sf.lcd_density"]
}

#[cfg(test)]
mod tests {
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
