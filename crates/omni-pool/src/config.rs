//! The app's settings: kept as `key = value` lines in `<app-data>/../app.conf`, every key optional,
//! an unknown key kept as it is (a newer version's) and a bad value refused by name.

use std::path::{Path, PathBuf};

/// Every setting, with its default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Close a device that has run no app for this many minutes (0: never).
    pub idle_close_minutes: u64,
    /// How many idle devices are kept warm anyway (so the next launch starts at once).
    pub keep_warm: u32,
    /// Warm a device as soon as the app opens.
    pub warm_on_start: bool,
    /// Stop every device when the app quits (else they keep running for the next session).
    pub stop_on_exit: bool,
    /// The most devices at once (0: no limit but the host's).
    pub max_devices: u32,
    /// The size of a new instance's display (and window), `<w>x<h>`.
    pub instance_size: (u32, u32),
    /// The devices' GPU backend: `auto`, `vulkan` or `gl`.
    pub gpu: String,
    /// Each device's RAM in MiB (0: the default).
    pub device_ram_mb: u64,
    /// The MCP server: on, and where it listens (127.0.0.1 only).
    pub mcp_enabled: bool,
    pub mcp_port: u16,
    /// Start an instance's app again on its window ([`crate::pool::RESTART_DELAY`] after) when it
    /// crashes -- not when it is closed on purpose. An instance's own setting overrides it
    /// ([`crate::Pool::set_auto_restart`]).
    pub auto_restart: bool,
    /// Lines of a newer version's keys, kept when saving.
    pub other: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            idle_close_minutes: 5,
            keep_warm: 1,
            warm_on_start: true,
            stop_on_exit: true,
            max_devices: 0,
            instance_size: (1280, 720),
            gpu: "auto".into(),
            device_ram_mb: 0,
            mcp_enabled: false,
            mcp_port: 7457,
            auto_restart: false,
            other: Vec::new(),
        }
    }
}

/// The settings' keys, their meaning and how to give them: what `settings` lists.
pub const KEYS: &[(&str, &str)] = &[
    ("idle_close_minutes", "close a device idle (no app running) this long; 0 never"),
    ("keep_warm", "idle devices kept warm anyway, so the next launch starts at once"),
    ("warm_on_start", "warm a device when the app opens (true/false)"),
    ("stop_on_exit", "stop every device when the app quits (true/false)"),
    ("max_devices", "the most devices at once; 0 no limit"),
    ("instance_size", "a new instance's display and window, <w>x<h>"),
    ("gpu", "the devices' GPU backend: auto, vulkan or gl"),
    ("device_ram_mb", "each device's RAM in MiB; 0 the default"),
    ("mcp_enabled", "serve MCP for coding agents on 127.0.0.1 (true/false)"),
    ("mcp_port", "the MCP server's port"),
    ("auto_restart", "start an app that crashed again on its window after 5 s (true/false)"),
];

fn boolean(key: &str, v: &str) -> Result<bool, String> {
    match v {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        _ => Err(format!("{key} wants true or false, not `{v}`")),
    }
}

fn number<T: std::str::FromStr>(key: &str, v: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("{key} wants a whole number, not `{v}`"))
}

impl Config {
    /// Where the settings are kept: `<app-data>/../app.conf` (`OMNI_APP_CONFIG` overrides).
    #[must_use]
    pub fn path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("OMNI_APP_CONFIG") {
            return Some(PathBuf::from(p));
        }
        omni_platform::process::app_data_dir().and_then(|d| Some(d.parent()?.join("app.conf")))
    }

    /// The settings in `path`, defaults where a key is missing, and what was wrong with the rest.
    #[must_use]
    pub fn load(path: &Path) -> (Self, Vec<String>) {
        let mut c = Self::default();
        let mut problems = Vec::new();
        let Ok(text) = std::fs::read_to_string(path) else { return (c, problems) };
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let Some((k, v)) = t.split_once('=') else {
                problems.push(format!("not `key = value`: {t}"));
                continue;
            };
            match c.set(k.trim(), v.trim()) {
                Ok(true) => {}
                Ok(false) => c.other.push(t.to_string()),
                Err(e) => problems.push(e),
            }
        }
        (c, problems)
    }

    /// Set `key` to `value`: `Ok(false)` for a key this version does not know.
    ///
    /// # Errors
    /// A value the key does not take.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool, String> {
        match key {
            "idle_close_minutes" => self.idle_close_minutes = number(key, value)?,
            "keep_warm" => self.keep_warm = number(key, value)?,
            "warm_on_start" => self.warm_on_start = boolean(key, value)?,
            "stop_on_exit" => self.stop_on_exit = boolean(key, value)?,
            "max_devices" => self.max_devices = number(key, value)?,
            "instance_size" => {
                self.instance_size = value
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?)))
                    .filter(|(w, h)| *w >= 320 && *h >= 240)
                    .ok_or_else(|| format!("{key} wants <w>x<h>, at least 320x240, not `{value}`"))?;
            }
            "gpu" => {
                if !matches!(value, "auto" | "vulkan" | "gl") {
                    return Err(format!("{key} wants auto, vulkan or gl, not `{value}`"));
                }
                self.gpu = value.to_string();
            }
            "device_ram_mb" => self.device_ram_mb = number(key, value)?,
            "mcp_enabled" => self.mcp_enabled = boolean(key, value)?,
            "mcp_port" => self.mcp_port = number::<u16>(key, value).and_then(|p| if p == 0 { Err(format!("{key} wants a port above 0")) } else { Ok(p) })?,
            "auto_restart" => self.auto_restart = boolean(key, value)?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// A key's value, as `set` takes it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "idle_close_minutes" => self.idle_close_minutes.to_string(),
            "keep_warm" => self.keep_warm.to_string(),
            "warm_on_start" => self.warm_on_start.to_string(),
            "stop_on_exit" => self.stop_on_exit.to_string(),
            "max_devices" => self.max_devices.to_string(),
            "instance_size" => format!("{}x{}", self.instance_size.0, self.instance_size.1),
            "gpu" => self.gpu.clone(),
            "device_ram_mb" => self.device_ram_mb.to_string(),
            "mcp_enabled" => self.mcp_enabled.to_string(),
            "mcp_port" => self.mcp_port.to_string(),
            "auto_restart" => self.auto_restart.to_string(),
            _ => return None,
        })
    }

    /// The settings as the file keeps them.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::from("# omnidroid app settings (`settings` in the app changes them)\n");
        for (key, _) in KEYS {
            out.push_str(&format!("{key} = {}\n", self.get(key).unwrap_or_default()));
        }
        for line in &self.other {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Write the settings to `path`, whole (a rename).
    ///
    /// # Errors
    /// The file system's.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = path.with_extension("conf.part");
        std::fs::write(&part, self.to_text())?;
        std::fs::rename(part, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_ones_asked_for() {
        let c = Config::default();
        assert_eq!((c.idle_close_minutes, c.keep_warm, c.warm_on_start, c.mcp_enabled, c.auto_restart), (5, 1, true, false, false));
    }

    #[test]
    fn settings_round_trip_and_a_bad_value_is_named() {
        let dir = std::env::temp_dir().join(format!("omni-pool-config-{}", std::process::id()));
        let path = dir.join("app.conf");
        let mut c = Config::default();
        c.set("idle_close_minutes", "0").unwrap();
        c.set("mcp_enabled", "on").unwrap();
        c.set("instance_size", "960x540").unwrap();
        c.set("auto_restart", "yes").unwrap();
        assert!(!c.set("from_a_newer_version", "x").unwrap());
        c.other.push("from_a_newer_version = x".into());
        c.save(&path).unwrap();
        let (back, problems) = Config::load(&path);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(back, c);
        assert!(Config::default().set("keep_warm", "-1").is_err());
        assert!(Config::default().set("gpu", "metal").is_err());
        assert!(Config::default().set("instance_size", "10x10").is_err());
        std::fs::write(&path, "keep_warm = lots\nnonsense\n").unwrap();
        let (c, problems) = Config::load(&path);
        assert_eq!(c.keep_warm, 1, "the default stays");
        assert_eq!(problems.len(), 2, "{problems:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
