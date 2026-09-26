//! `sysroot.manifest` (written by `tools/make_sysroot.py`).
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir { mode: u32 },
    /// Stored at `objects/<sha256[:2]>/<sha256>` (content-addressed: see `tools/make_sysroot.py`).
    File { mode: u32, size: u64, sha256: String },
    Symlink { target: Vec<u8> },
}

#[derive(Debug, Default)]
pub struct Manifest {
    pub entries: BTreeMap<Vec<u8>, Entry>,
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let mut entries = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let bad = || format!("sysroot.manifest line {}: {line:?}", i + 1);
        let mode = |s: &str| u32::from_str_radix(s, 8).map_err(|_| bad());
        let (path, entry) = match f.as_slice() {
            ["d", m, p] => (*p, Entry::Dir { mode: mode(m)? }),
            ["f", m, size, sha, p] if sha.len() >= 2 => (
                *p,
                Entry::File { mode: mode(m)?, size: size.parse().map_err(|_| bad())?, sha256: (*sha).to_string() },
            ),
            ["l", target, p] => (*p, Entry::Symlink { target: target.as_bytes().to_vec() }),
            _ => return Err(bad()),
        };
        entries.insert(path.as_bytes().to_vec(), entry);
    }
    Ok(Manifest { entries })
}
