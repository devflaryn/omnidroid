/// The root profile parsed from a per-instance root config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub rooted: bool,
    pub magisk_code: u32,
    pub module_ids: Vec<String>,
    pub su: SuPolicy,
    pub denylist: Vec<String>,
    pub shamiko: Shamiko,
}

/// The credentials an elevation grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    pub caps: u64,
}

/// The su policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuPolicy {
    All,
    Packages(Vec<String>),
}

/// Shamiko (hide) mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shamiko {
    Off,
    On,
    Whitelist,
}

impl Profile {
    /// Parse comma-separated values, dropping empty entries.
    fn parse_comma_list(s: &str) -> Vec<String> {
        s.split(',').map(|item| item.trim().to_string()).filter(|s| !s.is_empty()).collect()
    }

    /// Parse a root profile from text (line-oriented key=value format).
    pub fn parse(text: &str) -> Self {
        let mut rooted = false;
        let mut magisk_code = 0;
        let mut module_ids = Vec::new();
        let mut su = SuPolicy::Packages(vec![]); // Default: empty packages (only root/shell)
        let mut denylist = Vec::new();
        let mut shamiko = Shamiko::Off;

        for line in text.lines() {
            let line = line.trim();

            // Skip blank lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse key=value
            if let Some((key, value)) = line.split_once('=') {
                match key.trim() {
                    "root" => {
                        rooted = value.trim() == "1";
                    }
                    "magisk" => {
                        magisk_code = value.trim().parse().unwrap_or(0);
                    }
                    "module" => {
                        let id = value.trim().to_string();
                        if !id.is_empty() {
                            module_ids.push(id);
                        }
                    }
                    "su" => {
                        let val = value.trim();
                        if val == "all" {
                            su = SuPolicy::All;
                        } else {
                            // Parse comma-separated packages (empty → Packages(vec![]))
                            su = SuPolicy::Packages(Self::parse_comma_list(val));
                        }
                    }
                    "denylist" => {
                        // Parse comma-separated packages
                        denylist.extend(Self::parse_comma_list(value));
                    }
                    "shamiko" => {
                        let val = value.trim();
                        shamiko = match val {
                            "1" | "on" => Shamiko::On,
                            "whitelist" => Shamiko::Whitelist,
                            _ => Shamiko::Off,
                        };
                    }
                    _ => {}
                }
            }
        }

        Profile { rooted, magisk_code, module_ids, su, denylist, shamiko }
    }

    /// Serialize the profile to its canonical text form.
    pub fn serialize(&self) -> String {
        let mut lines = Vec::new();

        if self.rooted {
            lines.push("root=1".to_string());
        }

        if self.magisk_code > 0 {
            lines.push(format!("magisk={}", self.magisk_code));
        }

        for module in &self.module_ids {
            lines.push(format!("module={}", module));
        }

        match &self.su {
            SuPolicy::All => lines.push("su=all".to_string()),
            SuPolicy::Packages(pkgs) => {
                // Serialize as one line: su=<comma-joined> (empty list → su=)
                lines.push(format!("su={}", pkgs.join(",")));
            }
        }

        for deny in &self.denylist {
            lines.push(format!("denylist={}", deny));
        }

        match self.shamiko {
            Shamiko::On => lines.push("shamiko=on".to_string()),
            Shamiko::Whitelist => lines.push("shamiko=whitelist".to_string()),
            Shamiko::Off => {}
        }

        lines.join("\n") + "\n"
    }

    /// Whether the instance is rooted.
    pub fn is_rooted(&self) -> bool {
        self.rooted
    }

    /// Whether a uid is allowed to use su.
    pub fn su_allowed(&self, uid: u32) -> bool {
        uid == 0 || uid == 2000 || matches!(self.su, SuPolicy::All)
    }

    /// The credentials `caller_uid` gets by elevating to `target_uid`; `EACCES` when su is not allowed.
    pub fn elevation(&self, caller_uid: u32, target_uid: u32) -> Result<Ids, i32> {
        if !self.su_allowed(caller_uid) {
            return Err(crate::errno::EACCES.0);
        }
        if target_uid == 0 {
            Ok(Ids { uid: 0, gid: 0, groups: vec![0], caps: crate::sys::ALL_CAPS })
        } else {
            Ok(Ids { uid: target_uid, gid: target_uid, groups: vec![], caps: 0 })
        }
    }

    /// The module ids, in order.
    pub fn modules(&self) -> &[String] {
        &self.module_ids
    }

    /// The Magisk version code.
    pub fn magisk_version_code(&self) -> u32 {
        self.magisk_code
    }

    /// Whether a package is hidden (R2+).
    pub fn hidden(&self, _package: Option<&str>) -> bool {
        false
    }
}

/// File name of the host-only profile, directly in the instance directory (like `.omni-binds`).
/// It is deliberately NOT under `<instance>/data`, which the guest can write: a guest must never be
/// able to make its own device rooted.
pub const PROFILE_FILE: &str = ".omni-root-profile";

#[derive(Default)]
struct ProfileCell {
    /// The file's mtime when last read, when that was checked, and the parsed profile (rooted only).
    state: parking_lot::Mutex<(Option<std::time::SystemTime>, Option<std::time::Instant>, Option<std::sync::Arc<Profile>>)>,
}

impl Profile {
    /// The rooted profile of the instance at `instance`, read from `<instance>/.omni-root-profile`:
    /// `None` when the file is absent or the device is not rooted. One cache per instance
    /// directory, re-read when the file's mtime changes (checked at most once a second), as
    /// [`crate::vfs::Binds::of`] does.
    #[must_use]
    pub fn of(instance: &std::path::Path) -> Option<std::sync::Arc<Profile>> {
        use std::collections::HashMap;
        use std::sync::{Arc, OnceLock};
        static CELLS: OnceLock<parking_lot::Mutex<HashMap<std::path::PathBuf, Arc<ProfileCell>>>> = OnceLock::new();
        let cell = Arc::clone(CELLS.get_or_init(Default::default).lock().entry(instance.to_path_buf()).or_default());
        let file = instance.join(PROFILE_FILE);
        let mut st = cell.state.lock();
        let now = std::time::Instant::now();
        if st.1.is_some_and(|at| now.duration_since(at) < std::time::Duration::from_secs(1)) {
            return st.2.clone();
        }
        st.1 = Some(now);
        let modified = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
        if modified.is_some() && modified == st.0 {
            return st.2.clone();
        }
        st.0 = modified;
        st.2 = std::fs::read_to_string(&file).ok().map(|t| Profile::parse(&t)).filter(Profile::is_rooted).map(Arc::new);
        st.2.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_grants_root_and_refuses_a_disallowed_uid() {
        let p = Profile::parse("root=1
su=all
");
        let ids = p.elevation(10234, 0).expect("allowed");
        assert_eq!(ids.uid, 0);
        assert_eq!(ids.caps, crate::sys::ALL_CAPS);
        let p = Profile::parse("root=1
su=com.one
");
        assert_eq!(p.elevation(10234, 0).unwrap_err(), crate::errno::EACCES.0);
        // shell may still become root
        assert!(p.elevation(2000, 0).is_ok());
    }

    #[test]
    fn round_trips_and_reads_fields() {
        let text = "root=1\nmagisk=29000\nmodule=zygisk-frida\nmodule=emu-hide\nsu=all\ndenylist=com.roblox.client\nshamiko=whitelist\n";
        let p = Profile::parse(text);
        assert!(p.is_rooted());
        assert_eq!(p.magisk_version_code(), 29000);
        assert_eq!(p.modules(), &["zygisk-frida".to_string(), "emu-hide".to_string()]);
        assert!(p.su_allowed(10234));          // su=all
        assert_eq!(p.denylist, vec!["com.roblox.client".to_string()]);
        assert!(matches!(p.shamiko, Shamiko::Whitelist));
        // serialize -> parse is stable
        assert_eq!(Profile::parse(&p.serialize()).serialize(), p.serialize());
    }

    #[test]
    fn absent_profile_is_not_rooted_and_su_is_uid_gated() {
        let p = Profile::parse("");            // empty == no root
        assert!(!p.is_rooted());
        let p = Profile::parse("root=1\nsu=com.some.pkg\n");
        assert!(p.su_allowed(0));              // root always
        assert!(p.su_allowed(2000));           // shell always
        assert!(!p.su_allowed(10234));         // a package policy denies other uids in R1
    }

    #[test]
    fn multi_package_su_round_trips() {
        let text = "root=1\nsu=a,b\n";
        let p = Profile::parse(text);
        assert_eq!(p.su, SuPolicy::Packages(vec!["a".to_string(), "b".to_string()]));
        // Serialize and parse back: must preserve both packages
        let serialized = p.serialize();
        let p2 = Profile::parse(&serialized);
        assert_eq!(p2.su, SuPolicy::Packages(vec!["a".to_string(), "b".to_string()]));
        assert_eq!(p2.serialize(), serialized);
    }

    #[test]
    fn no_su_line_defaults_to_empty_packages() {
        let p = Profile::parse("root=1\n");
        assert_eq!(p.su, SuPolicy::Packages(vec![]));
        assert!(p.su_allowed(0));              // root always
        assert!(p.su_allowed(2000));           // shell always
        assert!(!p.su_allowed(10234));         // other uids denied with empty packages
    }
}
