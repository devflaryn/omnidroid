//! What `apexd` publishes at boot, for sub-project B: `/apex/apex-info-list.xml`, from which
//! `linkerconfig` learns the active APEXes and so gives ART and its peers their linker namespaces.
//! The APEX payloads are already flattened into the sysroot at `/apex/<name>` (`make_sysroot.py`).
use std::fmt::Write as _;

use crate::vfs::Sysroot;

/// `apex_manifest.pb`'s `name` (field 1) and `version` (field 2).
#[must_use]
pub fn manifest_name_version(pb: &[u8]) -> Option<(String, u64)> {
    let mut i = 0;
    let (mut name, mut version) = (None, 0u64);
    let varint = |i: &mut usize| -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = *pb.get(*i)?;
            *i += 1;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    };
    while i < pb.len() {
        let key = varint(&mut i)?;
        match key & 7 {
            0 => {
                let v = varint(&mut i)?;
                if key >> 3 == 2 {
                    version = v;
                }
            }
            2 => {
                let len = usize::try_from(varint(&mut i)?).ok()?;
                let bytes = pb.get(i..i.checked_add(len)?)?;
                if key >> 3 == 1 {
                    name = Some(String::from_utf8(bytes.to_vec()).ok()?);
                }
                i += len;
            }
            1 => i += 8,
            5 => i += 4,
            _ => return None,
        }
    }
    Some((name?, version))
}

/// An APEX of the image as it is mounted at boot: on `/dev/block/loop<index>`, at
/// `/apex/<name>@<version>` (and bound to `/apex/<name>`, where the sysroot holds its flattened
/// payload), backed by `backing`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApexMount {
    pub name: String,
    pub version: u64,
    pub index: usize,
    /// Its file on a partition (`/system/apex/<name>.apex`).
    pub file: String,
    /// What its loop device reads: the file, or for a compressed APEX (`.capex`) the file apexd
    /// decompresses it into (`/data/apex/decompressed/<name>@<version>.decompressed.apex`).
    pub backing: String,
}

/// The partitions' APEX directories, where a pre-installed APEX is.
const APEX_DIRS: [&str; 4] = ["/system/apex", "/system_ext/apex", "/product/apex", "/vendor/apex"];

/// Every APEX of the image with a payload in the sysroot and a file on a partition, in name order.
#[must_use]
pub fn mounts(sysroot: &Sysroot) -> Vec<ApexMount> {
    let mut dirs = sysroot.children(b"/apex");
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let dir = String::from_utf8_lossy(&dir).into_owned();
        let Some(pb) = sysroot.read(format!("/apex/{dir}/apex_manifest.pb").as_bytes()) else { continue };
        let Some((name, version)) = manifest_name_version(&pb) else { continue };
        let Some(file) = APEX_DIRS.iter().flat_map(|d| ["apex", "capex"].map(|ext| format!("{d}/{name}.{ext}"))).find(|p| sysroot.has(p.as_bytes())) else {
            continue;
        };
        let backing = if file.ends_with(".capex") { format!("/data/apex/decompressed/{name}@{version}.decompressed.apex") } else { file.clone() };
        out.push(ApexMount { name, version, index: out.len(), file, backing });
    }
    out
}

/// The XML `apexd` writes: every flattened APEX, active, from its partition.
#[must_use]
pub fn apex_info_list(sysroot: &Sysroot) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<apex-info-list>\n");
    for m in mounts(sysroot) {
        let (name, version, path) = (m.name, m.version, m.file);
        let _ = writeln!(
            out,
            "    <apex-info moduleName=\"{name}\" modulePath=\"{path}\" preinstalledModulePath=\"{path}\" \
             versionCode=\"{version}\" versionName=\"\" isFactory=\"true\" isActive=\"true\" \
             lastUpdateMillis=\"0\" provideSharedApexLibs=\"false\" partition=\"SYSTEM\"></apex-info>"
        );
    }
    out.push_str("</apex-info-list>\n");
    out.into_bytes()
}
