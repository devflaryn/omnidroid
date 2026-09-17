//! Golden-fixture plumbing: get the real ARM64 `.so` files out of the real APK.
//!
//! Global Constraint 2 says tests run against the real APK. `omni-apk` is not available to this
//! crate (it is a sibling, not a dependency, and depending on it would invert the crate order),
//! so this harness does the minimum ZIP work itself: read the end-of-central-directory record,
//! walk the central directory, and inflate the `lib/arm64-v8a/*.so` entries.
//!
//! Extracted libraries are cached under `target/`, never committed (the repo `.gitignore` covers
//! `/target/` and `*.apk`). When the APK is absent every golden test **skips** rather than
//! failing, because the APK is a 160 MB artefact that is not ours to redistribute.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Where the APK is expected: repo root, next to the workspace `Cargo.toml`.
pub const APK_NAME: &str = "Roblox-2.738.1397.apk";

/// The library every golden assertion in the brief is about.
pub const MAIN_LIB: &str = "libroblox.so";

/// Number of ARM64 libraries the APK contains (D9).
pub const EXPECTED_LIB_COUNT: usize = 11;

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <root>/crates/omni-elf.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate manifest dir has two ancestors")
        .to_path_buf()
}

fn apk_path() -> PathBuf {
    repo_root().join(APK_NAME)
}

fn cache_dir() -> PathBuf {
    repo_root().join("target").join("omni-elf-fixtures")
}

/// Emit the standard skip line and return `false`, or return `true` if the APK is present.
///
/// Printed rather than silent so a run without the APK cannot be mistaken for a run that
/// verified anything.
#[must_use]
pub fn apk_available() -> bool {
    if apk_path().is_file() {
        return true;
    }
    // Written straight to the process's stderr rather than through `eprintln!`, which libtest
    // captures and then discards for a passing test. A skipped golden run that looked exactly
    // like a passing one would be the worst outcome here.
    let notice = format!(
        "\nSKIP: omni-elf golden tests need {}, which is not at {}. \
         Every assertion against the real libraries was skipped.\n\n",
        APK_NAME,
        apk_path().display()
    );
    let _ = std::io::Write::write_all(&mut std::io::stderr(), notice.as_bytes());
    false
}

/// Every ARM64 library in the APK, by file name, extracted and cached.
///
/// Extraction happens once per test binary; the bytes are held for the process lifetime because
/// `libroblox.so` alone is 104 MiB and re-reading it per test would dominate the run.
pub fn libraries() -> Option<&'static BTreeMap<String, Vec<u8>>> {
    static LIBS: OnceLock<Option<BTreeMap<String, Vec<u8>>>> = OnceLock::new();
    LIBS.get_or_init(|| {
        if !apk_available() {
            return None;
        }
        match extract_all() {
            Ok(m) => Some(m),
            Err(e) => panic!("failed to extract ARM64 libraries from {APK_NAME}: {e}"),
        }
    })
    .as_ref()
}

/// `libroblox.so`'s bytes, or `None` when the APK is absent.
pub fn main_lib() -> Option<&'static [u8]> {
    libraries().map(|m| {
        m.get(MAIN_LIB)
            .unwrap_or_else(|| panic!("{MAIN_LIB} missing from the APK"))
            .as_slice()
    })
}

/// Skip-or-run helper: `let Some(libs) = require_libs() else { return };`
pub fn require_libs() -> Option<&'static BTreeMap<String, Vec<u8>>> {
    libraries()
}

// ---------------------------------------------------------------------------------------------
// Minimal ZIP reading
// ---------------------------------------------------------------------------------------------

const EOCD_SIG: u32 = 0x0605_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
const PREFIX: &str = "lib/arm64-v8a/";

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

struct Entry {
    name: String,
    method: u16,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
}

fn extract_all() -> std::io::Result<BTreeMap<String, Vec<u8>>> {
    let cache = cache_dir();
    std::fs::create_dir_all(&cache)?;

    let apk = std::fs::read(apk_path())?;
    let entries = central_directory(&apk)?;
    assert_eq!(
        entries.len(),
        EXPECTED_LIB_COUNT,
        "expected {EXPECTED_LIB_COUNT} entries under {PREFIX}, found {}: {:?}",
        entries.len(),
        entries.iter().map(|e| &e.name).collect::<Vec<_>>()
    );

    let mut out = BTreeMap::new();
    for e in entries {
        let short = e
            .name
            .rsplit('/')
            .next()
            .expect("split always yields one element")
            .to_owned();
        let cached = cache.join(&short);
        let bytes = match std::fs::read(&cached) {
            Ok(b) if b.len() as u64 == e.uncomp_size => b,
            _ => {
                let b = inflate_entry(&apk, &e)?;
                // Write via a temp name so a killed run cannot leave a short file behind that a
                // later run would trust on size alone.
                let tmp = cache.join(format!("{short}.partial"));
                std::fs::write(&tmp, &b)?;
                std::fs::rename(&tmp, &cached)?;
                b
            }
        };
        assert_eq!(
            bytes.len() as u64,
            e.uncomp_size,
            "{short}: inflated {} bytes, central directory says {}",
            bytes.len(),
            e.uncomp_size
        );
        out.insert(short, bytes);
    }
    Ok(out)
}

fn central_directory(apk: &[u8]) -> std::io::Result<Vec<Entry>> {
    // The EOCD is within the last 64 KiB + 22 bytes; scan backwards for its signature.
    let scan_from = apk.len().saturating_sub(66 * 1024);
    let mut eocd = None;
    let mut i = apk.len().saturating_sub(22);
    while i >= scan_from {
        if u32le(apk, i) == EOCD_SIG {
            eocd = Some(i);
            break;
        }
        if i == 0 {
            break;
        }
        i -= 1;
    }
    let eocd = eocd.ok_or_else(|| err("no end-of-central-directory record"))?;

    let mut cd_entries = u16le(apk, eocd + 10) as u64;
    let mut cd_size = u32le(apk, eocd + 12) as u64;
    let mut cd_offset = u32le(apk, eocd + 16) as u64;

    // ZIP64: a 0xffff/0xffffffff sentinel means the real value is in the ZIP64 EOCD.
    if cd_entries == 0xffff || cd_offset == 0xffff_ffff || cd_size == 0xffff_ffff {
        let locator = eocd
            .checked_sub(20)
            .ok_or_else(|| err("ZIP64 locator would be before the file start"))?;
        if u32le(apk, locator) != 0x0706_4b50 {
            return Err(err("ZIP64 sentinel present but no ZIP64 locator"));
        }
        let z64 = u64::from_le_bytes(apk[locator + 8..locator + 16].try_into().unwrap()) as usize;
        if u32le(apk, z64) != 0x0606_4b50 {
            return Err(err("ZIP64 end-of-central-directory signature mismatch"));
        }
        cd_entries = u64::from_le_bytes(apk[z64 + 32..z64 + 40].try_into().unwrap());
        cd_size = u64::from_le_bytes(apk[z64 + 40..z64 + 48].try_into().unwrap());
        cd_offset = u64::from_le_bytes(apk[z64 + 48..z64 + 56].try_into().unwrap());
    }

    let mut at = cd_offset as usize;
    let cd_end = at + cd_size as usize;
    let mut out = Vec::new();
    for _ in 0..cd_entries {
        if at + 46 > cd_end || u32le(apk, at) != CDH_SIG {
            return Err(err("central directory header signature mismatch"));
        }
        let method = u16le(apk, at + 10);
        let mut comp_size = u32le(apk, at + 20) as u64;
        let mut uncomp_size = u32le(apk, at + 24) as u64;
        let name_len = u16le(apk, at + 28) as usize;
        let extra_len = u16le(apk, at + 30) as usize;
        let comment_len = u16le(apk, at + 32) as usize;
        let mut local_offset = u32le(apk, at + 42) as u64;
        let name = String::from_utf8_lossy(&apk[at + 46..at + 46 + name_len]).into_owned();
        let extra = &apk[at + 46 + name_len..at + 46 + name_len + extra_len];

        if uncomp_size == 0xffff_ffff || comp_size == 0xffff_ffff || local_offset == 0xffff_ffff {
            // ZIP64 extended information extra field (0x0001), in the fixed order
            // uncompressed, compressed, local-header offset — only the sentinel fields present.
            let mut e = 0usize;
            while e + 4 <= extra.len() {
                let id = u16le(extra, e);
                let sz = u16le(extra, e + 2) as usize;
                let body = &extra[e + 4..(e + 4 + sz).min(extra.len())];
                if id == 0x0001 {
                    let mut b = 0usize;
                    let take = |b: &mut usize| -> Option<u64> {
                        if *b + 8 <= body.len() {
                            let v = u64::from_le_bytes(body[*b..*b + 8].try_into().unwrap());
                            *b += 8;
                            Some(v)
                        } else {
                            None
                        }
                    };
                    if uncomp_size == 0xffff_ffff {
                        uncomp_size = take(&mut b).ok_or_else(|| err("ZIP64 field truncated"))?;
                    }
                    if comp_size == 0xffff_ffff {
                        comp_size = take(&mut b).ok_or_else(|| err("ZIP64 field truncated"))?;
                    }
                    if local_offset == 0xffff_ffff {
                        local_offset = take(&mut b).ok_or_else(|| err("ZIP64 field truncated"))?;
                    }
                    break;
                }
                e += 4 + sz;
            }
        }

        if name.starts_with(PREFIX) && name.ends_with(".so") {
            out.push(Entry {
                name,
                method,
                comp_size,
                uncomp_size,
                local_offset,
            });
        }
        at += 46 + name_len + extra_len + comment_len;
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn inflate_entry(apk: &[u8], e: &Entry) -> std::io::Result<Vec<u8>> {
    let lfh = e.local_offset as usize;
    if u32le(apk, lfh) != LFH_SIG {
        return Err(err("local file header signature mismatch"));
    }
    let name_len = u16le(apk, lfh + 26) as usize;
    let extra_len = u16le(apk, lfh + 28) as usize;
    let data_at = lfh + 30 + name_len + extra_len;
    let data = &apk[data_at..data_at + e.comp_size as usize];
    match e.method {
        // Stored.
        0 => Ok(data.to_vec()),
        // Deflate. All 11 libraries in this APK are deflated (D9).
        8 => {
            let mut out = Vec::with_capacity(e.uncomp_size as usize);
            flate2::read::DeflateDecoder::new(data).read_to_end(&mut out)?;
            Ok(out)
        }
        m => Err(err(&format!("unsupported ZIP compression method {m}"))),
    }
}

fn err(msg: &str) -> std::io::Error {
    std::io::Error::other(msg.to_owned())
}
