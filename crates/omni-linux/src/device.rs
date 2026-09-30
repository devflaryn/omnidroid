//! omnidroid's device overlay: the files this runtime adds over the pinned AOSP image as a device's
//! vendor adds them -- the declarations of the HALs it serves from the host, and the in-process
//! halves of those HALs (a gralloc mapper). They live in `crates/omni-linux/device/` at their guest
//! paths and are compiled in; [`crate::vfs::Sysroot::open`] shows them as sysroot files. The image
//! is never edited: its manifest and its pin are unchanged.
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::manifest::Entry;

/// Every overlay file: its guest path and its bytes.
pub const FILES: &[(&str, &[u8])] = &[
    ("/vendor/etc/vintf/manifest/omni-graphics.xml", include_bytes!("../device/vendor/etc/vintf/manifest/omni-graphics.xml")),
    // gralloc 5's in-process mapper (`device/src/mapper.c`; `device/src/build.txt`, `device/SHA256SUMS`).
    ("/vendor/lib64/hw/mapper.omni.so", include_bytes!("../device/vendor/lib64/hw/mapper.omni.so")),
    // The boot ramdisk's global environment (AOSP's init.environ.rc), which init.rc imports.
    ("/init.environ.rc", include_bytes!("../device/init.environ.rc")),
    // The Vulkan driver, forwarding to the host's GPU (`device/src/vk/`; `crate::gpu`).
    ("/vendor/lib64/hw/vulkan.omni.so", include_bytes!("../device/vendor/lib64/hw/vulkan.omni.so")),
    // The GLES driver of the GL backend (`OMNI_GPU=gl`, a host whose GPU has no Vulkan), forwarding
    // every GLES command to the host's GLES (`device/src/gl/`; `crate::gpu::gl`). Android's EGL
    // loader takes it when `ro.hardware.egl=omni` (`crate::gpu::backend`).
    ("/vendor/lib64/egl/libGLES_omni.so", include_bytes!("../device/vendor/lib64/egl/libGLES_omni.so")),
    // This device has no sensors: the AOSP sensors multihal loads no sub-HAL and serves an empty
    // list (the image's lists the emulator's, which needs QEMU's sensors transport).
    ("/vendor/etc/sensors/hals.conf", include_bytes!("../device/vendor/etc/sensors/hals.conf")),
    // This device has no modem: its RIL is declared and not started (the file says why).
    ("/vendor/etc/init/rild_goldfish.rc", include_bytes!("../device/vendor/etc/init/rild_goldfish.rc")),
    // Nor does it declare telephony: the image's handheld features less `android.hardware.telephony*`
    // (the file says why).
    ("/vendor/etc/permissions/handheld_core_hardware.xml", include_bytes!("../device/vendor/etc/permissions/handheld_core_hardware.xml")),
    // Installed apps are granted what they ask for: a closed device with no owner's data on it (the
    // script says why; `persist.omni.autogrant=0` turns it off).
    ("/vendor/etc/init/omni_autogrant.rc", include_bytes!("../device/vendor/etc/init/omni_autogrant.rc")),
    ("/vendor/bin/omni_autogrant.sh", include_bytes!("../device/vendor/bin/omni_autogrant.sh")),
    // No background app processes kept: ActivityManager's cached-process limit (the script says
    // why; `persist.omni.cached_processes`).
    ("/vendor/etc/init/omni_lean.rc", include_bytes!("../device/vendor/etc/init/omni_lean.rc")),
    ("/vendor/bin/omni_lean.sh", include_bytes!("../device/vendor/bin/omni_lean.sh")),
];

/// The image's vendor files this device replaces with its own: device configuration, which a
/// vendor partition holds for its hardware. Any other overlay path already in the image is an
/// error.
pub const REPLACES: &[&str] = &["/vendor/etc/sensors/hals.conf", "/vendor/etc/init/rild_goldfish.rc", "/vendor/etc/permissions/handheld_core_hardware.xml"];

/// The image's apps and feature files this device does not have, as a product build leaves
/// packages out of its `PRODUCT_PACKAGES`: PackageManager never scans them, so nothing starts them
/// -- not a boot broadcast, and not `persistent` (which `pm disable-user` cannot stop: Android does
/// not kill a persistent process when its package is disabled, run 2026-09-28: `com.android.se` and
/// `com.android.emulator.multidisplay` alive after it). Each app process costs a host process of
/// ~260 MiB (the same run: 16 such, 5.0 GiB beside the app). A directory drops what is under it.
///
/// What a device of this kind has no use for: telephony (no modem, `rild_goldfish.rc`; the
/// `com.android.phone` stack is persistent), the secure element, the emulator's multi-display
/// provider, Bluetooth (its feature files: no feature, no Bluetooth service), printing and backup
/// (their features are gone from `handheld_core_hardware.xml` too, so their services never bind
/// the apps), and the phone's own apps. Kept: everything PackageManager requires (installer,
/// permission controller, ext services, settings, shell), the network stack, the WebView (the
/// app loads it at start: `VariationsSeedServer`), the keyboard, media storage.
pub const LEAVES_OUT: &[&str] = &[
    // Telephony.
    "/system/priv-app/TeleService",
    "/system/priv-app/TelephonyProvider",
    "/system_ext/priv-app/CarrierConfig",
    "/system/app/Stk",
    "/system/priv-app/MmsService",
    "/system/priv-app/ONS",
    "/system/priv-app/CellBroadcastLegacyApp",
    "/system/app/CarrierDefaultApp",
    "/system/app/SimAppDialog",
    "/system_ext/priv-app/EmulatorRadioConfig",
    "/product/priv-app/ImsServiceEntitlement",
    "/product/priv-app/Dialer",
    "/system/priv-app/CallLogBackup",
    "/system_ext/priv-app/EmergencyInfo",
    // The secure element and the emulator's displays: persistent.
    "/system/app/SecureElement",
    "/system_ext/priv-app/MultiDisplayProvider",
    // Bluetooth: the features (SystemServer starts no Bluetooth service without them).
    "/vendor/etc/permissions/android.hardware.bluetooth.xml",
    "/vendor/etc/permissions/android.hardware.bluetooth_le.xml",
    "/system/app/BluetoothMidiService",
    // Printing and backup (features gone; nothing binds these).
    "/system/app/PrintSpooler",
    "/system/priv-app/BuiltInPrintService",
    "/system/app/PrintRecommendationService",
    "/system/priv-app/LocalTransport",
    "/system/priv-app/SharedStorageBackup",
    // Contacts, calendar, messaging.
    "/product/priv-app/Contacts",
    "/system/priv-app/ContactsProvider",
    "/system/priv-app/E2eeContactKeysProvider",
    "/system/priv-app/BlockedNumberProvider",
    "/product/app/Calendar",
    "/system/priv-app/CalendarProvider",
    "/product/app/messaging",
    // A phone's own apps.
    "/product/app/Camera2",
    "/product/app/Gallery2",
    "/product/app/DeskClock",
    "/product/app/Music",
    "/product/app/PhotoTable",
    "/product/app/QuickSearchBox",
    "/product/app/Browser2",
    "/system/app/EasterEgg",
    "/system/app/BasicDreams",
    "/system/priv-app/LiveWallpapersPicker",
    "/system_ext/priv-app/ThemePicker",
    "/system/priv-app/AvatarPicker",
    "/system/priv-app/DeviceDiagnostics",
    "/system/priv-app/DeviceAsWebcam",
    "/system/app/Traceur",
    "/system/priv-app/DynamicSystemInstallationService",
];

/// **The hardware this device does not have**, left out as a product build leaves a HAL's package
/// out: its init service (`.rc`), its VINTF declaration (so no client waits for it) and its
/// feature files (so no system service or app looks for it). What remains is what the app and the
/// runtime use: audio, graphics (the host's), input, health, keystore/KeyMint, gatekeeper, the
/// network, media.
///
/// Gone: cameras (the emulator's two providers and the virtual camera; cameraserver stays and
/// reports none), fingerprint and
/// face (the emulator's and an example), Bluetooth's HAL,
/// USB (its HAL and `usbd`), the context hub, identity credentials (HAL and `credstore`), lights,
/// the vibrator, power statistics, the mock thermal HAL, GNSS, the goldfish codec2 HAL (the software
/// codecs stay), the goldfish allocator 3.0 and composer 3 (the host serves gralloc 5 and the
/// composer), the atrace HAL, and the daemons of a phone's maintenance: `incidentd`, `storaged`,
/// `update_verifier`, `misctrl`. Each ran as a guest process of the system's host process, with its
/// own memory and binder threads (`[init] started 67 services`, run 2026-09-28).
pub const HARDWARE_LEFT_OUT: &[&str] = &[
    // Cameras: the HALs. cameraserver stays, as on any device without a camera -- it answers
    // that there is none; without it apps wait for `media.camera` (Roblox's engine did, its
    // display stalled, run 2026-09-29).
    "/system/etc/init/virtual_camera.hal.rc",
    "/vendor/etc/init/android.hardware.camera.provider.ranchu.rc",
    "/vendor/etc/vintf/manifest/android.hardware.camera.provider.ranchu.xml",
    "/vendor/etc/init/android.hardware.camera.provider@2.7-service-google.rc",
    "/vendor/etc/vintf/manifest/android.hardware.camera.provider@2.7-service-google.xml",
    "/vendor/etc/permissions/android.hardware.camera.concurrent.xml",
    "/vendor/etc/permissions/android.hardware.camera.flash-autofocus.xml",
    "/vendor/etc/permissions/android.hardware.camera.front.xml",
    "/vendor/etc/permissions/android.hardware.camera.full.xml",
    "/vendor/etc/permissions/android.hardware.camera.raw.xml",
    // Bluetooth's HAL (its features are in LEAVES_OUT).
    "/vendor/etc/init/bluetooth-service-default.rc",
    "/vendor/etc/vintf/manifest/bluetooth-service-default.xml",
    // USB.
    "/system/etc/init/usbd.rc",
    "/vendor/etc/init/android.hardware.usb-service.example.rc",
    "/vendor/etc/vintf/manifest/android.hardware.usb-service.example.xml",
    // Context hub, identity credentials.
    "/vendor/etc/init/contexthub-default.rc",
    "/vendor/etc/vintf/manifest/contexthub-default.xml",
    "/vendor/etc/init/identity-default.rc",
    "/vendor/etc/vintf/manifest/identity-default.xml",
    "/vendor/etc/permissions/android.hardware.identity_credential.xml",
    "/system/etc/init/credstore.rc",
    // Lights, vibrator, power statistics, thermal, GNSS.
    "/vendor/etc/init/lights-default.rc",
    "/vendor/etc/vintf/manifest/lights-default.xml",
    "/vendor/etc/init/vibrator-default.rc",
    "/vendor/etc/vintf/manifest/android.hardware.vibrator.xml",
    "/vendor/etc/init/power.stats-default.rc",
    "/vendor/etc/vintf/manifest/android.hardware.power.stats.xml",
    "/vendor/etc/init/android.hardware.thermal@2.0-service.rc",
    "/vendor/etc/vintf/manifest/android.hardware.thermal@2.0-service.xml",
    "/vendor/etc/init/android.hardware.gnss-service.ranchu.rc",
    "/vendor/etc/vintf/manifest/android.hardware.gnss-service.ranchu.xml",
    // The emulator's codec2, allocator 3.0 and composer 3 (they need QEMU's pipe; the host serves
    // graphics).
    "/vendor/etc/init/android.hardware.media.c2@1.0-service-goldfish.rc",
    "/vendor/etc/vintf/manifest/android.hardware.media.c2@1.0-service-goldfish.xml",
    "/vendor/etc/init/android.hardware.graphics.allocator@3.0-service.ranchu.rc",
    "/vendor/etc/vintf/manifest/android.hardware.graphics.gralloc3.ranchu.xml",
    "/vendor/etc/init/hwc3.rc",
    // Biometrics: the emulator's fingerprint HAL needs QEMU's pipe (it aborted at every boot:
    // "Could not open the sensor service: 'fingerprintlisten'"), the face HAL is an example.
    "/vendor/etc/init/android.hardware.biometrics.fingerprint-service.ranchu.rc",
    "/vendor/etc/vintf/manifest/android.hardware.biometrics.fingerprint-service.ranchu.xml",
    "/vendor/etc/permissions/android.hardware.fingerprint.xml",
    "/vendor/etc/init/face-example.rc",
    "/vendor/etc/vintf/manifest/face-example.xml",
    "/vendor/etc/permissions/android.hardware.biometrics.face.xml",
    // Tracing and a phone's maintenance daemons.
    "/vendor/etc/init/android.hardware.atrace@1.0-service.rc",
    "/vendor/etc/vintf/manifest/android.hardware.atrace@1.0-service.xml",
    "/system/etc/init/incidentd.rc",
    "/system/etc/init/storaged.rc",
    "/system/etc/init/update_verifier.rc",
    "/system/etc/init/misctrl.rc",
];

/// A kiosk device (`OMNI_DEVICE_APPS=kiosk`) has also no SystemUI and no launcher: no status bar,
/// navigation bar, taskbar or keyguard, the app given the whole display from the first boot
/// (`tests/d8_app_only.rs` ran it as SystemUI disabled at a second boot). The home is Settings'
/// `FallbackHome`, which the system starts when no launcher is there.
///
/// `OMNI_KIOSK_IME=0` also leaves out the soft keyboard: the device's keyboard is the host's, which
/// a text field takes without an input method, and LatinIME is a host process of its own (~120 MiB,
/// run 2026-09-29) that every key visits first (`ImeInputStage`). Not the default: in PS99 without
/// it the first held key took 347-425 ms and a few more keys ended with the app force-finished
/// (run 2026-09-29, "runc"), cause not yet pinned.
pub const KIOSK_LEAVES_OUT: &[&str] = &["/system_ext/priv-app/SystemUI", "/system_ext/priv-app/Launcher3QuickStep"];
/// The soft keyboard, left out of a kiosk device with `OMNI_KIOSK_IME=0` (see [`KIOSK_LEAVES_OUT`]).
pub const KIOSK_IME: &str = "/product/app/LatinIME";

/// The device's global environment: `init.environ.rc`'s `export NAME VALUE` lines. On a device
/// init exports them before anything starts, so every process -- the zygote, system_server, each
/// app -- has them; the processes omnidroid starts itself are given them too. system_server needs
/// `EXTERNAL_STORAGE` when a caller asks for the storage volumes before the primary one is mounted
/// ("No primary storage defined yet; hacking together a stub"): without it, `new File(null)` threw
/// in its main thread and the system died -- on a slow host, where that call comes first (Colab's
/// 2 vCPUs, 2026-09-30).
#[must_use]
pub fn global_environment() -> Vec<(String, String)> {
    let rc = FILES.iter().find(|(path, _)| *path == "/init.environ.rc").map_or(&[][..], |(_, bytes)| *bytes);
    String::from_utf8_lossy(rc)
        .lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["export", name, value] => Some((name.to_string(), value.to_string())),
            _ => None,
        })
        .collect()
}

/// What `OMNI_DEVICE_APPS` makes of the image: `full` (the image as it is), `lean` (the default:
/// [`LEAVES_OUT`] and [`HARDWARE_LEFT_OUT`] left out), `lean-hw` (only [`LEAVES_OUT`]: the hardware
/// kept) or `kiosk` (lean, and [`KIOSK_LEAVES_OUT`]). Read by each host process of an
/// instance alike (the variable is inherited), so they all see one image.
#[must_use]
pub fn left_out() -> Vec<&'static str> {
    match std::env::var("OMNI_DEVICE_APPS").as_deref() {
        Ok("full") => Vec::new(),
        Ok("kiosk") => {
            let ime = (std::env::var("OMNI_KIOSK_IME").as_deref() == Ok("0")).then_some(KIOSK_IME);
            LEAVES_OUT.iter().chain(HARDWARE_LEFT_OUT).chain(KIOSK_LEAVES_OUT).copied().chain(ime).collect()
        }
        Ok("lean-hw") => LEAVES_OUT.to_vec(),
        _ => LEAVES_OUT.iter().chain(HARDWARE_LEFT_OUT).copied().collect(),
    }
}

/// An overlay file as the sysroot holds it: its guest path, its manifest entry, and the host file
/// with its bytes.
pub struct Materialized {
    pub guest: Vec<u8>,
    pub entry: Entry,
    pub host: PathBuf,
}

/// Write each overlay file once to a content-addressed host file (mapping a library needs a real
/// file), and describe it.
///
/// # Errors
/// A host file that cannot be written.
pub fn materialize() -> Result<Vec<Materialized>, String> {
    let dir = std::env::temp_dir().join("omni-device");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    FILES
        .iter()
        .map(|(guest, bytes)| {
            let sha256 = format!("{:x}", Sha256::digest(bytes));
            let host = dir.join(&sha256);
            if std::fs::read(&host).ok().as_deref() != Some(*bytes) {
                // Written beside and renamed, so a process mapping the file never sees it partial.
                let tmp = dir.join(format!("{sha256}.{}", std::process::id()));
                std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
                if std::fs::rename(&tmp, &host).is_err() {
                    let _ = std::fs::remove_file(&tmp);
                    if std::fs::read(&host).ok().as_deref() != Some(*bytes) {
                        return Err(format!("{}: could not be written", host.display()));
                    }
                }
            }
            // Libraries too: the image's own are 0644 (the linker maps them; nothing executes them).
            Ok(Materialized { guest: guest.as_bytes().to_vec(), entry: Entry::File { mode: 0o644, size: bytes.len() as u64, sha256 }, host })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_global_environment_is_init_environ_rc_exports() {
        let env = super::global_environment();
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("EXTERNAL_STORAGE"), Some("/sdcard"), "system_server builds its stub volume from it");
        assert_eq!(get("ANDROID_STORAGE"), Some("/storage"));
        assert_eq!(get("ANDROID_ROOT"), Some("/system"));
        assert!(env.iter().all(|(n, v)| !n.is_empty() && !v.is_empty()));
    }
}
