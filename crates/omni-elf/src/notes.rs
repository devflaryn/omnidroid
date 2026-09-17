//! `PT_NOTE` parsing: `.note.android.ident` and `.note.gnu.property`.

use crate::consts::*;
use crate::error::{ElfError, Result};
use crate::reader::View;

/// One ELF note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note<'a> {
    /// Owner name with the trailing NUL stripped, e.g. `"Android"`, `"GNU"`.
    pub name: &'a [u8],
    pub n_type: u32,
    pub desc: &'a [u8],
}

impl Note<'_> {
    /// Owner name as a `&str` if it is ASCII, which every note owner in practice is.
    pub fn name_str(&self) -> Option<&str> {
        core::str::from_utf8(self.name).ok()
    }
}

#[inline]
fn align4(n: usize) -> Option<usize> {
    n.checked_add(3).map(|x| x & !3)
}

/// Parse all notes in one `PT_NOTE` segment (or `SHT_NOTE` section).
///
/// A malformed note is an error rather than a stop, because the alternative — returning the
/// notes parsed so far — would make "no `.note.android.ident`" and "corrupt note stream" look
/// identical to the caller.
pub fn parse_notes<'a>(seg: &View<'a>) -> Result<Vec<Note<'a>>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < seg.len() {
        // A run of zero padding at the end of a note segment is legal and common.
        if seg.len() - at < 12 {
            if seg.bytes()[at..].iter().all(|&b| b == 0) {
                break;
            }
            return Err(ElfError::BadNote {
                offset: at,
                reason: "fewer than 12 bytes remain but they are not zero padding",
            });
        }
        let namesz = seg.u32("n_namesz", at)? as usize;
        let descsz = seg.u32("n_descsz", at + 4)? as usize;
        let n_type = seg.u32("n_type", at + 8)?;
        if namesz == 0 && descsz == 0 && n_type == 0 {
            break;
        }
        let name_at = at + 12;
        let name_raw = seg.slice("note name", name_at, namesz)?;
        // The owner name is NUL-terminated inside n_namesz; tolerate a missing NUL rather than
        // failing, since some toolchains have shipped notes without one.
        let name = match name_raw.iter().position(|&b| b == 0) {
            Some(p) => &name_raw[..p],
            None => name_raw,
        };
        let desc_at = name_at
            .checked_add(align4(namesz).ok_or(ElfError::BadNote {
                offset: at,
                reason: "n_namesz overflows when aligned",
            })?)
            .ok_or(ElfError::BadNote {
                offset: at,
                reason: "note descriptor offset overflows",
            })?;
        let desc = seg.slice("note descriptor", desc_at, descsz)?;
        out.push(Note { name, n_type, desc });
        at = desc_at
            .checked_add(align4(descsz).ok_or(ElfError::BadNote {
                offset: at,
                reason: "n_descsz overflows when aligned",
            })?)
            .ok_or(ElfError::BadNote {
                offset: at,
                reason: "next note offset overflows",
            })?;
    }
    Ok(out)
}

/// `.note.android.ident`, which records what the NDK built this object for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidIdent {
    /// `android_api`: the minimum SDK level.
    pub android_api: u32,
    /// The NDK version string, e.g. `"r28c"`.
    pub ndk_version: String,
    /// The NDK build number, e.g. `"13676358"`.
    pub ndk_build_number: String,
}

impl AndroidIdent {
    /// Parse the descriptor of an `Android`-owner, type-1 note.
    ///
    /// The layout is `uint32 android_api` followed by two fixed 64-byte NUL-padded strings.
    /// Older notes carry only the API level, so the strings are optional.
    pub fn parse(desc: &[u8]) -> Result<Self> {
        if desc.len() < 4 {
            return Err(ElfError::BadNote {
                offset: 0,
                reason: ".note.android.ident descriptor is shorter than 4 bytes",
            });
        }
        let android_api = u32::from_le_bytes([desc[0], desc[1], desc[2], desc[3]]);
        let field = |start: usize| -> String {
            let end = (start + 64).min(desc.len());
            if start >= desc.len() {
                return String::new();
            }
            let raw = &desc[start..end];
            let raw = match raw.iter().position(|&b| b == 0) {
                Some(p) => &raw[..p],
                None => raw,
            };
            String::from_utf8_lossy(raw).into_owned()
        };
        Ok(AndroidIdent {
            android_api,
            ndk_version: field(4),
            ndk_build_number: field(68),
        })
    }
}

/// AArch64 hardening features declared through `.note.gnu.property`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GnuProperties {
    /// The raw `GNU_PROPERTY_AARCH64_FEATURE_1_AND` value, if the property was present.
    pub aarch64_feature_1_and: Option<u32>,
    /// Branch Target Identification.
    pub bti: bool,
    /// Pointer Authentication.
    pub pac: bool,
    /// Guarded Control Stack.
    pub gcs: bool,
    /// Every `pr_type` seen, for a report that can say what was ignored.
    pub property_types: Vec<u32>,
}

impl GnuProperties {
    /// Parse the descriptor of a `GNU`-owner `NT_GNU_PROPERTY_TYPE_0` note.
    ///
    /// Each property is `uint32 pr_type`, `uint32 pr_datasz`, then `pr_datasz` bytes padded to
    /// an 8-byte boundary (ELF64).
    pub fn parse(desc: &[u8]) -> Result<Self> {
        let view = View::new(desc);
        let mut out = GnuProperties::default();
        let mut at = 0usize;
        while at + 8 <= desc.len() {
            let pr_type = view.u32("pr_type", at)?;
            let pr_datasz = view.u32("pr_datasz", at + 4)? as usize;
            let data = view.slice("pr_data", at + 8, pr_datasz)?;
            out.property_types.push(pr_type);
            if pr_type == GNU_PROPERTY_AARCH64_FEATURE_1_AND && pr_datasz >= 4 {
                let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                out.aarch64_feature_1_and = Some(v);
                out.bti = v & GNU_PROPERTY_AARCH64_FEATURE_1_BTI != 0;
                out.pac = v & GNU_PROPERTY_AARCH64_FEATURE_1_PAC != 0;
                out.gcs = v & GNU_PROPERTY_AARCH64_FEATURE_1_GCS != 0;
            }
            let step = 8usize
                .checked_add((pr_datasz + 7) & !7)
                .ok_or(ElfError::BadNote {
                    offset: at,
                    reason: "pr_datasz overflows when aligned",
                })?;
            if step == 0 {
                return Err(ElfError::BadNote {
                    offset: at,
                    reason: "zero-length property would not terminate",
                });
            }
            at += step;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note_bytes(name: &[u8], n_type: u32, desc: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let namesz = name.len() + 1;
        v.extend_from_slice(&(namesz as u32).to_le_bytes());
        v.extend_from_slice(&(desc.len() as u32).to_le_bytes());
        v.extend_from_slice(&n_type.to_le_bytes());
        v.extend_from_slice(name);
        v.push(0);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v.extend_from_slice(desc);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    #[test]
    fn parses_two_notes_with_padding() {
        let mut buf = note_bytes(b"Android", 1, &[26, 0, 0, 0]);
        buf.extend_from_slice(&note_bytes(b"GNU", NT_GNU_BUILD_ID, &[0xaa; 20]));
        let notes = parse_notes(&View::new(&buf)).unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].name, b"Android");
        assert_eq!(notes[0].n_type, 1);
        assert_eq!(notes[1].name, b"GNU");
        assert_eq!(notes[1].desc.len(), 20);
    }

    #[test]
    fn android_ident_reads_api_and_strings() {
        let mut desc = Vec::new();
        desc.extend_from_slice(&26u32.to_le_bytes());
        let mut ver = [0u8; 64];
        ver[..4].copy_from_slice(b"r28c");
        desc.extend_from_slice(&ver);
        let mut build = [0u8; 64];
        build[..8].copy_from_slice(b"13676358");
        desc.extend_from_slice(&build);
        let id = AndroidIdent::parse(&desc).unwrap();
        assert_eq!(id.android_api, 26);
        assert_eq!(id.ndk_version, "r28c");
        assert_eq!(id.ndk_build_number, "13676358");
    }

    #[test]
    fn gnu_property_reads_aarch64_feature_bits() {
        let mut desc = Vec::new();
        desc.extend_from_slice(&GNU_PROPERTY_AARCH64_FEATURE_1_AND.to_le_bytes());
        desc.extend_from_slice(&4u32.to_le_bytes());
        desc.extend_from_slice(&0b11u32.to_le_bytes());
        desc.extend_from_slice(&[0, 0, 0, 0]); // pad to 8
        let p = GnuProperties::parse(&desc).unwrap();
        assert_eq!(p.aarch64_feature_1_and, Some(3));
        assert!(p.bti && p.pac && !p.gcs);
    }

    #[test]
    fn truncated_note_is_an_error() {
        let buf = [0x20u8, 0, 0, 0, 0x10, 0, 0, 0, 1, 0, 0, 0];
        assert!(parse_notes(&View::new(&buf)).is_err());
    }
}
