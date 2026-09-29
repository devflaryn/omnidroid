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
/// The soft keyboard: the lean device's, not the kiosk's (its keyboard is the host's).
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
    assert!(lean.has(SYSTEMUI) && lean.has(LAUNCHER) && lean.has(IME), "SystemUI, the launcher and the IME kept");
    for p in KEPT {
        assert!(lean.has(p), "kept: {}", String::from_utf8_lossy(p));
    }
    // A left-out path is a whole name: its prefix alone takes nothing else.
    assert!(lean.has(b"/system/priv-app/Telecom/Telecom.apk"), "Telecom is not TeleService");

    let kiosk = open(Some("kiosk"));
    assert!(!kiosk.has(SYSTEMUI) && !kiosk.has(LAUNCHER), "a kiosk has no SystemUI or launcher");
    assert!(kiosk.has(IME), "a kiosk keeps the IME unless OMNI_KIOSK_IME=0");
    std::env::set_var("OMNI_KIOSK_IME", "0");
    assert!(!open(Some("kiosk")).has(IME), "OMNI_KIOSK_IME=0: no IME");
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
    assert!(with_hardware.has(CAMERA_HAL) && with_hardware.has(FINGERPRINT_HAL) && !with_hardware.has(TELESERVICE));
    std::env::remove_var("OMNI_DEVICE_APPS");
}
