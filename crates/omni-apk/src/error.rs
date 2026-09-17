//! Typed, diagnostic errors for APK and zip reading.
//!
//! Every variant names what failed and the values it failed with (Global Constraint 7). For a
//! container format that means, almost always, a **file offset**: "bad central directory
//! signature" is useless, "bad central-directory-record signature at file offset 159629312: found
//! 0x02014b51, expected 0x02014b50" tells you where to put the hex dump.
//!
//! There is deliberately no catch-all variant and no blanket `From<io::Error>`: an I/O failure
//! must say which file and which operation produced it, so [`ApkError::io`] is the only way to
//! build one.

use std::fmt;
use std::path::Path;

/// Result alias for every fallible operation in this crate.
pub type ApkResult<T> = Result<T, ApkError>;

/// Everything that can go wrong reading an APK or maintaining the extraction cache.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApkError {
    /// An operating-system I/O call failed.
    #[error("{operation} failed on {path}: {source}")]
    Io {
        /// The operation attempted, e.g. `"read the central directory"`.
        operation: &'static str,
        /// The file it was attempted on.
        path: String,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// The file is too short to be a zip at all.
    #[error(
        "{path} is {len} bytes long, which is too short to be a zip: the \
         end-of-central-directory record alone is {minimum} bytes"
    )]
    TooShort {
        /// The file in question.
        path: String,
        /// Its length.
        len: u64,
        /// The smallest possible zip size.
        minimum: u64,
    },

    /// No end-of-central-directory record was found where one must be.
    #[error(
        "no end-of-central-directory record (PK\\x05\\x06 with a consistent comment length) in \
         the last {searched} bytes of {path}: not a zip or APK"
    )]
    NoEndOfCentralDirectory {
        /// The file in question.
        path: String,
        /// How many bytes from the end were searched.
        searched: u64,
    },

    /// A fixed-size structure ran off the end of the bytes available for it.
    #[error(
        "truncated {structure} at file offset {offset}: needed {needed} more bytes for {field} \
         but only {available} remain"
    )]
    Truncated {
        /// The structure being parsed, e.g. `"central-directory record"`.
        structure: &'static str,
        /// Absolute file offset of the field that could not be read.
        offset: u64,
        /// Which field could not be read.
        field: &'static str,
        /// Bytes required.
        needed: usize,
        /// Bytes available.
        available: usize,
    },

    /// A structure did not start with the four-byte signature it must start with.
    #[error(
        "bad {structure} signature at file offset {offset}: found {found:#010x}, \
         expected {expected:#010x}"
    )]
    BadSignature {
        /// The structure being parsed.
        structure: &'static str,
        /// Absolute file offset of the signature.
        offset: u64,
        /// What was there.
        found: u32,
        /// What had to be there.
        expected: u32,
    },

    /// The archive is split across disks. Nothing in Android produces these and we do not read
    /// them.
    #[error(
        "multi-disk archives are not supported: {structure} reports this disk as {this_disk} and \
         the central directory as starting on disk {central_directory_disk}"
    )]
    MultiDisk {
        /// Which record said so.
        structure: &'static str,
        /// The disk this record is on.
        this_disk: u64,
        /// The disk the central directory starts on.
        central_directory_disk: u64,
    },

    /// The central directory does not lie inside the file.
    #[error(
        "the central directory is {size} bytes at file offset {offset}, ending at {end}, which is \
         past the end of {path} ({len} bytes)"
    )]
    CentralDirectoryOutOfBounds {
        /// The file in question.
        path: String,
        /// Where the central directory claims to start.
        offset: u64,
        /// How long it claims to be.
        size: u64,
        /// `offset + size`.
        end: u64,
        /// The real file length.
        len: u64,
    },

    /// The central directory did not contain the number of records it was declared to.
    #[error(
        "the end-of-central-directory record declares {declared} entries but {parsed} were parsed \
         from the {size} bytes of central directory at file offset {offset}"
    )]
    CentralDirectoryEntryCount {
        /// Entry count from the (zip64) end-of-central-directory record.
        declared: u64,
        /// Entry count actually parsed.
        parsed: u64,
        /// Central directory offset.
        offset: u64,
        /// Central directory size.
        size: u64,
    },

    /// An entry name was not valid UTF-8.
    ///
    /// Zip permits either CP437 or UTF-8 names (general-purpose flag bit 11). Android tooling
    /// always writes UTF-8, so a name that is not UTF-8 is a corrupt or hostile archive rather
    /// than a legacy one, and is rejected along with the bytes that failed.
    #[error("the entry name at file offset {offset} is not valid UTF-8 ({lossy:?}): {source}")]
    NameNotUtf8 {
        /// Absolute file offset of the name bytes.
        offset: u64,
        /// The name with invalid sequences replaced, for eyeballing.
        lossy: String,
        /// Where the decode failed.
        #[source]
        source: std::str::Utf8Error,
    },

    /// A zip64 extended-information extra field was too short for the values it must carry.
    #[error(
        "the zip64 extra field of `{name}` is {len} bytes, too short for the {needed} bytes of \
         64-bit values its central-directory record marks as 0xFFFFFFFF"
    )]
    Zip64ExtraTooShort {
        /// The entry whose extra field is short.
        name: String,
        /// The length of the zip64 extra field payload.
        len: usize,
        /// How many bytes were needed.
        needed: usize,
    },

    /// The zip64 end-of-central-directory record contradicts the 32-bit one.
    #[error(
        "the zip64 end-of-central-directory record at file offset {zip64_offset} says {what} is \
         {zip64}, but the end-of-central-directory record says {legacy} and did not mark that \
         field as overflowed"
    )]
    Zip64Disagreement {
        /// Offset of the zip64 record.
        zip64_offset: u64,
        /// The field that disagrees.
        what: &'static str,
        /// The zip64 value.
        zip64: u64,
        /// The 32-bit value.
        legacy: u64,
    },

    /// A field of the end-of-central-directory record was saturated but there is no zip64 record.
    #[error(
        "the end-of-central-directory record at file offset {eocd_offset} marks a field as \
         overflowed (0xFFFF/0xFFFFFFFF) but there is no zip64 end-of-central-directory locator in \
         the 20 bytes before it, so the real value cannot be recovered"
    )]
    Zip64LocatorMissing {
        /// Offset of the end-of-central-directory record.
        eocd_offset: u64,
    },

    /// Writing a decompressed entry to its destination failed.
    #[error("writing `{name}` to {destination} failed after {written} of {expected} bytes: {source}")]
    Sink {
        /// The entry being written.
        name: String,
        /// Where it was being written, as the caller described it.
        destination: String,
        /// How many bytes had been written.
        written: u64,
        /// How many bytes there were in total.
        expected: u64,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// A local file header named a different entry than the central-directory record that pointed
    /// at it.
    #[error(
        "the local file header at file offset {offset} names `{local_name}`, but the \
         central-directory record that points at it names `{name}`"
    )]
    LocalNameMismatch {
        /// The name from the central directory.
        name: String,
        /// The name from the local file header.
        local_name: String,
        /// Offset of the local file header.
        offset: u64,
    },

    /// A local file header disagreed with the central directory about a field that must match.
    #[error(
        "the local file header of `{name}` at file offset {offset} says {what} is {local}, but \
         the central directory says {central}"
    )]
    LocalFieldMismatch {
        /// The entry.
        name: String,
        /// Offset of the local file header.
        offset: u64,
        /// The disagreeing field.
        what: &'static str,
        /// The local file header's value.
        local: u64,
        /// The central directory's value.
        central: u64,
    },

    /// An entry's payload does not lie inside the file.
    #[error(
        "`{name}`: its {compressed_size}-byte payload starts at file offset {payload_offset} and \
         ends at {end}, past the end of {path} ({len} bytes)"
    )]
    PayloadOutOfBounds {
        /// The entry.
        name: String,
        /// The file.
        path: String,
        /// Payload start.
        payload_offset: u64,
        /// Payload length in the archive.
        compressed_size: u64,
        /// `payload_offset + compressed_size`.
        end: u64,
        /// The real file length.
        len: u64,
    },

    /// No entry with that name exists.
    #[error("{path} has no entry named `{name}`")]
    EntryNotFound {
        /// The name looked up.
        name: String,
        /// The archive searched.
        path: String,
    },

    /// The entry uses a compression method this crate cannot read.
    #[error(
        "`{name}` uses compression method {method} ({method_name}); omni-apk reads only \
         0 (STORED) and 8 (DEFLATED), the only two methods Android tooling produces"
    )]
    UnsupportedCompressionMethod {
        /// The entry.
        name: String,
        /// The method code from the central directory.
        method: u16,
        /// Its name, where the code is one from APPNOTE.TXT.
        method_name: &'static str,
    },

    /// The bytes read did not hash to the CRC-32 the central directory records.
    #[error(
        "CRC-32 mismatch for `{name}`: the central directory records {expected:#010x} but the \
         {len} bytes read hash to {actual:#010x}"
    )]
    CrcMismatch {
        /// The entry.
        name: String,
        /// CRC-32 from the central directory.
        expected: u32,
        /// CRC-32 of what was actually read.
        actual: u32,
        /// How many bytes were hashed.
        len: u64,
    },

    /// Decompression produced a different number of bytes than the entry declares.
    #[error(
        "`{name}` produced {actual} bytes but the central directory says its uncompressed size is \
         {expected}: the payload is truncated or the central directory is wrong"
    )]
    UncompressedSizeMismatch {
        /// The entry.
        name: String,
        /// The declared uncompressed size.
        expected: u64,
        /// What was actually produced.
        actual: u64,
    },

    /// The entry is larger than this process can hold in one allocation.
    #[error(
        "`{name}` is {size} bytes, which cannot be held in memory on this target (usize is \
         {pointer_width} bits); stream it instead of reading it into a buffer"
    )]
    TooLargeForAddressSpace {
        /// The entry.
        name: String,
        /// Its uncompressed size.
        size: u64,
        /// The width of `usize` on this target.
        pointer_width: u32,
    },

    /// Memory to hold an entry could not be reserved.
    ///
    /// A declared uncompressed size is attacker-controlled input, so it is never handed to an
    /// infallible allocator. Between the plausibility ceiling that bounds the request and
    /// `Vec::try_reserve_exact`, a lying archive produces this error instead of aborting the
    /// process — which matters because an allocation abort is not a panic and no caller can
    /// contain it.
    #[error("could not reserve {bytes} bytes to hold `{name}`: {source}")]
    Allocation {
        /// The entry being read.
        name: String,
        /// How many bytes were asked for.
        bytes: usize,
        /// The allocator's complaint.
        #[source]
        source: std::collections::TryReserveError,
    },

    /// The deflate stream was malformed.
    #[error("inflating `{name}` failed after producing {produced} of {expected} bytes: {source}")]
    Inflate {
        /// The entry.
        name: String,
        /// Bytes produced before the failure.
        produced: u64,
        /// Bytes expected in total.
        expected: u64,
        /// The decoder's complaint.
        #[source]
        source: std::io::Error,
    },

    /// The extraction cache was asked to extract something that is not a native library.
    #[error(
        "`{name}` is not a native library: the extraction cache takes entries named \
         `lib/<abi>/<name>.so`"
    )]
    NotANativeLibrary {
        /// The entry offered.
        name: String,
    },

    /// A cache entry existed but was the wrong size, so it could not be trusted.
    ///
    /// [`LibraryCache::extract`](crate::LibraryCache::extract) treats this as a miss and
    /// re-extracts rather than returning it; it is returned by
    /// [`LibraryCache::lookup`](crate::LibraryCache::lookup) so a caller that only wants to know
    /// whether the cache is usable can see that it was not.
    #[error(
        "cache entry {path} is {actual} bytes but `{name}` is {expected} bytes uncompressed: the \
         cache file is truncated or was not written by omni-apk"
    )]
    CacheSizeMismatch {
        /// The cache file.
        path: String,
        /// The entry it was supposed to hold.
        name: String,
        /// The expected size.
        expected: u64,
        /// The size on disk.
        actual: u64,
    },

    /// A finished cache file could not be published under its content-addressed name.
    #[error(
        "could not publish the extraction of `{name}` as {final_path}: renaming {temp_path} \
         failed ({source}), and {final_path} is not present with the expected {expected} bytes \
         either"
    )]
    CachePublish {
        /// The entry being extracted.
        name: String,
        /// The temporary file holding the finished bytes.
        temp_path: String,
        /// The content-addressed destination.
        final_path: String,
        /// The expected size of the destination.
        expected: u64,
        /// Why the rename failed.
        #[source]
        source: std::io::Error,
    },
}

impl ApkError {
    /// Build an [`ApkError::Io`], which is the only way to construct one.
    pub(crate) fn io(
        operation: &'static str,
        path: impl AsRef<Path>,
        source: std::io::Error,
    ) -> Self {
        ApkError::Io {
            operation,
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}

/// Render a path for an error message without borrowing it.
pub(crate) fn show(path: impl AsRef<Path>) -> String {
    path.as_ref().display().to_string()
}

/// The name of a zip compression-method code, for diagnostics.
///
/// Only 0 and 8 are readable; the rest exist so that
/// [`UnsupportedCompressionMethod`](ApkError::UnsupportedCompressionMethod) can say *which*
/// unreadable method was asked for rather than printing a bare number.
#[must_use]
pub const fn compression_method_name(method: u16) -> &'static str {
    match method {
        0 => "STORED",
        1 => "SHRUNK",
        6 => "IMPLODED",
        8 => "DEFLATED",
        9 => "DEFLATE64",
        12 => "BZIP2",
        14 => "LZMA",
        93 => "ZSTD",
        95 => "XZ",
        96 => "JPEG",
        97 => "WAVPACK",
        98 => "PPMD",
        99 => "AES",
        _ => "unknown",
    }
}

/// A 32-byte SHA-256 digest rendered as lowercase hex. Cache keys are spelled this way.
pub(crate) struct Hex32<'a>(pub(crate) &'a [u8; 32]);

impl fmt::Display for Hex32<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
