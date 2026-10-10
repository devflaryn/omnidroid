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

/// **The compressed APEXes, decompressed before the first boot** (`OMNI_APEX_PREDECOMPRESS=0`: not).
/// On a new device `apexd` decompresses every `.capex` into `/data/apex/decompressed` -- 22 files in
/// the guest's own code, ~1.8 s of init's wait for `apexd.status` on the i7 (s13), more on a slower
/// machine. What it writes is the capex's `original_apex` entry as it is; finding one already there
/// that is that APEX (its key, version and root digest), it logs "Skipping decompression". Here each
/// is taken out of its capex once into a host cache named by the capex's content (the temporary
/// directory's `omni-apex-decompressed`) and copied into the new instance, root's -- a copy, not a
/// link: an instance's files are its own to delete, and none can write the cache through it
/// (~240 MB, a fraction of a second). One that `apexd` rejected would be decompressed by it as
/// before.
pub fn predecompress(sysroot: &Sysroot, instance: &std::path::Path, owners: &crate::owners::Owners) {
    if std::env::var("OMNI_APEX_PREDECOMPRESS").as_deref() == Ok("0") {
        return;
    }
    let cache = std::env::temp_dir().join("omni-apex-decompressed");
    let dest_dir = instance.join("data").join("apex").join("decompressed");
    let mut placed = 0;
    for m in mounts(sysroot).into_iter().filter(|m| m.file.ends_with(".capex")) {
        let Some(capex) = sysroot.host_path(m.file.as_bytes()) else { continue };
        let Some(key) = capex.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        let cached = cache.join(format!("{key}.apex"));
        if !cached.exists() {
            let Ok(bytes) = omni_apk::Apk::open(&capex).and_then(|a| a.read_named("original_apex")) else { continue };
            let _ = std::fs::create_dir_all(&cache);
            let partial = cache.join(format!("{key}.{}", std::process::id()));
            if std::fs::write(&partial, &bytes).is_err() {
                let _ = std::fs::remove_file(&partial);
                continue;
            }
            if std::fs::rename(&partial, &cached).is_err() {
                let _ = std::fs::remove_file(&partial);  // another process made it first
            }
        }
        let dest = dest_dir.join(format!("{}@{}.decompressed.apex", m.name, m.version));
        if dest.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(&dest_dir);
        if std::fs::copy(&cached, &dest).is_err() {
            let _ = std::fs::remove_file(&dest);
            continue;
        }
        owners.set(&dest, crate::owners::Owner { uid: 0, gid: 0, mode: 0o644 });
        placed += 1;
    }
    if placed > 0 {
        eprintln!("[apex] {placed} compressed APEXes placed decompressed (OMNI_APEX_PREDECOMPRESS=0: apexd decompresses them)");
    }
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
