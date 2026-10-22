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
    // The rooted device's module install and scripts: inert unless /data/adb/omni/enabled exists.
    ("/vendor/etc/init/omni_root.rc", include_bytes!("../device/vendor/etc/init/omni_root.rc")),
    ("/vendor/bin/omni_root.sh", include_bytes!("../device/vendor/bin/omni_root.sh")),
    // No background app processes kept: ActivityManager's cached-process limit (the script says
    // why; `persist.omni.cached_processes`).
    ("/vendor/etc/init/omni_lean.rc", include_bytes!("../device/vendor/etc/init/omni_lean.rc")),
    ("/vendor/bin/omni_lean.sh", include_bytes!("../device/vendor/bin/omni_lean.sh")),
    // The boot receivers a device of this kind has no use for, disabled once per device and for
    // each user the pool makes (the script says which and why; `persist.omni.quiet_boot=0`: not).
    ("/vendor/etc/init/omni_quiet.rc", include_bytes!("../device/vendor/etc/init/omni_quiet.rc")),
    ("/vendor/bin/omni_quiet.sh", include_bytes!("../device/vendor/bin/omni_quiet.sh")),
    // The boot's own settings: no dexopt at install (the file says why).
    ("/vendor/etc/init/omni_boot.rc", include_bytes!("../device/vendor/etc/init/omni_boot.rc")),
    // Every app process's entry point and a spare app process's (`device/src/spare/`;
    // `crate::zygote`'s `app_command` and spare).
    ("/vendor/framework/omni-spare.jar", include_bytes!("../device/vendor/framework/omni-spare.jar")),
    // The framework's configuration as this device has it: a static overlay (RRO), as a vendor
    // partition carries one (`device/src/overlay/`; the values file says why each is there).
    ("/vendor/overlay/omni-device-overlay.apk", include_bytes!("../device/vendor/overlay/omni-device-overlay.apk")),
    // The kiosk device's home, in place of Settings' FallbackHome (`KIOSK_HOME`, `device/src/home/`).
    ("/vendor/app/OmniHome/OmniHome.apk", include_bytes!("../device/vendor/app/OmniHome/OmniHome.apk")),
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
    // Not the browser (`/product/app/Browser2`): an app that opens a web page with ACTION_VIEW
    // crashes when nothing on the device takes it -- Roblox did, its in-game key page
    // (`ActivityNotFoundException`, the app ended mid-game, 2026-10-02). It runs only when opened.
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

/// **The image's idle apps that can be left out** (`OMNI_DEVICE_IDLE_APPS=out`; off by default:
/// `disable`, the device's setup disables them once it is up, as before). Of the 18 apps
/// `tests/r_app.rs`' setup disabled after the first boot (`IDLE_APPS`: `cmd package
/// disable-user`, ~7 s), until then scanned, started and sent the boot broadcast like any other:
/// PackageManager never scans these.
///
/// **The rule** (`tests/lean_image.rs` checks its first part): an app is left out only if no
/// system service needs it to exist. A disabled package is still found by name, an absent one is
/// not -- and a system service that resolves its app when it is constructed then throws, which
/// ends system_server: `DeviceLockService` ("Service with android.app.action.
/// DEVICE_LOCK_CONTROLLER_SERVICE not found", `DeviceLockServiceImpl.<init>`, every boot with
/// DeviceLockController left out, runs r-44772, 2026-10-09). So:
/// 1. never an app of an APEX that ships system_server code (`javalib/service-*.jar`): its service
///    is the app's counterpart -- DeviceLock (the crash), HealthFitness, OnDevicePersonalization
///    and FederatedCompute, AdServices, RKP all kept;
/// 2. never an app system_server binds through a framework API (`services.jar`'s own code, AOSP 15
///    sources read): KeyChain kept (`KeyChain.bindAsUser` from DevicePolicyManagerService, and
///    something binds `KeyChainService` at user unlock, run r-44396); WallpaperBackup kept
///    (`services.jar` names it; the backup service's user setup is not seen in any boot here);
/// 3. what is left out had only lookups that handle the absence (below, each).
///
/// Two of the 18 are already not on the lean device (`com.android.cellbroadcastreceiver`:
/// [`LEAVES_OUT`]'s `CellBroadcastLegacyApp`) or not in the image (`com.android.ext.
/// adservices.api`). Of the 5 app processes the 18 started before Roblox on a fresh boot (run
/// r-44396), DocumentsUI and ExternalStorageProvider are gone; DeviceLockController (twice) and
/// KeyChain stay.
pub const IDLE_APPS_LEFT_OUT: &[&str] = &[
    // com.android.cellbroadcastreceiver.module, com.android.cellbroadcastservice: the cellbroadcast
    // APEX has no system_server code; their only binder, telephony, is not on the device.
    "/apex/com.android.cellbroadcast/priv-app/CellBroadcastApp@AE3A.240806.019",
    "/apex/com.android.cellbroadcast/priv-app/CellBroadcastServiceModule@AE3A.240806.019",
    // com.android.nfc: the nfcservices APEX has no system_server code (framework-nfc only), and the
    // device declares no NFC.
    "/apex/com.android.nfcservices/priv-app/NfcNciApex@AE3A.240806.019",
    // com.android.statementservice: app-link verification's agent; PackageManager answers a device
    // without one with `DomainVerificationProxyUnavailable` (in this image's services.jar), and the
    // r-44772 boots passed PackageManager's start and DomainVerificationService without it.
    "/system/priv-app/StatementService",
    // com.android.documentsui: services.jar names it nowhere; it ran only for its own pre-boot
    // receiver.
    "/system/priv-app/DocumentsUI",
    // com.android.managedprovisioning: DevicePolicyManagerService names it only on the
    // provisioning paths (a broadcast to it, cross-profile filters of profiles there are none of).
    "/system/priv-app/ManagedProvisioning",
    // com.android.externalstorage: StorageManagerService looks its provider up and handles none
    // (`getProviderInfo(EXTERNAL_STORAGE_PROVIDER_AUTHORITY)`, `if (provider != null)`); it ran
    // only for its own mount receiver.
    "/system/priv-app/ExternalStorageProvider",
    // com.android.providers.userdictionary: the IME's dictionary (no IME here); services.jar names
    // it nowhere.
    "/system/priv-app/UserDictionaryProvider",
];

/// The packages of [`IDLE_APPS_LEFT_OUT`], in its order: what a setup that disables the idle
/// apps once the device is up leaves alone when they are not in the image.
pub const IDLE_APP_PACKAGES_LEFT_OUT: &[&str] = &[
    "com.android.cellbroadcastreceiver.module",
    "com.android.cellbroadcastservice",
    "com.android.nfc",
    "com.android.statementservice",
    "com.android.documentsui",
    "com.android.managedprovisioning",
    "com.android.externalstorage",
    "com.android.providers.userdictionary",
];

/// Idle apps that must stay in the image (see [`IDLE_APPS_LEFT_OUT`]'s rule): what a system
/// service needs to exist. Listed so a later edit cannot leave one out unnoticed
/// (`tests/lean_image.rs`).
pub const IDLE_APPS_KEPT: &[&str] = &[
    "/apex/com.android.devicelock/priv-app/DeviceLockController@AE3A.240806.019",
    "/apex/com.android.healthfitness/priv-app/HealthConnectController@AE3A.240806.019",
    "/apex/com.android.ondevicepersonalization/priv-app/OnDevicePersonalization@AE3A.240806.019",
    "/apex/com.android.ondevicepersonalization/app/FederatedCompute@AE3A.240806.019",
    "/apex/com.android.adservices/priv-app/AdServicesApk@AE3A.240806.019",
    "/apex/com.android.rkpd/priv-app/rkpdapp@AE3A.240806.019",
    "/system/app/KeyChain",
    "/system/app/WallpaperBackup",
];

/// Whether the idle apps are left out of the image (`OMNI_DEVICE_IDLE_APPS=out`) rather than
/// disabled once the device is up (`disable`, the default). Not of the `full` image.
#[must_use]
pub fn idle_apps_out() -> bool {
    std::env::var("OMNI_DEVICE_IDLE_APPS").as_deref() == Ok("out") && std::env::var("OMNI_DEVICE_APPS").as_deref() != Ok("full")
}

/// What a saved device's name carries when the idle apps are left out (`-idleout`), else
/// nothing: a device made with them is another device (its package list, its data).
#[must_use]
pub fn idle_apps_key_suffix() -> &'static str {
    if idle_apps_out() {
        "-idleout"
    } else {
        ""
    }
}

/// **The image's overlays that never apply here**, left out of every lean device
/// (`OMNI_DEVICE_OVERLAYS=all` keeps them): the emulator's device skins (`EmulationPixel*` for the
/// framework, `SystemUIEmulationPixel*` for SystemUI), Developer options' cutout emulations, an
/// optional font and the notes role -- all disabled (`cmd overlay list`: `[ ]`, warm device
/// 2026-10-22) -- and the telephony apps' overlays, whose targets [`LEAVES_OUT`] drops. 54 of the
/// kiosk device's 134 packages with [`KIOSK_INERT_OVERLAYS`], each scanned by PackageManager and
/// tracked by OverlayManager at every boot, and held in system_server.
pub const INERT_OVERLAYS: &[&str] = &[
    "/product/overlay/EmulationPixel2XL",
    "/product/overlay/EmulationPixel3",
    "/product/overlay/EmulationPixel3XL",
    "/product/overlay/EmulationPixel3a",
    "/product/overlay/EmulationPixel3aXL",
    "/product/overlay/EmulationPixel4",
    "/product/overlay/EmulationPixel4XL",
    "/product/overlay/EmulationPixel4a",
    "/product/overlay/EmulationPixel5",
    "/product/overlay/EmulationPixel6",
    "/product/overlay/EmulationPixel6Pro",
    "/product/overlay/EmulationPixel6a",
    "/product/overlay/EmulationPixel7",
    "/product/overlay/EmulationPixel7Pro",
    "/product/overlay/EmulationPixel7a",
    "/product/overlay/EmulationPixel8",
    "/product/overlay/EmulationPixel8Pro",
    "/product/overlay/EmulationPixel8a",
    "/product/overlay/EmulationPixelFold",
    "/product/overlay/SystemUIEmulationPixel3",
    "/product/overlay/SystemUIEmulationPixel3XL",
    "/product/overlay/SystemUIEmulationPixel3a",
    "/product/overlay/SystemUIEmulationPixel3aXL",
    "/product/overlay/SystemUIEmulationPixel4",
    "/product/overlay/SystemUIEmulationPixel4XL",
    "/product/overlay/SystemUIEmulationPixel4a",
    "/product/overlay/SystemUIEmulationPixel5",
    "/product/overlay/SystemUIEmulationPixel6",
    "/product/overlay/SystemUIEmulationPixel6Pro",
    "/product/overlay/SystemUIEmulationPixel6a",
    "/product/overlay/SystemUIEmulationPixel7",
    "/product/overlay/SystemUIEmulationPixel7Pro",
    "/product/overlay/SystemUIEmulationPixel7a",
    "/product/overlay/SystemUIEmulationPixel8",
    "/product/overlay/SystemUIEmulationPixel8Pro",
    "/product/overlay/SystemUIEmulationPixel8a",
    "/product/overlay/SystemUIEmulationPixelFold",
    "/product/overlay/DisplayCutoutEmulationCorner",
    "/product/overlay/DisplayCutoutEmulationDouble",
    "/product/overlay/DisplayCutoutEmulationEmu01",
    "/product/overlay/DisplayCutoutEmulationHole",
    "/product/overlay/DisplayCutoutEmulationTall",
    "/product/overlay/DisplayCutoutEmulationWaterfall",
    "/product/overlay/FontNotoSerifSource",
    "/product/overlay/NotesRoleEnabled",
    "/product/overlay/TeleService__emulator__auto_generated_characteristics_rro.apk",
    "/product/overlay/TelephonyProvider__emulator__auto_generated_characteristics_rro.apk",
    "/vendor/overlay/CarrierConfig__sdk_phone64_arm64__auto_generated_rro_vendor.apk",
    "/vendor/overlay/TeleService__sdk_phone64_arm64__auto_generated_rro_vendor.apk",
];

/// The overlays of what the kiosk device does not have: the navigation bar's modes (no SystemUI,
/// no bar; all four disabled) and SystemUI's own vendor overlay.
pub const KIOSK_INERT_OVERLAYS: &[&str] = &[
    "/product/overlay/NavigationBarMode2Button",
    "/product/overlay/NavigationBarMode3Button",
    "/product/overlay/NavigationBarModeGestural",
    "/product/overlay/TransparentNavigationBar",
    "/vendor/overlay/SystemUI__sdk_phone64_arm64__auto_generated_rro_vendor.apk",
];

/// Whether [`INERT_OVERLAYS`] are left out: a lean device's, unless `OMNI_DEVICE_OVERLAYS=all`.
#[must_use]
pub fn inert_overlays_out() -> bool {
    std::env::var("OMNI_DEVICE_OVERLAYS").as_deref() != Ok("all") && std::env::var("OMNI_DEVICE_APPS").as_deref() != Ok("full")
}

/// What a saved device's name carries when its image has no inert overlays (`-ol`), else nothing.
#[must_use]
pub fn overlays_key_suffix() -> &'static str {
    if inert_overlays_out() {
        "-ol"
    } else {
        ""
    }
}

/// **The kiosk device's own home** (`/vendor/app/OmniHome`, `device/src/home/`): a black window,
/// where Settings' FallbackHome was the home of a device without a launcher. FallbackHome brought
/// up all of Settings -- a host process of ~220 MiB private on every warm device, and one more for
/// every pool user (its own home), for a window the composer never shows -- and its statsd retries
/// (`crate::socket`'s sink). Only the kiosk device has it (the others have Launcher3, and two homes
/// would ask which); `OMNI_KIOSK_HOME=0` leaves it out (FallbackHome again).
pub const KIOSK_HOME: &str = "/vendor/app/OmniHome";

/// [`KIOSK_HOME`]'s package.
pub const KIOSK_HOME_PACKAGE: &str = "com.omnidroid.home";

/// **The kiosk home starts in a spare app process** (`crate::zygote`'s spare; `OMNI_HOME_SPARE=0`:
/// not). boot_completed waits for the home to draw, and a home started cold spent ~0.8 of its ~2.5 s
/// on its host process and ART coming up (run 2026-10-22); a spare started as soon as the zygote
/// serves is up by then. Only where spares are kept (`OMNI_APP_SPARE=1`: the warm device, the pool).
#[must_use]
pub fn home_spare() -> bool {
    std::env::var("OMNI_HOME_SPARE").as_deref() != Ok("0")
}

/// Whether this device has [`KIOSK_HOME`].
#[must_use]
pub fn kiosk_home() -> bool {
    std::env::var("OMNI_DEVICE_APPS").as_deref() == Ok("kiosk") && std::env::var("OMNI_KIOSK_HOME").as_deref() != Ok("0")
}

/// What the name of a saved device (`kiosk` or not) carries when it has [`KIOSK_HOME`] (`-kh`),
/// else nothing: a device saved with FallbackHome as its home is another device.
#[must_use]
pub fn kiosk_home_key_suffix(kiosk: bool) -> &'static str {
    if kiosk && std::env::var("OMNI_KIOSK_HOME").as_deref() != Ok("0") {
        "-kh"
    } else {
        ""
    }
}

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
/// The soft keyboard ([`KIOSK_IME`]) is left out too, by default -- of the lean devices as well
/// (see [`apps_left_out`]); `OMNI_KIOSK_IME=1` keeps it.
pub const KIOSK_LEAVES_OUT: &[&str] = &["/system_ext/priv-app/SystemUI", "/system_ext/priv-app/Launcher3QuickStep"];
/// **The soft keyboard (LatinIME), left out of the `lean`, `lean-hw` and `kiosk` devices by
/// default**; `OMNI_KIOSK_IME=1` keeps it (and `full` is the image as it is).
///
/// The device's keyboard is the host's, which a text field takes without an input method, and the
/// project owner wants no soft keyboard to appear anywhere (2026-10-07). Two more reasons: LatinIME
/// is a host process of its own (~120 MiB, run 2026-09-29) that every key visits first
/// (`ImeInputStage`); and **an IME decides the keyboard's layout** -- Android 14+'s
/// KeyboardLayoutManager lays a physical-keyboard layout over the device's own map, chosen from the
/// current IME subtype's locale (LatinIME's Turkish subtype on a `tr-TR` device: Turkish Q, whatever
/// the host types with). With no IME there is no subtype and no overlay, and the device keyboard's
/// own map -- made from the host's layout (`crate::keymap`) -- is the one used.
///
/// History, kept: this was `OMNI_KIOSK_IME=0`'s opt-in, not the default, because in PS99 without the
/// IME the first held key took 347-425 ms and a few more keys ended with the app force-finished
/// (run 2026-09-29, "runc"), cause not pinned. It is the default now at the owner's request; that
/// run's symptom is to be re-checked on the run this change was made for (2026-10-07), not assumed
/// gone.
pub const KIOSK_IME: &str = "/product/app/LatinIME";

/// **Services this device does not run**, with the properties each sets once it is done -- which
/// init.rc waits for (`wait_for_prop`), so they are set at once instead.
///
/// `odsign`, the on-device signing daemon, checks the ART artifacts odrefresh compiled into
/// `/data` against a key in keystore2. Here the boot image and the system server's code are the
/// image's own (`/system/framework/**/*.odex`), and keystore2 is not up that early: it waited 5 s
/// for keystore2 at every boot, gave up ("Could not create keystore key") and set both
/// properties on its way out (2026-10-01) -- while init waited on `odsign.key.done`. The outcome is
/// the same without it, 5 s sooner. `OMNI_KEEP_SERVICES=odsign` runs it.
///
/// `apexd-bootstrap` (`apexd --bootstrap`, init's first `exec_start`, before /data): it activates the
/// bootstrap APEXes from loop and device-mapper devices, which this kernel does not have, and failed
/// at every boot -- "Failed LOOP_CTL_ADD", "Failed to open device-mapper", then "Failed to create
/// directory /apex/sharedlibs: Read-only file system", exit 1 -- having set no property and written
/// nothing; the flattened APEXes are in the sysroot, and the `apexd` service that follows is the
/// one whose `apexd.status` init waits for. 199 ms of init's critical path (2026-10-22).
pub const SERVICES_LEFT_OUT: &[(&str, &[(&str, &str)])] = &[("odsign", &[("odsign.key.done", "1"), ("odsign.verification.done", "1")]), ("apexd-bootstrap", &[])];

/// The properties `service` would have set, if the device leaves it out ([`SERVICES_LEFT_OUT`],
/// less `OMNI_KEEP_SERVICES`, comma-separated).
#[must_use]
pub fn service_left_out(service: &str) -> Option<&'static [(&'static str, &'static str)]> {
    let kept = std::env::var("OMNI_KEEP_SERVICES").unwrap_or_default();
    if kept.split(',').any(|k| k.trim() == service) {
        return None;
    }
    SERVICES_LEFT_OUT.iter().find(|(name, _)| *name == service).map(|(_, props)| *props)
}

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
/// kept) or `kiosk` (lean, and [`KIOSK_LEAVES_OUT`]); each but `full` without the soft keyboard
/// unless `OMNI_KIOSK_IME=1` ([`KIOSK_IME`]), and each but `full` without the idle apps when
/// `OMNI_DEVICE_IDLE_APPS=out` ([`IDLE_APPS_LEFT_OUT`]). Read by each host process of an
/// instance alike (the variable is inherited), so they all see one image.
///
/// The device's own boot settings can be left out too, to measure what each is worth:
/// `OMNI_DEVICE_OVERLAY=0` (the framework overlay: the boot waits for a wallpaper again) and
/// `OMNI_DEVICE_BOOT_RC=0` (the device's boot settings: installs dexopt again).
#[must_use]
pub fn left_out() -> Vec<&'static str> {
    let mut out = apps_left_out();
    if std::env::var("OMNI_DEVICE_OVERLAY").as_deref() == Ok("0") {
        out.push("/vendor/overlay/omni-device-overlay.apk");
    }
    if std::env::var("OMNI_DEVICE_BOOT_RC").as_deref() == Ok("0") {
        out.push("/vendor/etc/init/omni_boot.rc");
    }
    // An app's host process has no preloaded-classes list (`OMNI_APP_PRELOAD=1` gives it one), so
    // its WrapperInit preloads none of the zygote's classes (`ZygoteInit.preloadClasses` finds no
    // file) and the app loads only the classes it uses. On a device a zygote preloads them once and
    // every app forks with them; here each app host process starts alone and preloaded them all
    // (`PreloadClasses` 2.9-4.2 s a process), on the one thread an app's start runs on. Measured on
    // one warm device, launch against launch (2026-10-01, `tools/warm_ab.py`): the probe app's
    // first frame 1.8 s sooner (median of 8 ABBA pairs, 7 of them faster; 13.9 -> 12.1 s),
    // Roblox's 2.1 and 3.7 s sooner (2 pairs). system_server keeps its preload.
    if std::env::var("OMNI_LINUX_APP").as_deref() == Ok("1") && std::env::var("OMNI_APP_PRELOAD").as_deref() != Ok("1") {
        out.push("/system/etc/preloaded-classes");
    }
    out
}

/// What the device mode leaves out of the image; the soft keyboard with it unless `OMNI_KIOSK_IME=1`
/// (see [`KIOSK_IME`]).
fn apps_left_out() -> Vec<&'static str> {
    let ime = (std::env::var("OMNI_KIOSK_IME").as_deref() != Ok("1")).then_some(KIOSK_IME);
    let idle: &[&str] = if idle_apps_out() { IDLE_APPS_LEFT_OUT } else { &[] };
    // The kiosk home is the device's own file, not the image's: out of every other device.
    let home = (!kiosk_home()).then_some(KIOSK_HOME);
    let ime = ime.into_iter().chain(home);
    let overlays: &[&str] = if inert_overlays_out() { INERT_OVERLAYS } else { &[] };
    let kiosk_overlays: &[&str] = if inert_overlays_out() { KIOSK_INERT_OVERLAYS } else { &[] };
    match std::env::var("OMNI_DEVICE_APPS").as_deref() {
        Ok("full") => vec![KIOSK_HOME],
        Ok("kiosk") => LEAVES_OUT.iter().chain(HARDWARE_LEFT_OUT).chain(KIOSK_LEAVES_OUT).chain(idle).chain(overlays).chain(kiosk_overlays).copied().chain(ime).collect(),
        Ok("lean-hw") => LEAVES_OUT.iter().chain(idle).chain(overlays).copied().chain(ime).collect(),
        _ => LEAVES_OUT.iter().chain(HARDWARE_LEFT_OUT).chain(idle).chain(overlays).copied().chain(ime).collect(),
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

    /// Every overlay file `device/SHA256SUMS` pins is embedded as pinned -- the spare's jar among
    /// them, whose dex holds the class every app process starts under (`crate::zygote`'s
    /// `app_command`) and the spare's: a stale jar left every app unable to start, no test failing.
    #[test]
    fn the_embedded_files_are_the_ones_sha256sums_pins() {
        use sha2::{Digest, Sha256};
        let mut pinned = Vec::new();
        for (sum, rel) in include_str!("../device/SHA256SUMS").lines().filter_map(|l| l.split_once(" *")) {
            let Some((path, bytes)) = super::FILES.iter().find(|(p, _)| p.strip_prefix('/') == Some(rel)) else { continue };
            assert_eq!(format!("{:x}", Sha256::digest(bytes)), sum, "{path} is not the file device/SHA256SUMS pins");
            pinned.push(*path);
        }
        assert!(pinned.contains(&"/vendor/framework/omni-spare.jar"), "{pinned:?}");
        let (_, jar) = super::FILES.iter().find(|(p, _)| *p == "/vendor/framework/omni-spare.jar").unwrap();
        let at = std::env::temp_dir().join(format!("omni-spare-{}.jar", std::process::id()));
        std::fs::write(&at, jar).unwrap();
        let dex = omni_apk::Apk::open(&at).and_then(|a| a.read_named("classes.dex"));
        let _ = std::fs::remove_file(&at);
        let dex = dex.unwrap();
        for class in [&b"Lcom/omnidroid/spare/App;"[..], b"Lcom/omnidroid/spare/Spare;"] {
            assert!(dex.windows(class.len()).any(|w| w == class), "{} not in the jar's dex", String::from_utf8_lossy(class));
        }
    }
}
