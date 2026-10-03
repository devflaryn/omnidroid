//! The pinned Magisk installer assets (`util_functions.sh` + `busybox`), fetched by
//! `tools/fetch_magisk.py` into `sysroot/magisk-<version>/`. They are GPL and never committed.
use std::path::{Path, PathBuf};

/// `tools/magisk.pin`: the one official Magisk release the installer environment comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagiskPin {
    pub version: String,
    pub version_code: u32,
    pub url: String,
    pub sha256: String,
}

impl MagiskPin {
    pub fn parse(text: &str) -> Result<MagiskPin, String> {
        let get = |key: &str| -> Result<String, String> {
            text.lines()
                .filter_map(|l| l.trim().split_once('='))
                .find(|(k, _)| k.trim() == key)
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("magisk.pin: missing `{key}`"))
        };
        let version = get("version")?;
        if version.contains("..") || !version.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(format!("magisk.pin: unsafe version `{version}`"));
        }
        let code = get("versionCode")?;
        Ok(MagiskPin {
            version,
            version_code: code
                .parse()
                .map_err(|_| format!("magisk.pin: bad versionCode `{code}`"))?,
            url: get("url")?,
            sha256: get("sha256")?,
        })
    }
}

/// Where the extracted installer files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagiskAssets {
    pub util_functions: PathBuf,
    pub busybox: PathBuf,
    pub version_code: u32,
}

impl MagiskAssets {
    pub fn find(repo_root: &Path) -> Result<MagiskAssets, String> {
        let hint = "run `python tools/fetch_magisk.py`";
        let pin_path = repo_root.join("tools").join("magisk.pin");
        let text = std::fs::read_to_string(&pin_path)
            .map_err(|e| format!("cannot read {}: {e}; {hint}", pin_path.display()))?;
        let pin = MagiskPin::parse(&text)?;
        let dir = repo_root.join("sysroot").join(format!("magisk-{}", pin.version));
        let util_functions = dir.join("util_functions.sh");
        let busybox = dir.join("busybox");
        for p in [&util_functions, &busybox] {
            if !p.is_file() {
                return Err(format!("Magisk {} asset missing: {}; {hint}", pin.version, p.display()));
            }
        }
        Ok(MagiskAssets { util_functions, busybox, version_code: pin.version_code })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_the_pin() {
        let p = MagiskPin::parse("version=v29.0\nversionCode=29000\nurl=https://example/m.apk\nsha256=abcd\n").unwrap();
        assert_eq!(p.version_code, 29000);
        assert_eq!(p.sha256, "abcd");
    }
    #[test]
    fn rejects_unsafe_version() {
        for v in ["v1/../x", "..", "a\\b", "v 1", "a/b"] {
            let t = format!("version={v}
versionCode=1
url=u
sha256=s
");
            assert!(MagiskPin::parse(&t).is_err(), "{v}");
        }
    }
    #[test]
    fn find_explains_when_absent() {
        let err = MagiskAssets::find(std::path::Path::new("/no/such/repo")).unwrap_err();
        assert!(err.contains("fetch_magisk.py"));
    }
}
