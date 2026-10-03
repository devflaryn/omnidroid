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
    /// Parse a root profile from text (line-oriented key=value format).
    pub fn parse(text: &str) -> Self {
        let mut rooted = false;
        let mut magisk_code = 0;
        let mut module_ids = Vec::new();
        let mut su = SuPolicy::All; // Default
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
                        module_ids.push(value.trim().to_string());
                    }
                    "su" => {
                        let val = value.trim();
                        if val == "all" {
                            su = SuPolicy::All;
                        } else {
                            // Parse comma-separated packages, or handle multiple su= lines
                            let packages: Vec<String> = val.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                            if !packages.is_empty() {
                                su = SuPolicy::Packages(packages);
                            }
                        }
                    }
                    "denylist" => {
                        // Parse comma-separated packages
                        let packages: Vec<String> = value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                        denylist.extend(packages);
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
                for pkg in pkgs {
                    lines.push(format!("su={}", pkg));
                }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
