//! The per-process view of the root profile: whether a process is hidden from root and whether it
//! gets the emulator spoof. Decided from the host-only profile and the app's package only.
use super::profile::{base_package, Profile};

/// What one process sees of the rooted device. `Default` (neither) is every non-rooted path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessView {
    /// Root artifacts (su, modules, Magisk files/props) are hidden from this process.
    pub hidden: bool,
    /// Emulator artifacts are spoofed for this process.
    pub spoofed: bool,
}

impl ProcessView {
    /// The view for a process of `package` (a package or process name) running as `uid`. No profile
    /// is inert; uid 0 (root) and 2000 (shell) are never hidden from, and whitelist mode spares uids < 10000.
    #[must_use]
    pub fn for_process(profile: Option<&Profile>, package: Option<&str>, uid: u32) -> ProcessView {
        let Some(profile) = profile else { return ProcessView::default() };
        // DenyList hides at any uid but root/shell; whitelist mode hides only real apps (uid >= 10000),
        // never system/privileged uids such as system_server (1000).
        let hidden = uid != 0
            && uid != 2000
            && (profile.denylisted(package) || (uid >= 10000 && profile.whitelist_hidden(package)));
        // Emulator spoofing (Pixel 8 props, hidden ranchu/goldfish/qemu files) is only for the app
        // whose anti-tamper we defeat -- the same processes root is hidden from -- never the
        // system's own services. Spoofing servicemanager/system_server/the HALs would hide the
        // device's real `ranchu`-named VINTF fragments from the boot that needs them: the audio
        // HAL's manifest is `android.hardware.audio@7.1-impl.ranchu.xml`, so hiding it leaves
        // `IDevicesFactory` undeclared, audioserver finds no HAL and crashes, and AudioService
        // deadlocks system_server until the watchdog kills the boot.
        let spoofed = hidden && profile.spoofed(package);
        ProcessView { hidden, spoofed }
    }
}

/// The base package of a host app process's argv (byte strings): the base of the existing
/// `--nice-name=` (`com.roblox.client:gl` -> `com.roblox.client`); `None` when absent or empty.
/// Nothing is added to the guest `app_process64` argv.
#[must_use]
pub fn package_of(argv: &[Vec<u8>]) -> Option<String> {
    let name = argv.iter().find_map(|a| String::from_utf8_lossy(a).strip_prefix("--nice-name=").map(String::from))?;
    (!name.is_empty()).then(|| base_package(&name).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn view_matches_base_package_and_tolerates_missing_name() {
        let p = crate::root::Profile::parse("root=1\ndenylist=com.roblox.client\nmodule=emu-hide\n");
        let v = ProcessView::for_process(Some(&p), Some("com.roblox.client:gl"), 10234);
        assert!(v.hidden && v.spoofed);
        // a process with no name (system_server, a daemon) on this profile: not denylisted -> not hidden
        let v2 = ProcessView::for_process(Some(&p), None, 1000);
        assert!(!v2.hidden);
        // no profile at all -> inert
        let v3 = ProcessView::for_process(None, Some("com.roblox.client"), 10234);
        assert!(!v3.hidden && !v3.spoofed);
    }
    #[test]
    fn whitelist_allows_su_uids() {
        let p = crate::root::Profile::parse("root=1\nshamiko=whitelist\n");
        assert!(ProcessView::for_process(Some(&p), Some("com.x"), 10234).hidden);
        assert!(!ProcessView::for_process(Some(&p), Some("com.x"), 0).hidden); // root never hidden from itself
        assert!(!ProcessView::for_process(Some(&p), Some("com.x"), 2000).hidden); // shell
    }
    #[test]
    fn whitelist_spares_system_uids_but_hides_apps() {
        let p = crate::root::Profile::parse("root=1
shamiko=whitelist
su=com.allowed
");
        assert!(!ProcessView::for_process(Some(&p), Some("com.x"), 1000).hidden); // system_server
        assert!(ProcessView::for_process(Some(&p), Some("com.x"), 10234).hidden);
        assert!(!ProcessView::for_process(Some(&p), Some("com.allowed"), 10234).hidden);
        // an explicit denylist entry still hides below the app uid range
        let d = crate::root::Profile::parse("root=1
denylist=com.x
shamiko=whitelist
");
        assert!(ProcessView::for_process(Some(&d), Some("com.x"), 1000).hidden);
    }
    #[test]
    fn package_of_is_the_base_of_nice_name() {
        let a = |s: &str| s.as_bytes().to_vec();
        let argv = [a("/system/bin"), a("--application"), a("--nice-name=com.roblox.client:gl"), a("com.android.internal.os.WrapperInit")];
        assert_eq!(package_of(&argv).as_deref(), Some("com.roblox.client"));
        assert_eq!(package_of(&[a("--application"), a("x")]), None);
        assert_eq!(package_of(&[a("--nice-name=")]), None);
    }
}
