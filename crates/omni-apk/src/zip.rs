//! Zip container parsing: end of central directory, zip64, central directory, local file headers.
//!
//! This is written by hand rather than taken from a zip crate for one reason: Omnidroid needs
//! facts that a high-level "read me this file" API hides. Specifically it needs the **absolute
//! file offset of each entry's payload** and the **alignment of that offset**, because together
//! with the compression method they decide whether the entry can be mapped straight out of the
//! APK (D11). Nothing above the container layer can recover those.
//!
//! # Where each fact comes from
//!
//! | Fact | Source |
//! |---|---|
//! | name, method, CRC-32, compressed size, uncompressed size | central-directory record |
//! | offset of the **local file header** | central-directory record |
//! | offset of the **payload** | local file header — see below |
//!
//! The central directory records where an entry's *local file header* starts, never where its
//! data starts. The data starts after the local header's own 30 fixed bytes plus its own name and
//! **its own extra field**, and the local extra field is not the central one: `zipalign` does its
//! padding by growing the local extra field, so in a 4-byte-aligned APK the local extra field is
//! 0–3 bytes while the central extra field is usually empty. So the payload offset — and
//! therefore the mappability of the entry — can only be learned by reading the local file header.
//!
//! Sizes and CRC-32 are taken from the central directory and never from the local header, because
//! a writer that sets general-purpose flag bit 3 leaves all three zero in the local header and
//! emits them in a trailing data descriptor instead. Where the local header *does* carry non-zero
//! values, they are cross-checked and a disagreement is an error.

use crate::error::{compression_method_name, ApkError, ApkResult};

/// The page size Omnidroid maps at, and therefore the alignment that decides direct mappability.
///
/// 4 KB, not 64 KB: the placeholder-based mapping path measured in D11 constrains both the base
/// address and the file offset to 4 KB, not to the 64 KB allocation granularity that plain
/// `MapViewOfFile` would impose.
pub const MAPPING_ALIGNMENT: u64 = 4096;

/// The largest page size Omnidroid may ever have to satisfy (Android's 16 KB page mode).
///
/// Used as the cap for [`ZipEntry::payload_alignment`]: alignment beyond this is not interesting,
/// so reporting it would only invite tests that assert accidental facts.
pub const MAX_INTERESTING_ALIGNMENT: u64 = 16384;

pub(crate) const SIG_LOCAL_FILE_HEADER: u32 = 0x0403_4b50;
pub(crate) const SIG_CENTRAL_RECORD: u32 = 0x0201_4b50;
pub(crate) const SIG_EOCD: u32 = 0x0605_4b50;
pub(crate) const SIG_ZIP64_EOCD: u32 = 0x0606_4b50;
pub(crate) const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;

pub(crate) const EOCD_LEN: usize = 22;
pub(crate) const ZIP64_LOCATOR_LEN: usize = 20;
pub(crate) const ZIP64_EOCD_LEN: usize = 56;
pub(crate) const LOCAL_FILE_HEADER_LEN: usize = 30;

/// The fixed part of a central-directory record. A record is never shorter than this, which makes
/// it the divisor that bounds how many records a given number of bytes can hold.
pub(crate) const CENTRAL_RECORD_LEN: usize = 46;

/// The most one byte of a deflate stream can expand to, from RFC 1951's maximum match length and
/// minimum encoding: 1032:1. Nothing legitimate exceeds it, so it is the ceiling that turns a
/// declared uncompressed size into a bounded allocation request.
pub const MAX_DEFLATE_EXPANSION: u64 = 1032;

/// The longest possible zip comment, and therefore the furthest the EOCD can be from the end.
pub(crate) const MAX_COMMENT_LEN: u64 = u16::MAX as u64;

/// General-purpose flag bit 3: sizes and CRC-32 live in a trailing data descriptor, and the local
/// file header's copies are zero.
pub(crate) const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;

/// The zip64 extended-information extra field's header ID.
pub(crate) const EXTRA_ID_ZIP64: u16 = 0x0001;

// ---------------------------------------------------------------------------------------------
// Little-endian field reader
// ---------------------------------------------------------------------------------------------

/// A bounds-checked little-endian cursor that knows its absolute file offset.
///
/// Every failure it produces names the structure, the field and the absolute offset, which is the
/// only form of container-parsing error that is actually actionable.
pub(crate) struct Fields<'a> {
    data: &'a [u8],
    pos: usize,
    base: u64,
    structure: &'static str,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(data: &'a [u8], base: u64, structure: &'static str) -> Self {
        Self {
            data,
            pos: 0,
            base,
            structure,
        }
    }

    /// Absolute file offset of the cursor.
    pub(crate) fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub(crate) fn take(&mut self, n: usize, field: &'static str) -> ApkResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(ApkError::Truncated {
                structure: self.structure,
                offset: self.offset(),
                field,
                needed: n,
                available: self.remaining(),
            });
        }
        let out = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub(crate) fn u16(&mut self, field: &'static str) -> ApkResult<u16> {
        let b = self.take(2, field)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, field: &'static str) -> ApkResult<u32> {
        let b = self.take(4, field)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self, field: &'static str) -> ApkResult<u64> {
        let b = self.take(8, field)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Read the four-byte signature and require it to be `expected`.
    pub(crate) fn signature(&mut self, expected: u32) -> ApkResult<()> {
        let offset = self.offset();
        let found = self.u32("signature")?;
        if found != expected {
            return Err(ApkError::BadSignature {
                structure: self.structure,
                offset,
                found,
                expected,
            });
        }
        Ok(())
    }
}

/// Decode an entry name, requiring UTF-8.
pub(crate) fn decode_name(bytes: &[u8], offset: u64) -> ApkResult<String> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_owned()),
        Err(source) => Err(ApkError::NameNotUtf8 {
            offset,
            lossy: String::from_utf8_lossy(bytes).into_owned(),
            source,
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// Compression method
// ---------------------------------------------------------------------------------------------

/// How an entry's payload is stored in the archive.
///
/// `Other` exists so that an archive containing one entry with an exotic method is still
/// *readable* for all its other entries; the failure happens when that entry is read, not when
/// the archive is opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompressionMethod {
    /// Method 0: the payload is the file, byte for byte. The only directly mappable method.
    Stored,
    /// Method 8: raw deflate, as in RFC 1951 with no zlib or gzip wrapper.
    Deflated,
    /// Anything else. Not readable by this crate.
    Other(u16),
}

impl CompressionMethod {
    /// Decode a method code from a header field.
    #[must_use]
    pub const fn from_code(code: u16) -> Self {
        match code {
            0 => CompressionMethod::Stored,
            8 => CompressionMethod::Deflated,
            other => CompressionMethod::Other(other),
        }
    }

    /// The code as it appears in the headers.
    #[must_use]
    pub const fn code(self) -> u16 {
        match self {
            CompressionMethod::Stored => 0,
            CompressionMethod::Deflated => 8,
            CompressionMethod::Other(code) => code,
        }
    }

    /// The APPNOTE.TXT name of the method, or `"unknown"`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        compression_method_name(self.code())
    }
}

impl std::fmt::Display for CompressionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.code(), self.name())
    }
}

// ---------------------------------------------------------------------------------------------
// End of central directory
// ---------------------------------------------------------------------------------------------

/// The located end-of-central-directory record, with zip64 values folded in where present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EndOfCentralDirectory {
    /// File offset of the 22-byte end-of-central-directory record.
    pub offset: u64,
    /// Number of entries in the central directory.
    pub entry_count: u64,
    /// File offset of the first central-directory record.
    pub central_directory_offset: u64,
    /// Length in bytes of the central directory.
    pub central_directory_size: u64,
    /// Length of the archive comment that follows the record.
    pub comment_len: u16,
    /// True when a zip64 end-of-central-directory record supplied the values above.
    pub zip64: bool,
    /// File offset of the zip64 end-of-central-directory record, when there is one.
    pub zip64_record_offset: Option<u64>,
}

/// The values read out of the 22-byte record, before zip64 is consulted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LegacyEocd {
    pub(crate) offset: u64,
    pub(crate) this_disk: u16,
    pub(crate) central_directory_disk: u16,
    pub(crate) entry_count: u16,
    pub(crate) central_directory_size: u32,
    pub(crate) central_directory_offset: u32,
    pub(crate) comment_len: u16,
    /// File offset of the zip64 locator, when one sits immediately before the record.
    pub(crate) zip64_record_offset: Option<u64>,
}

impl LegacyEocd {
    /// True when any field is saturated, i.e. its real value lives in the zip64 record.
    pub(crate) fn needs_zip64(&self) -> bool {
        self.this_disk == u16::MAX
            || self.central_directory_disk == u16::MAX
            || self.entry_count == u16::MAX
            || self.central_directory_size == u32::MAX
            || self.central_directory_offset == u32::MAX
    }
}

/// Find the end-of-central-directory record inside the tail of the file.
///
/// `tail` must be the last `tail.len()` bytes of the file and `tail_base` their file offset. The
/// scan runs backwards and accepts the first candidate whose comment length reaches **exactly**
/// the end of the file, which is what makes a stray `PK\x05\x06` inside compressed data
/// harmless.
///
/// The zip64 locator is only ever looked for in the 20 bytes **immediately before** the accepted
/// record, never by scanning: `docs/research/apk-analysis.md` §1.1 records that a naive search for
/// `PK\x06\x06` in this APK hits a false positive inside compressed data.
pub(crate) fn find_end_of_central_directory(
    tail: &[u8],
    tail_base: u64,
    path: &str,
) -> ApkResult<LegacyEocd> {
    if tail.len() < EOCD_LEN {
        return Err(ApkError::NoEndOfCentralDirectory {
            path: path.to_owned(),
            searched: tail.len() as u64,
        });
    }

    let sig = SIG_EOCD.to_le_bytes();
    for start in (0..=tail.len() - EOCD_LEN).rev() {
        if tail[start..start + 4] != sig {
            continue;
        }
        let comment_len = u16::from_le_bytes([tail[start + 20], tail[start + 21]]) as usize;
        if start + EOCD_LEN + comment_len != tail.len() {
            continue;
        }

        let offset = tail_base + start as u64;
        let mut f = Fields::new(
            &tail[start..start + EOCD_LEN],
            offset,
            "end-of-central-directory record",
        );
        f.signature(SIG_EOCD)?;
        let this_disk = f.u16("disk number")?;
        let central_directory_disk = f.u16("disk of central directory")?;
        let _entries_this_disk = f.u16("entries on this disk")?;
        let entry_count = f.u16("total entries")?;
        let central_directory_size = f.u32("central directory size")?;
        let central_directory_offset = f.u32("central directory offset")?;

        // The locator, if there is one, is the 20 bytes immediately before the record.
        let zip64_record_offset = if start >= ZIP64_LOCATOR_LEN {
            let loc = &tail[start - ZIP64_LOCATOR_LEN..start];
            if loc[..4] == SIG_ZIP64_LOCATOR.to_le_bytes() {
                let mut lf = Fields::new(
                    loc,
                    offset - ZIP64_LOCATOR_LEN as u64,
                    "zip64 end-of-central-directory locator",
                );
                lf.signature(SIG_ZIP64_LOCATOR)?;
                let _disk = lf.u32("disk of zip64 record")?;
                Some(lf.u64("zip64 record offset")?)
            } else {
                None
            }
        } else {
            None
        };

        return Ok(LegacyEocd {
            offset,
            this_disk,
            central_directory_disk,
            entry_count,
            central_directory_size,
            central_directory_offset,
            comment_len: comment_len as u16,
            zip64_record_offset,
        });
    }

    Err(ApkError::NoEndOfCentralDirectory {
        path: path.to_owned(),
        searched: tail.len() as u64,
    })
}

/// Fold a zip64 end-of-central-directory record into the 32-bit one.
///
/// Where the 32-bit record did **not** mark a field as overflowed, the two must agree; a
/// disagreement means the archive is inconsistent and is reported rather than silently resolved
/// in either direction.
pub(crate) fn apply_zip64(
    legacy: &LegacyEocd,
    record: &[u8],
    record_offset: u64,
) -> ApkResult<EndOfCentralDirectory> {
    let mut f = Fields::new(record, record_offset, "zip64 end-of-central-directory record");
    f.signature(SIG_ZIP64_EOCD)?;
    let _record_size = f.u64("record size")?;
    let _version_made_by = f.u16("version made by")?;
    let _version_needed = f.u16("version needed")?;
    let this_disk = f.u32("disk number")?;
    let central_directory_disk = f.u32("disk of central directory")?;
    let _entries_this_disk = f.u64("entries on this disk")?;
    let entry_count = f.u64("total entries")?;
    let central_directory_size = f.u64("central directory size")?;
    let central_directory_offset = f.u64("central directory offset")?;

    if this_disk != central_directory_disk {
        return Err(ApkError::MultiDisk {
            structure: "the zip64 end-of-central-directory record",
            this_disk: u64::from(this_disk),
            central_directory_disk: u64::from(central_directory_disk),
        });
    }

    let check = |what: &'static str, zip64: u64, legacy_value: u64, saturated: bool| {
        if !saturated && zip64 != legacy_value {
            Err(ApkError::Zip64Disagreement {
                zip64_offset: record_offset,
                what,
                zip64,
                legacy: legacy_value,
            })
        } else {
            Ok(())
        }
    };
    check(
        "the entry count",
        entry_count,
        u64::from(legacy.entry_count),
        legacy.entry_count == u16::MAX,
    )?;
    check(
        "the central directory size",
        central_directory_size,
        u64::from(legacy.central_directory_size),
        legacy.central_directory_size == u32::MAX,
    )?;
    check(
        "the central directory offset",
        central_directory_offset,
        u64::from(legacy.central_directory_offset),
        legacy.central_directory_offset == u32::MAX,
    )?;

    Ok(EndOfCentralDirectory {
        offset: legacy.offset,
        entry_count,
        central_directory_offset,
        central_directory_size,
        comment_len: legacy.comment_len,
        zip64: true,
        zip64_record_offset: Some(record_offset),
    })
}

/// Use the 32-bit values as they stand, having established that none is saturated.
pub(crate) fn without_zip64(legacy: &LegacyEocd) -> ApkResult<EndOfCentralDirectory> {
    if legacy.this_disk != legacy.central_directory_disk {
        return Err(ApkError::MultiDisk {
            structure: "the end-of-central-directory record",
            this_disk: u64::from(legacy.this_disk),
            central_directory_disk: u64::from(legacy.central_directory_disk),
        });
    }
    Ok(EndOfCentralDirectory {
        offset: legacy.offset,
        entry_count: u64::from(legacy.entry_count),
        central_directory_offset: u64::from(legacy.central_directory_offset),
        central_directory_size: u64::from(legacy.central_directory_size),
        comment_len: legacy.comment_len,
        zip64: false,
        zip64_record_offset: legacy.zip64_record_offset,
    })
}

// ---------------------------------------------------------------------------------------------
// Central directory
// ---------------------------------------------------------------------------------------------

/// One central-directory record: everything about an entry except where its payload starts.
#[derive(Debug, Clone)]
pub(crate) struct CentralRecord {
    pub(crate) name: String,
    pub(crate) flags: u16,
    pub(crate) method: CompressionMethod,
    pub(crate) crc32: u32,
    pub(crate) compressed_size: u64,
    pub(crate) uncompressed_size: u64,
    pub(crate) local_header_offset: u64,
    pub(crate) central_record_offset: u64,
}

/// Parse `count` central-directory records out of `bytes`, which start at file offset `base`.
///
/// `count` is attacker-controlled and, on the zip64 path, is a full `u64` rather than something the
/// format caps at 65,535 — so it is never used as an allocation size directly. A record cannot be
/// shorter than its 46 fixed bytes, so `bytes.len() / 46` is a hard ceiling on how many can
/// actually be there, and reserving beyond that could only ever be wasted. A `count` that exceeds
/// the ceiling still fails, one line later, with [`ApkError::CentralDirectoryEntryCount`] naming
/// both numbers.
pub(crate) fn parse_central_directory(
    bytes: &[u8],
    base: u64,
    count: u64,
) -> ApkResult<Vec<CentralRecord>> {
    let ceiling = bytes.len() / CENTRAL_RECORD_LEN;
    let capacity = usize::try_from(count).unwrap_or(usize::MAX).min(ceiling);
    let mut records = Vec::with_capacity(capacity);
    let mut f = Fields::new(bytes, base, "central-directory record");

    while records.len() as u64 != count {
        if f.remaining() == 0 {
            break;
        }
        let central_record_offset = f.offset();
        f.signature(SIG_CENTRAL_RECORD)?;
        let _version_made_by = f.u16("version made by")?;
        let _version_needed = f.u16("version needed")?;
        let flags = f.u16("general purpose flags")?;
        let method = CompressionMethod::from_code(f.u16("compression method")?);
        let _mod_time = f.u16("last modified time")?;
        let _mod_date = f.u16("last modified date")?;
        let crc32 = f.u32("crc-32")?;
        let compressed_size_32 = f.u32("compressed size")?;
        let uncompressed_size_32 = f.u32("uncompressed size")?;
        let name_len = f.u16("file name length")? as usize;
        let extra_len = f.u16("extra field length")? as usize;
        let comment_len = f.u16("file comment length")? as usize;
        let disk_start = f.u16("disk number start")?;
        let _internal_attrs = f.u16("internal file attributes")?;
        let _external_attrs = f.u32("external file attributes")?;
        let local_header_offset_32 = f.u32("local header offset")?;

        let name_offset = f.offset();
        let name = decode_name(f.take(name_len, "file name")?, name_offset)?;
        let extra = f.take(extra_len, "extra field")?;
        let _comment = f.take(comment_len, "file comment")?;

        if disk_start != 0 && disk_start != u16::MAX {
            return Err(ApkError::MultiDisk {
                structure: "a central-directory record",
                this_disk: 0,
                central_directory_disk: u64::from(disk_start),
            });
        }

        let (uncompressed_size, compressed_size, local_header_offset) = resolve_zip64_sizes(
            &name,
            extra,
            uncompressed_size_32,
            compressed_size_32,
            local_header_offset_32,
        )?;

        records.push(CentralRecord {
            name,
            flags,
            method,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
            central_record_offset,
        });
    }

    if records.len() as u64 != count {
        return Err(ApkError::CentralDirectoryEntryCount {
            declared: count,
            parsed: records.len() as u64,
            offset: base,
            size: bytes.len() as u64,
        });
    }
    Ok(records)
}

/// Replace any saturated 32-bit size or offset with the 64-bit value from the zip64 extra field.
///
/// The zip64 extended-information field carries only the values that overflowed, in a fixed order
/// (uncompressed size, compressed size, local header offset, disk start), so which values are
/// present is decided entirely by which 32-bit fields read `0xFFFFFFFF`.
fn resolve_zip64_sizes(
    name: &str,
    extra: &[u8],
    uncompressed_size_32: u32,
    compressed_size_32: u32,
    local_header_offset_32: u32,
) -> ApkResult<(u64, u64, u64)> {
    let wants_uncompressed = uncompressed_size_32 == u32::MAX;
    let wants_compressed = compressed_size_32 == u32::MAX;
    let wants_offset = local_header_offset_32 == u32::MAX;

    if !(wants_uncompressed || wants_compressed || wants_offset) {
        return Ok((
            u64::from(uncompressed_size_32),
            u64::from(compressed_size_32),
            u64::from(local_header_offset_32),
        ));
    }

    let needed =
        8 * (usize::from(wants_uncompressed) + usize::from(wants_compressed) + usize::from(wants_offset));
    let payload = find_extra_field(extra, EXTRA_ID_ZIP64).ok_or_else(|| ApkError::Zip64ExtraTooShort {
        name: name.to_owned(),
        len: 0,
        needed,
    })?;
    if payload.len() < needed {
        return Err(ApkError::Zip64ExtraTooShort {
            name: name.to_owned(),
            len: payload.len(),
            needed,
        });
    }

    let mut f = Fields::new(payload, 0, "zip64 extended information extra field");
    let uncompressed_size = if wants_uncompressed {
        f.u64("uncompressed size")?
    } else {
        u64::from(uncompressed_size_32)
    };
    let compressed_size = if wants_compressed {
        f.u64("compressed size")?
    } else {
        u64::from(compressed_size_32)
    };
    let local_header_offset = if wants_offset {
        f.u64("local header offset")?
    } else {
        u64::from(local_header_offset_32)
    };
    Ok((uncompressed_size, compressed_size, local_header_offset))
}

/// Find the payload of one `(id, len, payload)` record inside an extra field.
pub(crate) fn find_extra_field(extra: &[u8], id: u16) -> Option<&[u8]> {
    let mut pos = 0usize;
    while pos + 4 <= extra.len() {
        let field_id = u16::from_le_bytes([extra[pos], extra[pos + 1]]);
        let len = u16::from_le_bytes([extra[pos + 2], extra[pos + 3]]) as usize;
        let start = pos + 4;
        let end = start.checked_add(len)?;
        if end > extra.len() {
            return None;
        }
        if field_id == id {
            return Some(&extra[start..end]);
        }
        pos = end;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Local file headers
// ---------------------------------------------------------------------------------------------

/// The parts of a local file header that matter: the two lengths that place the payload, and the
/// three values that must not contradict the central directory.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalFileHeader {
    pub(crate) name_len: u16,
    pub(crate) extra_len: u16,
    pub(crate) method: CompressionMethod,
    pub(crate) crc32: u32,
    pub(crate) compressed_size: u32,
    pub(crate) uncompressed_size: u32,
}

impl LocalFileHeader {
    /// Where the payload starts, given where the header starts.
    pub(crate) fn payload_offset(&self, header_offset: u64) -> u64 {
        header_offset
            + LOCAL_FILE_HEADER_LEN as u64
            + u64::from(self.name_len)
            + u64::from(self.extra_len)
    }
}

/// Parse the 30 fixed bytes of a local file header.
pub(crate) fn parse_local_file_header(bytes: &[u8], offset: u64) -> ApkResult<LocalFileHeader> {
    let mut f = Fields::new(bytes, offset, "local file header");
    f.signature(SIG_LOCAL_FILE_HEADER)?;
    let _version_needed = f.u16("version needed")?;
    let _flags = f.u16("general purpose flags")?;
    let method = CompressionMethod::from_code(f.u16("compression method")?);
    let _mod_time = f.u16("last modified time")?;
    let _mod_date = f.u16("last modified date")?;
    let crc32 = f.u32("crc-32")?;
    let compressed_size = f.u32("compressed size")?;
    let uncompressed_size = f.u32("uncompressed size")?;
    let name_len = f.u16("file name length")?;
    let extra_len = f.u16("extra field length")?;
    Ok(LocalFileHeader {
        name_len,
        extra_len,
        method,
        crc32,
        compressed_size,
        uncompressed_size,
    })
}

/// Cross-check a local file header against the central-directory record that points at it.
///
/// Sizes and CRC-32 are only compared when the local header actually carries them: with
/// general-purpose flag bit 3 set they are zero by design and the real values are in a trailing
/// data descriptor.
pub(crate) fn check_local_against_central(
    record: &CentralRecord,
    local: &LocalFileHeader,
    local_name: &str,
) -> ApkResult<()> {
    let offset = record.local_header_offset;
    if local_name != record.name {
        return Err(ApkError::LocalNameMismatch {
            name: record.name.clone(),
            local_name: local_name.to_owned(),
            offset,
        });
    }
    if local.method != record.method {
        return Err(ApkError::LocalFieldMismatch {
            name: record.name.clone(),
            offset,
            what: "the compression method",
            local: u64::from(local.method.code()),
            central: u64::from(record.method.code()),
        });
    }

    let deferred = record.flags & FLAG_DATA_DESCRIPTOR != 0;
    let compare = |what: &'static str, local_value: u64, central: u64| -> ApkResult<()> {
        if local_value == 0 && (deferred || central == 0) {
            return Ok(());
        }
        if local_value != central {
            return Err(ApkError::LocalFieldMismatch {
                name: record.name.clone(),
                offset,
                what,
                local: local_value,
                central,
            });
        }
        Ok(())
    };
    compare("the CRC-32", u64::from(local.crc32), u64::from(record.crc32))?;
    // A zip64 entry stores 0xFFFFFFFF in the local header's 32-bit size fields too; the real
    // values are in the local header's own zip64 extra field, which we do not need because the
    // central directory already gave us them.
    if local.compressed_size != u32::MAX {
        compare(
            "the compressed size",
            u64::from(local.compressed_size),
            record.compressed_size,
        )?;
    }
    if local.uncompressed_size != u32::MAX {
        compare(
            "the uncompressed size",
            u64::from(local.uncompressed_size),
            record.uncompressed_size,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Entries
// ---------------------------------------------------------------------------------------------

/// One entry of an APK, resolved far enough to be read or mapped.
///
/// Construction is internal: an entry only exists once its local file header has been read and
/// cross-checked, so [`payload_offset`](Self::payload_offset) is never a guess.
#[derive(Debug, Clone)]
pub struct ZipEntry {
    pub(crate) name: String,
    pub(crate) method: CompressionMethod,
    pub(crate) flags: u16,
    pub(crate) crc32: u32,
    pub(crate) compressed_size: u64,
    pub(crate) uncompressed_size: u64,
    pub(crate) local_header_offset: u64,
    pub(crate) local_extra_len: u16,
    pub(crate) payload_offset: u64,
    pub(crate) central_record_offset: u64,
}

impl ZipEntry {
    /// The entry's name, exactly as spelled in the central directory.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How the payload is compressed.
    #[must_use]
    pub const fn method(&self) -> CompressionMethod {
        self.method
    }

    /// True when the payload is the file's bytes verbatim (method 0).
    #[must_use]
    pub const fn is_stored(&self) -> bool {
        matches!(self.method, CompressionMethod::Stored)
    }

    /// True when the payload is a raw deflate stream (method 8).
    #[must_use]
    pub const fn is_deflated(&self) -> bool {
        matches!(self.method, CompressionMethod::Deflated)
    }

    /// The entry's general-purpose flag word.
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.flags
    }

    /// The CRC-32 of the *uncompressed* bytes, from the central directory.
    #[must_use]
    pub const fn crc32(&self) -> u32 {
        self.crc32
    }

    /// How many bytes the payload occupies in the archive.
    #[must_use]
    pub const fn compressed_size(&self) -> u64 {
        self.compressed_size
    }

    /// How many bytes the entry is once decompressed.
    ///
    /// This is a *claim* made by the central directory, not a measurement. It is checked against
    /// what the payload actually produces on every read, and
    /// [`plausible_uncompressed_size`](Self::plausible_uncompressed_size) is what should be used to
    /// size a buffer.
    #[must_use]
    pub const fn uncompressed_size(&self) -> u64 {
        self.uncompressed_size
    }

    /// The declared uncompressed size, clamped to what the payload that is actually present could
    /// possibly produce.
    ///
    /// A hostile archive can declare any size it likes in 8 bytes of central directory —
    /// 70,368,744,177,664 out of a 200-byte file, say — so the declared size must never reach an
    /// allocator unclamped. A STORED entry can only produce its own payload; a DEFLATED one can
    /// produce at most [`MAX_DEFLATE_EXPANSION`] times it. Since the payload is bounds-checked
    /// against the file length when the archive is opened, this is bounded by the file that exists.
    #[must_use]
    pub fn plausible_uncompressed_size(&self) -> u64 {
        let ceiling = match self.method {
            CompressionMethod::Stored => self.compressed_size,
            _ => self.compressed_size.saturating_mul(MAX_DEFLATE_EXPANSION),
        };
        self.uncompressed_size.min(ceiling)
    }

    /// File offset of the entry's local file header.
    #[must_use]
    pub const fn local_header_offset(&self) -> u64 {
        self.local_header_offset
    }

    /// File offset of the entry's central-directory record.
    ///
    /// Carried so that a disagreement between the two copies of an entry's metadata can be
    /// hex-dumped from both ends.
    #[must_use]
    pub const fn central_record_offset(&self) -> u64 {
        self.central_record_offset
    }

    /// Length of the local file header's extra field.
    ///
    /// Exposed because it is the whole of `zipalign`'s mechanism: alignment padding is inserted
    /// here and nowhere else, so this number explains the payload offset.
    #[must_use]
    pub const fn local_extra_len(&self) -> u16 {
        self.local_extra_len
    }

    /// **File offset of the entry's payload.** Read from the local file header, not the central
    /// directory.
    #[must_use]
    pub const fn payload_offset(&self) -> u64 {
        self.payload_offset
    }

    /// True when the payload offset is a multiple of `alignment`.
    ///
    /// `alignment` must be a non-zero power of two; anything else is not an alignment and returns
    /// `false` rather than panicking, since the value often comes from a runtime page-size query.
    #[must_use]
    pub const fn is_payload_aligned(&self, alignment: u64) -> bool {
        if alignment == 0 || !alignment.is_power_of_two() {
            return false;
        }
        self.payload_offset % alignment == 0
    }

    /// The largest power of two that divides the payload offset, capped at
    /// [`MAX_INTERESTING_ALIGNMENT`].
    #[must_use]
    pub fn payload_alignment(&self) -> u64 {
        let cap = MAX_INTERESTING_ALIGNMENT.trailing_zeros();
        1u64 << self.payload_offset.trailing_zeros().min(cap)
    }

    /// **Whether this entry can be mapped straight out of the APK** (D11).
    ///
    /// True only when the entry is STORED *and* its payload begins on a
    /// [`MAPPING_ALIGNMENT`]-byte boundary. That is what `zipalign -p -f 4` produces and it is the
    /// fast path: such an entry needs no extraction cache at all, because a file-backed view of
    /// the APK itself already presents its bytes at the right offsets.
    ///
    /// None of the 11 `.so` in `Roblox-2.738.1397.apk` satisfies this — they are DEFLATED and only
    /// 4-byte aligned — which is the measurement that forced the extraction cache.
    #[must_use]
    pub const fn is_directly_mappable(&self) -> bool {
        self.is_stored() && self.is_payload_aligned(MAPPING_ALIGNMENT)
    }

    /// Verify a buffer against this entry's recorded CRC-32.
    ///
    /// Public because verification is not only useful on the read path: anything that has held an
    /// entry's bytes across an untrusted boundary — a cache file, a mapping, an IPC hop — can
    /// re-check them against the archive's own record.
    pub fn verify_crc32(&self, bytes: &[u8]) -> ApkResult<()> {
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(bytes);
        let actual = hasher.finalize();
        if actual != self.crc32 {
            return Err(ApkError::CrcMismatch {
                name: self.name.clone(),
                expected: self.crc32,
                actual,
                len: bytes.len() as u64,
            });
        }
        Ok(())
    }

    /// The aligned window of the APK that covers this entry's STORED payload.
    ///
    /// The asset fast path. A STORED entry that is *not* page-aligned still needs no copy: map
    /// from the aligned offset below it and skip [`MapWindow::payload_delta`] bytes. That works
    /// for assets, whose bytes are consumed as a buffer, but not for ELF segments, whose file
    /// offsets must land at specific addresses — hence D11.
    ///
    /// Returns `None` for a non-STORED entry, or for an `alignment` that is zero or not a power of
    /// two.
    #[must_use]
    pub fn stored_map_window(&self, alignment: u64) -> Option<MapWindow> {
        if !self.is_stored() || alignment == 0 || !alignment.is_power_of_two() {
            return None;
        }
        let file_offset = self.payload_offset & !(alignment - 1);
        let payload_delta = self.payload_offset - file_offset;
        Some(MapWindow {
            file_offset,
            payload_delta,
            len: payload_delta + self.compressed_size,
        })
    }
}

/// An aligned file window that covers a STORED entry's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapWindow {
    /// Aligned file offset to map from.
    pub file_offset: u64,
    /// How far into the mapped window the payload starts.
    pub payload_delta: u64,
    /// How many bytes to map, counting from `file_offset`.
    pub len: u64,
}
