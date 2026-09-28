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
    b"/product/app/LatinIME/LatinIME.apk",
];

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
    for p in KEPT {
        assert!(lean.has(p), "kept: {}", String::from_utf8_lossy(p));
    }
    // A left-out path is a whole name: its prefix alone takes nothing else.
    assert!(lean.has(b"/system/priv-app/Telecom/Telecom.apk"), "Telecom is not TeleService");

    let kiosk = open(Some("kiosk"));
    assert!(!kiosk.has(SYSTEMUI) && !kiosk.has(LAUNCHER), "a kiosk has no SystemUI or launcher");
    assert!(!kiosk.has(TELESERVICE));
    for p in KEPT {
        assert!(kiosk.has(p), "kept: {}", String::from_utf8_lossy(p));
    }
    std::env::remove_var("OMNI_DEVICE_APPS");
}
