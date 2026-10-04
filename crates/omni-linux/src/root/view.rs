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
    /// is inert; uid 0 (root) and 2000 (shell) are never hidden from.
    #[must_use]
    pub fn for_process(profile: Option<&Profile>, package: Option<&str>, uid: u32) -> ProcessView {
        let Some(profile) = profile else { return ProcessView::default() };
        ProcessView { hidden: uid != 0 && uid != 2000 && profile.hidden(package), spoofed: profile.spoofed(package) }
    }
}

/// The base package of a host app process's argv (byte strings): `--package-name=` first, else the
/// base of `--nice-name=` (which can be a process name such as `com.roblox.client:gl`).
#[must_use]
pub fn package_of(argv: &[Vec<u8>]) -> Option<String> {
    let find = |key: &str| argv.iter().find_map(|a| String::from_utf8_lossy(a).strip_prefix(key).map(String::from));
    let name = find("--package-name=").filter(|s| !s.is_empty()).or_else(|| find("--nice-name="))?;
    Some(base_package(&name).to_string())
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
    fn package_of_prefers_package_name_then_nice_name_base() {
        let a = |s: &str| s.as_bytes().to_vec();
        assert_eq!(package_of(&[a("--nice-name=com.r:gl"), a("--package-name=com.r")]).as_deref(), Some("com.r"));
        assert_eq!(package_of(&[a("--nice-name=com.r:gl")]).as_deref(), Some("com.r"));
        assert_eq!(package_of(&[a("x")]), None);
    }
}
