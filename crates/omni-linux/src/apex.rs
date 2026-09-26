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

/// The XML `apexd` writes: every flattened APEX, active, from the system partition.
#[must_use]
pub fn apex_info_list(sysroot: &Sysroot) -> Vec<u8> {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<apex-info-list>\n");
    let mut dirs = sysroot.children(b"/apex");
    dirs.sort();
    for dir in dirs {
        let dir = String::from_utf8_lossy(&dir).into_owned();
        let Some(pb) = sysroot.read(format!("/apex/{dir}/apex_manifest.pb").as_bytes()) else { continue };
        let Some((name, version)) = manifest_name_version(&pb) else { continue };
        let path = ["capex", "apex"]
            .iter()
            .map(|ext| format!("/system/apex/{name}.{ext}"))
            .find(|p| sysroot.read(p.as_bytes()).is_some() || sysroot.has(p.as_bytes()))
            .unwrap_or_else(|| format!("/system/apex/{name}.apex"));
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
