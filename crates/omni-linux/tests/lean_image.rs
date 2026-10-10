//! What the device leaves out of the image (`omni_linux::device::LEAVES_OUT`, `OMNI_DEVICE_APPS`):
//! the apps PackageManager would scan and start are not there at all -- not a file, not a directory
//! entry -- while what it requires stays. One test: the variable is the process's.
mod common;

use omni_linux::vfs::Sysroot;

const TELESERVICE: &[u8] = b"/system/priv-app/TeleService/TeleService.apk";
const SYSTEMUI: &[u8] = b"/system_ext/priv-app/SystemUI/SystemUI.apk";
const LAUNCHER: &[u8] = b"/system_ext/priv-app/Launcher3QuickStep/Launcher3QuickStep.apk";
const BLUETOOTH_FEATURE: &[u8] = b"/vendor/etc/permissions/android.hardware.bluetooth.xml";
/// What PackageManager cannot start without, and what the app needs.
const KEPT: &[&[u8]] = &[
    b"/system/priv-app/PackageInstaller/PackageInstaller.apk",
    b"/system/priv-app/SettingsProvider/SettingsProvider.apk",
    b"/system_ext/priv-app/Settings/Settings.apk",
    b"/system/priv-app/Shell/Shell.apk",
    b"/system/priv-app/NetworkStack/NetworkStack.apk",
    b"/product/app/webview/webview.apk",
];
/// The soft keyboard: in the full image only, unless `OMNI_KIOSK_IME=1` (the device's keyboard is
/// the host's, and an IME would choose its layout: `device::KIOSK_IME`).
const IME: &[u8] = b"/product/app/LatinIME/LatinIME.apk";

fn open(mode: Option<&str>) -> std::sync::Arc<Sysroot> {
    match mode {
        Some(m) => std::env::set_var("OMNI_DEVICE_APPS", m),
        None => std::env::remove_var("OMNI_DEVICE_APPS"),
    }
    let Some(dir) = common::sysroot() else { panic!("no sysroot (tools/make_sysroot.py)") };
    Sysroot::open(&dir).expect("the sysroot")
}

#[test]
fn each_device_mode_leaves_its_apps_out_of_the_image() {
    let full = open(Some("full"));
    for p in [TELESERVICE, SYSTEMUI, LAUNCHER, BLUETOOTH_FEATURE] {
        assert!(full.has(p), "the full image has {}", String::from_utf8_lossy(p));
    }

    // The default: telephony and the rest gone, file and directory; the system UI kept.
    let lean = open(None);
    assert!(!lean.has(TELESERVICE) && !lean.has(b"/system/priv-app/TeleService"), "no TeleService");
    assert!(!lean.children(b"/system/priv-app").iter().any(|n| n == b"TeleService"), "not listed either");
    assert!(!lean.has(BLUETOOTH_FEATURE), "no Bluetooth feature");
    assert!(lean.has(SYSTEMUI) && lean.has(LAUNCHER), "SystemUI and the launcher kept");
    assert!(full.has(IME) && !lean.has(IME), "the full image has the IME, the lean device not");
    for p in KEPT {
        assert!(lean.has(p), "kept: {}", String::from_utf8_lossy(p));
    }
    // A left-out path is a whole name: its prefix alone takes nothing else.
    assert!(lean.has(b"/system/priv-app/Telecom/Telecom.apk"), "Telecom is not TeleService");

    let kiosk = open(Some("kiosk"));
    assert!(!kiosk.has(SYSTEMUI) && !kiosk.has(LAUNCHER), "a kiosk has no SystemUI or launcher");
    assert!(!kiosk.has(IME), "a kiosk has no IME unless OMNI_KIOSK_IME=1");
    std::env::set_var("OMNI_KIOSK_IME", "1");
    assert!(open(Some("kiosk")).has(IME) && open(None).has(IME), "OMNI_KIOSK_IME=1: the IME kept");
    std::env::remove_var("OMNI_KIOSK_IME");
    assert!(!kiosk.has(TELESERVICE));
    for p in KEPT {
        assert!(kiosk.has(p), "kept: {}", String::from_utf8_lossy(p));
    }

    // The hardware the device does not have (`device::HARDWARE_LEFT_OUT`): gone from lean and kiosk
    // with its VINTF declaration and features; kept by `lean-hw`. What the app uses stays.
    const CAMERA_HAL: &[u8] = b"/vendor/etc/init/android.hardware.camera.provider.ranchu.rc";
    const CAMERA_VINTF: &[u8] = b"/vendor/etc/vintf/manifest/android.hardware.camera.provider.ranchu.xml";
    const FINGERPRINT_HAL: &[u8] = b"/vendor/etc/init/android.hardware.biometrics.fingerprint-service.ranchu.rc";
    for device in [&lean, &kiosk] {
        for p in [CAMERA_HAL, CAMERA_VINTF, FINGERPRINT_HAL] {
            assert!(!device.has(p), "left out: {}", String::from_utf8_lossy(p));
        }
        for p in [b"/system/etc/init/cameraserver.rc".as_slice(), b"/vendor/etc/init/android.hardware.audio.service.rc", b"/vendor/etc/init/android.hardware.health-service.example.rc", b"/vendor/etc/init/android.hardware.security.keymint-service.rc"] {
            assert!(device.has(p), "kept: {}", String::from_utf8_lossy(p));
        }
    }
    let with_hardware = open(Some("lean-hw"));
    assert!(with_hardware.has(CAMERA_HAL) && with_hardware.has(FINGERPRINT_HAL) && !with_hardware.has(TELESERVICE) && !with_hardware.has(IME));

    // The overlay that empties the zygote's preloaded resources: in every image by default, out with
    // `OMNI_APP_PRELOAD_RES=1` (the device's own overlay stays either way).
    const NO_PRELOAD: &[u8] = b"/vendor/overlay/omni-nopreload-overlay.apk";
    const DEVICE_OVERLAY: &[u8] = b"/vendor/overlay/omni-device-overlay.apk";
    assert!(lean.has(NO_PRELOAD) && kiosk.has(NO_PRELOAD) && full.has(NO_PRELOAD), "the no-preload overlay by default");
    std::env::set_var("OMNI_APP_PRELOAD_RES", "1");
    let preloading = open(None);
    std::env::remove_var("OMNI_APP_PRELOAD_RES");
    assert!(!preloading.has(NO_PRELOAD) && preloading.has(DEVICE_OVERLAY), "OMNI_APP_PRELOAD_RES=1: the image's lists");
    assert!(open(None).has(NO_PRELOAD), "and back");

    // The idle apps (`device::IDLE_APPS_LEFT_OUT`, `OMNI_DEVICE_IDLE_APPS=out`): in the image by
    // default (the setup disables them once the device is up); with `out`, gone from lean and
    // kiosk -- each listed path one the image has, so a renamed APEX app cannot slip through --
    // while every package PackageManager requires, and what Roblox and the kiosk use, stays.
    let idle = omni_linux::device::IDLE_APPS_LEFT_OUT;
    for p in idle {
        assert!(lean.has(p.as_bytes()) && kiosk.has(p.as_bytes()), "by default the image has {p}");
    }
    assert_eq!(omni_linux::device::idle_apps_key_suffix(), "", "the default device's key is unchanged");
    std::env::set_var("OMNI_DEVICE_IDLE_APPS", "out");
    const REQUIRED: &[&[u8]] = &[
        // PackageManager's required packages: installer and uninstaller, permission controller,
        // SDK sandbox, ext services (the AdExt boot receiver too), the shared library.
        b"/system/priv-app/PackageInstaller/PackageInstaller.apk",
        b"/apex/com.android.permission/priv-app/PermissionController@AE3A.240806.019/PermissionController.apk",
        b"/apex/com.android.adservices/app/SdkSandbox@AE3A.240806.019/SdkSandbox.apk",
        b"/apex/com.android.extservices/priv-app/ExtServices-sminus@AE3A.240806.019/ExtServices-sminus.apk",
        b"/system/app/ExtShared/ExtShared.apk",
        // What the boot and the app use: media storage, the network stack, the browser for links.
        b"/apex/com.android.mediaprovider/priv-app/MediaProvider@AE3A.240806.019/MediaProvider.apk",
        b"/apex/com.android.tethering/priv-app/TetheringNext@AE3A.240806.019/TetheringNext.apk",
        b"/product/app/Browser2/Browser2.apk",
    ];
    for (name, device) in [("lean", open(None)), ("kiosk", open(Some("kiosk")))] {
        for p in idle {
            assert!(!device.has(p.as_bytes()), "{name} with idle apps out: {p} gone");
        }
        for p in KEPT.iter().chain(REQUIRED) {
            assert!(device.has(p), "{name} with idle apps out keeps {}", String::from_utf8_lossy(p));
        }
        // The APEX itself stays (its framework jar and system_server code are the boot's).
        assert!(device.has(b"/apex/com.android.adservices/apex_manifest.pb") && device.has(b"/apex/com.android.devicelock/apex_manifest.pb"));
    }
    // The rule (`device::IDLE_APPS_LEFT_OUT`): never an app of an APEX that ships system_server
    // code -- its service may need the app to exist when it is constructed (DeviceLockService threw
    // without DeviceLockController and took system_server down, runs r-44772). The kept list is the
    // rule's other half, and the DeviceLock case is what the rule catches.
    let full = open(Some("full"));
    let apex_with_service_code = |path: &str| -> Option<String> {
        let name = path.strip_prefix("/apex/")?.split('/').next()?;
        let javalib = format!("/apex/{name}/javalib");
        full.children(javalib.as_bytes())
            .iter()
            .find(|n| n.starts_with(b"service-") && n.ends_with(b".jar"))
            .map(|n| format!("{javalib}/{}", String::from_utf8_lossy(n)))
    };
    for p in idle {
        assert_eq!(apex_with_service_code(p), None, "{p}: its APEX ships system_server code, so it must stay");
    }
    assert!(
        apex_with_service_code("/apex/com.android.devicelock/priv-app/DeviceLockController@AE3A.240806.019").is_some(),
        "the rule catches the app whose absence ended system_server"
    );
    let kept = omni_linux::device::IDLE_APPS_KEPT;
    assert!(kept.iter().all(|k| !idle.contains(k)), "kept and left out are apart");
    assert_eq!(omni_linux::device::IDLE_APP_PACKAGES_LEFT_OUT.len(), idle.len(), "a package for each path");
    for (name, device) in [("lean", open(None)), ("kiosk", open(Some("kiosk")))] {
        for k in kept {
            assert!(device.has(k.as_bytes()), "{name} with idle apps out keeps {k}");
        }
    }
    assert_eq!(omni_linux::device::idle_apps_key_suffix(), "-idleout", "a saved device without them is another");
    assert!(open(Some("full")).has(idle[0].as_bytes()), "the full image is the image as it is");
    std::env::remove_var("OMNI_DEVICE_IDLE_APPS");
    std::env::remove_var("OMNI_DEVICE_APPS");
}
