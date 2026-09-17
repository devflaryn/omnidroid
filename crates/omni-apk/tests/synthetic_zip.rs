//! Tests for the container cases the real APK cannot exercise.
//!
//! `Roblox-2.738.1397.apk` is not zip64, has no page-aligned STORED library, and uses no
//! compression method other than 0 and 8 — so three of the things this crate promises would
//! otherwise be untested. These tests build zips byte by byte to cover them.
//!
//! The builder here is deliberately dumb: it writes the fields out longhand rather than sharing any
//! code with the parser, so a misreading of APPNOTE.TXT cannot cancel itself out.

use std::io::Write;
use std::path::{Path, PathBuf};

use omni_apk::{Apk, ApkError, CompressionMethod, MAPPING_ALIGNMENT};

// -------------------------------------------------------------------------------------------
// A minimal zip writer
// -------------------------------------------------------------------------------------------

struct Entry {
    name: String,
    method: u16,
    flags: u16,
    crc32: u32,
    stored: Vec<u8>,
    uncompressed_size: u64,
    /// Bytes of padding to insert in the local extra field, exactly as `zipalign` does.
    local_padding: u16,
    /// Force the 32-bit central-directory fields to 0xFFFFFFFF and emit a zip64 extra field.
    zip64: bool,
}

impl Entry {
    fn stored(name: &str, data: &[u8]) -> Self {
        Self {
            name: name.to_owned(),
            method: 0,
            flags: 1 << 11,
            crc32: crc32(data),
            stored: data.to_vec(),
            uncompressed_size: data.len() as u64,
            local_padding: 0,
            zip64: false,
        }
    }

    fn deflated(name: &str, data: &[u8]) -> Self {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).expect("deflating");
        Self {
            name: name.to_owned(),
            method: 8,
            flags: 1 << 11,
            crc32: crc32(data),
            stored: encoder.finish().expect("finishing the deflate stream"),
            uncompressed_size: data.len() as u64,
            local_padding: 0,
            zip64: false,
        }
    }

    fn with_method(mut self, method: u16) -> Self {
        self.method = method;
        self
    }

    fn with_crc32(mut self, crc32: u32) -> Self {
        self.crc32 = crc32;
        self
    }

    fn with_uncompressed_size(mut self, size: u64) -> Self {
        self.uncompressed_size = size;
        self
    }

    fn with_local_padding(mut self, padding: u16) -> Self {
        self.local_padding = padding;
        self
    }

    fn saturating_to_zip64(mut self) -> Self {
        self.zip64 = true;
        self
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Build a zip. `zip64_eocd` writes a zip64 end-of-central-directory record and locator, with the
/// 32-bit record's fields saturated.
fn build(entries: &[Entry], zip64_eocd: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut local_offsets = Vec::new();

    for entry in entries {
        local_offsets.push(out.len() as u64);
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&entry.flags.to_le_bytes());
        out.extend_from_slice(&entry.method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&entry.crc32.to_le_bytes());
        out.extend_from_slice(&(entry.stored.len() as u32).to_le_bytes());
        out.extend_from_slice(&(entry.uncompressed_size as u32).to_le_bytes());
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&entry.local_padding.to_le_bytes());
        out.extend_from_slice(entry.name.as_bytes());
        // zipalign's padding: an extra field whose contents nobody reads.
        out.resize(out.len() + usize::from(entry.local_padding), 0);
        out.extend_from_slice(&entry.stored);
    }

    let central_directory_offset = out.len() as u64;
    for (entry, local_offset) in entries.iter().zip(&local_offsets) {
        let zip64_extra: Vec<u8> = if entry.zip64 {
            let mut extra = Vec::new();
            extra.extend_from_slice(&0x0001u16.to_le_bytes());
            extra.extend_from_slice(&24u16.to_le_bytes());
            extra.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
            extra.extend_from_slice(&(entry.stored.len() as u64).to_le_bytes());
            extra.extend_from_slice(&local_offset.to_le_bytes());
            extra
        } else {
            Vec::new()
        };

        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&entry.flags.to_le_bytes());
        out.extend_from_slice(&entry.method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&entry.crc32.to_le_bytes());
        if entry.zip64 {
            out.extend_from_slice(&u32::MAX.to_le_bytes());
            out.extend_from_slice(&u32::MAX.to_le_bytes());
        } else {
            out.extend_from_slice(&(entry.stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&(entry.uncompressed_size as u32).to_le_bytes());
        }
        out.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(zip64_extra.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out.extend_from_slice(&0u16.to_le_bytes()); // disk start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
        out.extend_from_slice(&0u32.to_le_bytes()); // external attributes
        if entry.zip64 {
            out.extend_from_slice(&u32::MAX.to_le_bytes());
        } else {
            out.extend_from_slice(&(*local_offset as u32).to_le_bytes());
        }
        out.extend_from_slice(entry.name.as_bytes());
        out.extend_from_slice(&zip64_extra);
    }
    let central_directory_size = out.len() as u64 - central_directory_offset;

    if zip64_eocd {
        let zip64_eocd_offset = out.len() as u64;
        out.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
        out.extend_from_slice(&44u64.to_le_bytes()); // size of the rest of this record
        out.extend_from_slice(&45u16.to_le_bytes()); // version made by
        out.extend_from_slice(&45u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u32.to_le_bytes()); // this disk
        out.extend_from_slice(&0u32.to_le_bytes()); // disk of central directory
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&central_directory_size.to_le_bytes());
        out.extend_from_slice(&central_directory_offset.to_le_bytes());

        out.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // disk of the zip64 record
        out.extend_from_slice(&zip64_eocd_offset.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes()); // total disks
    }

    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk of central directory
    if zip64_eocd {
        out.extend_from_slice(&u16::MAX.to_le_bytes());
        out.extend_from_slice(&u16::MAX.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
        out.extend_from_slice(&u32::MAX.to_le_bytes());
    } else {
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central_directory_size as u32).to_le_bytes());
        out.extend_from_slice(&(central_directory_offset as u32).to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
    out
}

/// A zip on disk that deletes itself.
struct TempZip {
    path: PathBuf,
}

impl TempZip {
    fn new(label: &str, bytes: &[u8]) -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "omni-apk-synthetic-{label}-{}-{}.apk",
            std::process::id(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).expect("writing a synthetic zip");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn open(&self) -> Result<Apk, ApkError> {
        Apk::open(self.path())
    }
}

impl Drop for TempZip {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[test]
fn stored_and_deflated_entries_round_trip() {
    let payload = b"omnidroid".repeat(500);
    let zip = TempZip::new(
        "round-trip",
        &build(
            &[
                Entry::stored("stored.bin", &payload),
                Entry::deflated("deflated.bin", &payload),
            ],
            false,
        ),
    );
    let apk = zip.open().expect("the synthetic zip must open");

    assert_eq!(apk.entries().len(), 2);
    assert!(!apk.end_of_central_directory().zip64);

    let stored = apk.require_entry("stored.bin").expect("stored.bin");
    assert_eq!(stored.method(), CompressionMethod::Stored);
    assert_eq!(stored.compressed_size(), payload.len() as u64);
    assert_eq!(apk.read_entry(stored).expect("reading stored.bin"), payload);

    let deflated = apk.require_entry("deflated.bin").expect("deflated.bin");
    assert_eq!(deflated.method(), CompressionMethod::Deflated);
    assert!(
        deflated.compressed_size() < deflated.uncompressed_size(),
        "the fixture would not be testing inflate otherwise"
    );
    assert_eq!(
        apk.read_entry(deflated).expect("reading deflated.bin"),
        payload
    );
}

/// The fast path the real APK does not have: a STORED entry whose payload lands on a 4 KB boundary
/// is directly mappable, and the predicate says so.
#[test]
fn a_page_aligned_stored_entry_is_directly_mappable() {
    // The first local header is 30 bytes + a 7-byte name, so 4096 - 37 bytes of extra field puts
    // the payload at exactly 4096.
    let name = "aligned";
    let padding = u16::try_from(4096 - 30 - name.len()).expect("padding fits in u16");
    let payload = b"page-aligned payload".repeat(64);
    let zip = TempZip::new(
        "aligned",
        &build(
            &[Entry::stored(name, &payload).with_local_padding(padding)],
            false,
        ),
    );
    let apk = zip.open().expect("the synthetic zip must open");

    let entry = apk.require_entry(name).expect("the aligned entry");
    assert_eq!(entry.payload_offset(), 4096, "payload offset");
    assert_eq!(entry.local_extra_len(), padding);
    assert!(entry.is_stored());
    assert!(entry.is_payload_aligned(MAPPING_ALIGNMENT));
    assert!(entry.is_payload_aligned(4096));
    assert!(
        entry.is_directly_mappable(),
        "a STORED, page-aligned entry is the whole reason the predicate exists"
    );
    assert_eq!(entry.payload_alignment(), 4096);

    // And its map window is degenerate, because there is nothing to skip.
    let window = entry
        .stored_map_window(MAPPING_ALIGNMENT)
        .expect("a STORED entry has a window");
    assert_eq!(window.file_offset, 4096);
    assert_eq!(window.payload_delta, 0);
    assert_eq!(window.len, payload.len() as u64);

    // A DEFLATED entry at the same offset would not be mappable, alignment notwithstanding.
    let zip = TempZip::new(
        "aligned-deflated",
        &build(
            &[Entry::deflated(name, &payload).with_local_padding(padding)],
            false,
        ),
    );
    let apk = zip.open().expect("the synthetic zip must open");
    let entry = apk.require_entry(name).expect("the aligned entry");
    assert_eq!(entry.payload_offset(), 4096);
    assert!(entry.is_payload_aligned(MAPPING_ALIGNMENT));
    assert!(
        !entry.is_directly_mappable(),
        "alignment alone is not enough: compressed bytes are not the file's bytes"
    );
}

/// Zip64 is supported although this APK does not use it: a 32-bit record with saturated fields plus
/// a locator and a zip64 record, and per-entry zip64 extra fields.
#[test]
fn zip64_sizes_and_offsets_are_read_from_the_extra_fields() {
    let payload = b"zip64".repeat(100);
    let zip = TempZip::new(
        "zip64",
        &build(
            &[
                Entry::stored("first.bin", &payload).saturating_to_zip64(),
                Entry::deflated("second.bin", &payload).saturating_to_zip64(),
            ],
            true,
        ),
    );
    let apk = zip.open().expect("the zip64 archive must open");

    let eocd = apk.end_of_central_directory();
    assert!(eocd.zip64, "the zip64 record must have been used");
    assert!(eocd.zip64_record_offset.is_some(), "and located");
    assert_eq!(eocd.entry_count, 2, "from the zip64 record, not the 0xFFFF");
    assert_eq!(apk.entries().len(), 2);

    // Both entries' sizes and local-header offsets are 0xFFFFFFFF in the central-directory record
    // proper, so every number here came out of a zip64 extra field.
    let first = apk.require_entry("first.bin").expect("first.bin");
    assert_eq!(first.uncompressed_size(), 500);
    assert_eq!(first.compressed_size(), 500);
    assert_eq!(first.local_header_offset(), 0);
    assert_eq!(first.payload_offset(), 30 + 9, "30 fixed bytes plus `first.bin`");

    let second = apk.require_entry("second.bin").expect("second.bin");
    assert_eq!(second.uncompressed_size(), 500);
    assert_eq!(
        second.local_header_offset(),
        30 + 9 + 500,
        "after the first entry's header, name and payload"
    );
    assert_eq!(second.payload_offset(), 30 + 9 + 500 + 30 + 10);

    for name in ["first.bin", "second.bin"] {
        let entry = apk.require_entry(name).expect(name);
        assert_eq!(
            apk.read_entry(entry).unwrap_or_else(|e| panic!("{name}: {e}")),
            payload
        );
    }
}

#[test]
fn a_saturated_record_with_no_locator_is_refused() {
    let payload = b"no locator".to_vec();
    // Build a zip64-shaped end record, then cut the locator out from under it.
    let bytes = build(&[Entry::stored("x", &payload).saturating_to_zip64()], true);
    let eocd_start = bytes.len() - 22;
    let mut mangled = bytes[..eocd_start - 20].to_vec();
    mangled.extend_from_slice(&bytes[eocd_start..]);
    let zip = TempZip::new("no-locator", &mangled);

    match zip.open() {
        Err(ApkError::Zip64LocatorMissing { eocd_offset }) => {
            assert_eq!(eocd_offset as usize, mangled.len() - 22);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a saturated record with no locator was accepted"),
    }
}

#[test]
fn a_corrupt_payload_fails_the_crc_check_on_read() {
    let payload = b"the CRC-32 in the central directory is the contract".to_vec();
    let mut entry = Entry::stored("payload.bin", &payload);
    entry.stored[10] ^= 0xff; // corrupt the data, leave the recorded CRC-32 alone
    let zip = TempZip::new("bad-crc", &build(&[entry], false));
    let apk = zip.open().expect("the archive still opens: only the data is wrong");

    let entry = apk.require_entry("payload.bin").expect("payload.bin");
    match apk.read_entry(entry) {
        Err(ApkError::CrcMismatch {
            name,
            expected,
            actual,
            len,
        }) => {
            assert_eq!(name, "payload.bin");
            assert_eq!(expected, crc32(&payload));
            assert_ne!(actual, expected);
            assert_eq!(len, payload.len() as u64);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a corrupt payload was accepted"),
    }
}

#[test]
fn a_lying_uncompressed_size_is_refused() {
    let payload = b"twelve bytes".to_vec();
    let zip = TempZip::new(
        "bad-size",
        &build(
            &[Entry::stored("payload.bin", &payload)
                .with_uncompressed_size(payload.len() as u64 + 1)],
            false,
        ),
    );
    let apk = zip.open().expect("the archive opens");
    let entry = apk.require_entry("payload.bin").expect("payload.bin");
    match apk.read_entry(entry) {
        Err(ApkError::UncompressedSizeMismatch {
            expected, actual, ..
        }) => {
            assert_eq!(expected, payload.len() as u64 + 1);
            assert_eq!(actual, payload.len() as u64);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a lying uncompressed size was accepted"),
    }
}

#[test]
fn an_unreadable_compression_method_is_named_not_guessed() {
    let payload = b"pretend this is zstd".to_vec();
    let zip = TempZip::new(
        "zstd",
        &build(
            &[
                Entry::stored("readable.bin", &payload),
                Entry::stored("exotic.bin", &payload).with_method(93),
            ],
            false,
        ),
    );
    // The archive as a whole is still usable; only the exotic entry is not.
    let apk = zip.open().expect("one exotic entry must not break the archive");
    assert_eq!(
        apk.read_entry(apk.require_entry("readable.bin").expect("readable.bin"))
            .expect("the readable entry"),
        payload
    );

    let entry = apk.require_entry("exotic.bin").expect("exotic.bin");
    assert_eq!(entry.method(), CompressionMethod::Other(93));
    assert_eq!(entry.method().name(), "ZSTD");
    assert!(!entry.is_stored() && !entry.is_deflated());
    assert!(!entry.is_directly_mappable());
    match apk.read_entry(entry) {
        Err(ApkError::UnsupportedCompressionMethod {
            name,
            method,
            method_name,
        }) => {
            assert_eq!(name, "exotic.bin");
            assert_eq!(method, 93);
            assert_eq!(method_name, "ZSTD");
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("method 93 was read somehow"),
    }
}

#[test]
fn a_local_header_that_contradicts_the_central_directory_is_refused() {
    let payload = b"mismatched".to_vec();
    // Central directory says one CRC-32, the local header says another. Neither has flag bit 3, so
    // the local header's copy is authoritative enough to be checked.
    let mut bytes = build(&[Entry::stored("payload.bin", &payload)], false);
    bytes[14] ^= 0xff; // the local header's CRC-32
    let zip = TempZip::new("mismatch", &bytes);
    match zip.open() {
        Err(ApkError::LocalFieldMismatch { name, what, .. }) => {
            assert_eq!(name, "payload.bin");
            assert_eq!(what, "the CRC-32");
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a contradictory local header was accepted"),
    }
}

#[test]
fn a_file_that_is_not_a_zip_is_refused_with_its_name() {
    let zip = TempZip::new("not-a-zip", &b"this is not a zip file".repeat(100));
    match zip.open() {
        Err(ApkError::NoEndOfCentralDirectory { path, searched }) => {
            assert!(path.ends_with(".apk"), "the error must name the file: {path}");
            assert_eq!(searched, 2200);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a text file was opened as a zip"),
    }

    let tiny = TempZip::new("tiny", b"PK");
    match tiny.open() {
        Err(ApkError::TooShort { len, minimum, .. }) => {
            assert_eq!(len, 2);
            assert_eq!(minimum, 22);
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a 2-byte file was opened as a zip"),
    }
}

#[test]
fn an_archive_comment_does_not_hide_the_end_record() {
    let payload = b"commented".to_vec();
    let mut bytes = build(&[Entry::stored("payload.bin", &payload)], false);
    // Give the archive a comment that itself contains a plausible end-of-central-directory
    // signature, which is exactly the false positive a backwards scan has to survive. It survives it
    // by requiring the candidate's comment length to reach the end of the file exactly, which this
    // one's does not.
    let comment: Vec<u8> = [&0x0605_4b50u32.to_le_bytes()[..], &[0u8; 36][..]].concat();
    let comment_len_at = bytes.len() - 2;
    bytes[comment_len_at..].copy_from_slice(&(comment.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&comment);

    let zip = TempZip::new("commented", &bytes);
    let apk = zip.open().expect("a commented archive must open");
    assert_eq!(apk.end_of_central_directory().comment_len, 40);
    assert_eq!(apk.entries().len(), 1);
    assert_eq!(
        apk.read_named("payload.bin").expect("reading the entry"),
        payload
    );
}

#[test]
fn a_crc_can_be_checked_against_an_entry_directly() {
    let payload = b"verifiable".to_vec();
    let zip = TempZip::new(
        "verify",
        &build(&[Entry::stored("payload.bin", &payload).with_crc32(crc32(&payload))], false),
    );
    let apk = zip.open().expect("open");
    let entry = apk.require_entry("payload.bin").expect("payload.bin");
    entry.verify_crc32(&payload).expect("the correct bytes");
    assert!(entry.verify_crc32(b"different").is_err());
    assert!(entry.verify_crc32(&[]).is_err());
}
