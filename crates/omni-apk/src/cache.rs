//! The content-addressed, 4 KB-aligned native-library extraction cache (D11).
//!
//! # Why this exists
//!
//! Omnidroid wants to map a guest `.so` from a file so that its read-only and executable pages are
//! backed by the page cache and shared between instances, instead of costing private commit per
//! launch. Mapping straight out of the APK needs the entry to be STORED at a
//! [`MAPPING_ALIGNMENT`]-aligned offset. In `Roblox-2.738.1397.apk` all 11 libraries are DEFLATED
//! and 4-byte aligned, so not one qualifies. So each library is inflated **once** into a file of
//! its own, where its bytes start at offset 0 and every page-size question answers itself.
//!
//! # Why the key is a content hash
//!
//! The cache path is `<root>/libs/<sha256 of the uncompressed bytes>/<file name>`. Keying on
//! content rather than on APK path means two APKs that ship the same library share one cache entry,
//! and — the part that matters for a modified APK like this one (D6) — a tampered library can never
//! land on top of a stock one, because a different byte is a different path.
//!
//! # How a hit is found without decompressing
//!
//! The content hash is only knowable after decompressing, so a lookup keyed on it alone would
//! decompress 109 MB to discover it did not need to. A small hint index closes the loop:
//! `<root>/index/<probe key>.sha256` holds the content hash, so a probe costs one `open`.
//!
//! The probe key is a SHA-256 over two things, and it needs both:
//!
//! 1. **Which archive.** Its canonicalised path, its length, and its last-modified time.
//! 2. **Which entry in that archive.** Its name, its local-header offset, its compressed and
//!    uncompressed sizes, its CRC-32, and its compression method.
//!
//! Everything in both groups is already in hand — `Apk::open` stats the file and parses the central
//! directory — so the key costs no extra I/O.
//!
//! # Why the key is scoped to one archive
//!
//! Without group 1 the index is a CRC-32-keyed lookup shared by every APK on the machine, and
//! **CRC-32 is linear**: given a tampered library, four bytes anywhere in it can be tuned to
//! restore any target CRC-32, and it can be padded to match the original's compressed and
//! uncompressed lengths exactly. Running the attacker's APK once would then overwrite the hint for
//! that (name, size, CRC-32) triple, and a later run of a **stock** APK would be handed the
//! attacker's file as a cache hit, with length the only thing checked. That would defeat the
//! property this cache exists to provide.
//!
//! Note what does *not* fix it. Re-verifying with [`ZipEntry::verify_crc32`] after mapping checks
//! the very CRC-32 the attacker matched. Hashing a bounded prefix and suffix of the cache file does
//! not work either: a length-matched forgery leaves the middle free, which is exactly where
//! injected code goes. Scoping the key to the archive does fix it — poisoning now requires
//! overwriting the victim's own APK, in place, at an identical length and mtime.
//!
//! Storage sharing is untouched, because the *destination* is still the content hash. Two APKs
//! shipping the same library have different probe keys, so the second one decompresses once and
//! then lands on the file the first one published, through the same race path as any other
//! concurrent extractor.
//!
//! The index remains a **hint**, never an authority: its value must name a file that exists and is
//! exactly the right length, an unparseable or stale entry is ignored, and the file is still named
//! by its own content.
//!
//! Two residual weaknesses, stated rather than hidden. An attacker who can overwrite the victim's
//! APK in place at the same length and mtime can still poison a hint — but such an attacker can
//! simply edit the library in the APK instead, so this is not the weakest link. And on a platform
//! that reports no mtime, or with a path that does not survive `to_string_lossy` injectively, the
//! key degrades to path-and-length.
//!
//! # Atomicity
//!
//! Every publish is write-to-temp, `sync_all`, rename. A reader therefore only ever sees a complete
//! file under a cache path: a crash mid-extraction leaves a `.part` file that nothing looks at, and
//! never a short file under a name that says it is 109,193,800 bytes long. Multiple processes race
//! freely, because the destination path is a function of the content: whoever wins wrote the same
//! bytes, so losing the race is a cache hit rather than an error.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use sha2::{Digest, Sha256};

use crate::apk::{split_lib_path, Apk};
use crate::error::{show, ApkError, ApkResult, Hex32};
use crate::zip::{ZipEntry, MAPPING_ALIGNMENT};

/// Subdirectory of the cache root holding the extracted libraries.
pub const LIBS_DIR: &str = "libs";

/// Subdirectory holding the hint index.
const INDEX_DIR: &str = "index";

/// Subdirectory holding in-progress extractions.
const TMP_DIR: &str = "tmp";

/// Extension of a hint-index file.
const INDEX_SUFFIX: &str = ".sha256";

/// Domain separator for the probe key, so the key can be versioned when its inputs change.
///
/// v2 added the archive's identity — path, length, mtime — and the entry's local-header offset. v1
/// keys are simply never looked up again; a stale v1 index file is inert, and the extraction it
/// pointed at is still a valid, correctly named cache file that a v2 key will find again.
const PROBE_DOMAIN: &[u8] = b"omni-apk extraction-cache probe v2\0";

/// The most an index file may be; 64 hex digits and a newline is 65. Reading a little more means a
/// longer file is recognised as wrong rather than silently truncated to something that parses.
const MAX_INDEX_BYTES: u64 = 128;

/// Write buffer for an extraction. One megabyte, because the interesting case is 109 MB.
const WRITE_BUFFER: usize = 1024 * 1024;

/// Distinguishes temporary files made by this process from each other.
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// What [`LibraryCache::extract`] actually had to do.
///
/// Returned rather than logged because "did this re-extract?" is a correctness question for the
/// cache, and a test that cannot see the answer cannot check it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOutcome {
    /// The library was inflated and published by this call.
    Extracted,
    /// A valid cache entry was already present and was reused untouched.
    Reused,
    /// This call inflated the library, but another extractor published the identical file first,
    /// so that one was kept. Byte-identical by construction: the path is the content hash.
    ReusedAfterRace,
}

impl CacheOutcome {
    /// True when nothing had to be inflated.
    #[must_use]
    pub const fn is_hit(self) -> bool {
        matches!(self, CacheOutcome::Reused)
    }

    /// True when this call did the decompression work, whether or not its output was the copy kept.
    #[must_use]
    pub const fn did_work(self) -> bool {
        matches!(self, CacheOutcome::Extracted | CacheOutcome::ReusedAfterRace)
    }
}

/// A library sitting in the cache, ready to be mapped.
#[derive(Debug, Clone)]
pub struct CachedLibrary {
    path: PathBuf,
    sha256: [u8; 32],
    len: u64,
    outcome: CacheOutcome,
}

impl CachedLibrary {
    /// The path of the extracted library.
    ///
    /// A path and not an open file, deliberately. On Windows a file that will ever be mapped
    /// executable has to be opened `GENERIC_READ | GENERIC_EXECUTE` and sectioned
    /// `PAGE_EXECUTE_READ` from the outset — a view of a read-only section can never be raised to
    /// executable afterwards (D11). Only the caller knows whether it wants an executable mapping,
    /// so only the caller opens the file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The SHA-256 of the library's bytes, which is also the directory it lives in.
    #[must_use]
    pub const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    /// The SHA-256 rendered as lowercase hex: the cache key.
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        Hex32(&self.sha256).to_string()
    }

    /// The length of the extracted library in bytes, equal to the entry's uncompressed size.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// True when the extracted library is empty. Never true for a real ELF.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// What the call that produced this had to do.
    #[must_use]
    pub const fn outcome(&self) -> CacheOutcome {
        self.outcome
    }

    /// Where the library's bytes begin in the cache file: **0**.
    ///
    /// The whole point of the cache. The file is the library and nothing else, so every ELF file
    /// offset is also a cache-file offset, and the payload starts on a boundary of every page size
    /// there is.
    #[must_use]
    pub const fn payload_offset(&self) -> u64 {
        0
    }

    /// True when the payload begins on an `alignment`-byte boundary. Always true for a power of two.
    #[must_use]
    pub const fn is_payload_aligned(&self, alignment: u64) -> bool {
        if alignment == 0 || !alignment.is_power_of_two() {
            return false;
        }
        self.payload_offset() % alignment == 0
    }

    /// True: a cache file is always mappable at [`MAPPING_ALIGNMENT`], which is why it exists.
    #[must_use]
    pub const fn is_directly_mappable(&self) -> bool {
        self.is_payload_aligned(MAPPING_ALIGNMENT)
    }
}

/// A directory of extracted, 4 KB-aligned guest libraries, shared by every Omnidroid instance on
/// the machine.
///
/// Cheap to construct and cheap to clone: it is a path plus the rules above. Nothing is created on
/// disk until something is extracted.
#[derive(Debug, Clone)]
pub struct LibraryCache {
    root: PathBuf,
}

impl LibraryCache {
    /// Name a cache root. Creates nothing.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The cache root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a library with this content hash and file name lives.
    #[must_use]
    pub fn library_path(&self, sha256: &[u8; 32], file_name: &str) -> PathBuf {
        self.root
            .join(LIBS_DIR)
            .join(Hex32(sha256).to_string())
            .join(file_name)
    }

    /// Is there already a usable cache entry for this library?
    ///
    /// `Ok(None)` means "no, extract it". [`ApkError::CacheSizeMismatch`] means an entry was found
    /// but is the wrong length and so cannot be trusted; [`extract`](Self::extract) treats that as
    /// a miss, while a caller that only wants to inspect the cache sees why it was unusable.
    ///
    /// `apk` is required, and is not merely where `entry` came from: the archive's identity is part
    /// of the probe key, so a hint written while reading one APK is invisible while reading another.
    /// See the module documentation for why that is load-bearing rather than tidy.
    pub fn lookup(&self, apk: &Apk, entry: &ZipEntry) -> ApkResult<Option<CachedLibrary>> {
        let file_name = native_library_file_name(entry)?;
        let index_path = self.index_path(&probe_key(apk, entry));

        let Some(sha256) = read_index(&index_path)? else {
            return Ok(None);
        };
        let path = self.library_path(&sha256, file_name);
        match fs::metadata(&path) {
            Ok(metadata) if !metadata.is_file() => Ok(None),
            Ok(metadata) if metadata.len() == entry.uncompressed_size() => Ok(Some(CachedLibrary {
                path,
                sha256,
                len: metadata.len(),
                outcome: CacheOutcome::Reused,
            })),
            Ok(metadata) => Err(ApkError::CacheSizeMismatch {
                path: show(&path),
                name: entry.name().to_owned(),
                expected: entry.uncompressed_size(),
                actual: metadata.len(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ApkError::io("inspecting a cache entry", &path, error)),
        }
    }

    /// Make sure this library is in the cache, extracting it if it is not, and say which happened.
    ///
    /// The entry must be named `lib/<abi>/<name>.so`. The returned file's bytes are verified twice
    /// over before it is published: the inflated stream must be exactly as long as the central
    /// directory says, and must hash to the CRC-32 the central directory records.
    ///
    /// A failure to write the hint index is logged and does not fail the call: the extraction
    /// itself succeeded and the cache file is valid, and the only consequence is that the next
    /// lookup will miss and re-extract.
    pub fn extract(&self, apk: &Apk, entry: &ZipEntry) -> ApkResult<CachedLibrary> {
        let file_name = native_library_file_name(entry)?;

        match self.lookup(apk, entry) {
            Ok(Some(hit)) => {
                tracing::debug!(
                    entry = entry.name(),
                    path = %hit.path.display(),
                    "extraction cache hit"
                );
                return Ok(hit);
            }
            Ok(None) => {}
            Err(stale @ ApkError::CacheSizeMismatch { .. }) => {
                tracing::warn!(entry = entry.name(), error = %stale, "re-extracting");
            }
            Err(other) => return Err(other),
        }

        let started = Instant::now();
        let probe = probe_key(apk, entry);
        let tmp_dir = self.root.join(TMP_DIR);
        create_dir_all(&tmp_dir)?;
        // Unique among live processes: no two live processes share a pid, and no two calls in one
        // process share a sequence number. A leftover `.part` from a dead process with the same pid
        // is simply truncated by `File::create`, since nothing is writing it any more.
        let temp = TempFile::new(tmp_dir.join(format!(
            "{probe}.{}.{}.part",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )));

        let sha256 = self.write_temp(apk, entry, temp.path())?;
        let final_path = self.library_path(&sha256, file_name);
        let outcome = self.publish(&temp, &final_path, entry)?;

        if let Err(error) = self.write_index(&probe, &sha256) {
            tracing::warn!(
                entry = entry.name(),
                error = %error,
                "could not write the extraction cache hint index; the next lookup will miss and \
                 re-extract"
            );
        }

        tracing::info!(
            entry = entry.name(),
            bytes = entry.uncompressed_size(),
            path = %final_path.display(),
            elapsed_ms = started.elapsed().as_millis(),
            ?outcome,
            "extracted a native library"
        );

        Ok(CachedLibrary {
            path: final_path,
            sha256,
            len: entry.uncompressed_size(),
            outcome,
        })
    }

    /// Inflate the entry into `temp`, returning the SHA-256 of what was written.
    ///
    /// The bytes are on disk and durable when this returns: without the `sync_all` a crash could
    /// leave a file whose name says it is complete and whose tail is zeroes.
    fn write_temp(&self, apk: &Apk, entry: &ZipEntry, temp: &Path) -> ApkResult<[u8; 32]> {
        let file =
            File::create(temp).map_err(|e| ApkError::io("creating a cache temporary file", temp, e))?;
        let mut writer = BufWriter::with_capacity(WRITE_BUFFER, file);
        let destination = show(temp);
        let sha256 = apk.copy_entry_hashing(entry, &mut writer, &destination)?;
        let file = writer
            .into_inner()
            .map_err(|e| ApkError::io("flushing a cache temporary file", temp, e.into_error()))?;
        file.sync_all()
            .map_err(|e| ApkError::io("syncing a cache temporary file", temp, e))?;
        Ok(sha256)
    }

    /// Move a finished temporary file to its content-addressed name.
    ///
    /// A rename failure is not automatically an error: another extractor may have published the
    /// same file first and may still hold it open (a mapped file cannot be replaced on Windows).
    /// Since the destination name *is* the hash of its contents, a destination of the right length
    /// is the file we were about to write, so it is kept and ours is discarded.
    fn publish(
        &self,
        temp: &TempFile,
        final_path: &Path,
        entry: &ZipEntry,
    ) -> ApkResult<CacheOutcome> {
        let dir = final_path.parent().unwrap_or(&self.root);
        create_dir_all(dir)?;

        match fs::rename(temp.path(), final_path) {
            Ok(()) => {
                temp.disarm();
                Ok(CacheOutcome::Extracted)
            }
            Err(rename_error) => match fs::metadata(final_path) {
                Ok(metadata)
                    if metadata.is_file() && metadata.len() == entry.uncompressed_size() =>
                {
                    tracing::debug!(
                        entry = entry.name(),
                        path = %final_path.display(),
                        "lost the extraction race; keeping the identical file already published"
                    );
                    Ok(CacheOutcome::ReusedAfterRace)
                }
                _ => Err(ApkError::CachePublish {
                    name: entry.name().to_owned(),
                    temp_path: show(temp.path()),
                    final_path: show(final_path),
                    expected: entry.uncompressed_size(),
                    source: rename_error,
                }),
            },
        }
    }

    /// Path of the hint-index file for a probe key.
    fn index_path(&self, probe: &str) -> PathBuf {
        self.root.join(INDEX_DIR).join(format!("{probe}{INDEX_SUFFIX}"))
    }

    /// Publish a hint-index entry, atomically, so a concurrent reader never sees a half-written
    /// hash.
    fn write_index(&self, probe: &str, sha256: &[u8; 32]) -> ApkResult<()> {
        let index_dir = self.root.join(INDEX_DIR);
        create_dir_all(&index_dir)?;
        let tmp_dir = self.root.join(TMP_DIR);
        create_dir_all(&tmp_dir)?;

        let temp = TempFile::new(tmp_dir.join(format!(
            "{probe}.{}.{}.index",
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        )));
        let mut file = File::create(temp.path())
            .map_err(|e| ApkError::io("creating a cache index temporary file", temp.path(), e))?;
        writeln!(file, "{}", Hex32(sha256))
            .map_err(|e| ApkError::io("writing a cache index entry", temp.path(), e))?;
        file.sync_all()
            .map_err(|e| ApkError::io("syncing a cache index entry", temp.path(), e))?;
        drop(file);

        let final_path = self.index_path(probe);
        fs::rename(temp.path(), &final_path)
            .map_err(|e| ApkError::io("publishing a cache index entry", &final_path, e))?;
        temp.disarm();
        Ok(())
    }
}

/// The name of the file a native-library entry extracts to, or why it is not one.
fn native_library_file_name(entry: &ZipEntry) -> ApkResult<&str> {
    split_lib_path(entry.name())
        .filter(|(_, file_name)| file_name.ends_with(".so"))
        .map(|(_, file_name)| file_name)
        .ok_or_else(|| ApkError::NotANativeLibrary {
            name: entry.name().to_owned(),
        })
}

/// The hint-index key: which archive, and which entry inside it.
///
/// Hashed rather than concatenated so the key is a fixed-length, filesystem-safe name whatever the
/// archive and entry are called. Variable-length fields are length-prefixed, so no two different
/// inputs can serialise to the same byte string.
///
/// Every input is already in memory — `Apk::open` stats the file and parses the central directory —
/// so this costs no I/O. See the module documentation for why the archive's identity is in here.
fn probe_key(apk: &Apk, entry: &ZipEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(PROBE_DOMAIN);

    // Which archive.
    let path = apk.canonical_path().to_string_lossy();
    hasher.update((path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    hasher.update(apk.file_len().to_le_bytes());
    match apk
        .modified()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
    {
        Some(since_epoch) => {
            hasher.update([1u8]);
            hasher.update(since_epoch.as_secs().to_le_bytes());
            hasher.update(since_epoch.subsec_nanos().to_le_bytes());
        }
        // No mtime, or one before the epoch. Distinguished from every real timestamp by the tag
        // byte, so "unknown" can never be confused with a particular time.
        None => hasher.update([0u8]),
    }

    // Which entry inside it.
    hasher.update((entry.name().len() as u64).to_le_bytes());
    hasher.update(entry.name().as_bytes());
    hasher.update(entry.local_header_offset().to_le_bytes());
    hasher.update(entry.uncompressed_size().to_le_bytes());
    hasher.update(entry.compressed_size().to_le_bytes());
    hasher.update(entry.crc32().to_le_bytes());
    hasher.update(entry.method().code().to_le_bytes());

    let digest: [u8; 32] = hasher.finalize().into();
    Hex32(&digest).to_string()
}

/// Read a hint-index file, treating anything unparseable as an absent hint.
///
/// "Unparseable" includes *not being text at all*. The contents of this file are whatever is on
/// disk, so they may be arbitrary bytes — truncated by a crash, replaced by something else, or
/// simply junk — and none of that may make the cache permanently unusable. Only an I/O failure that
/// is not "absent" is an error.
fn read_index(path: &Path) -> ApkResult<Option<[u8; 32]>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ApkError::io("opening a cache index entry", path, error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_INDEX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| ApkError::io("reading a cache index entry", path, e))?;

    let hash = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| parse_hex32(text.trim()));
    match hash {
        Some(sha256) => Ok(Some(sha256)),
        None => {
            tracing::warn!(
                path = %path.display(),
                bytes = bytes.len(),
                "ignoring an unparseable extraction cache index entry"
            );
            Ok(None)
        }
    }
}

/// Parse exactly 64 lowercase-or-uppercase hex digits into 32 bytes.
fn parse_hex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = (bytes[index * 2] as char).to_digit(16)?;
        let low = (bytes[index * 2 + 1] as char).to_digit(16)?;
        *slot = (high * 16 + low) as u8;
    }
    Some(out)
}

fn create_dir_all(path: &Path) -> ApkResult<()> {
    fs::create_dir_all(path).map_err(|e| ApkError::io("creating a cache directory", path, e))
}

/// A temporary file that removes itself unless it was successfully published.
///
/// This is what keeps a failed or panicking extraction from leaving 109 MB of garbage behind, and
/// it is why no partial file ever appears under a cache path: the only path a partial file can have
/// is this one, and nothing looks there.
struct TempFile {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl TempFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            armed: std::cell::Cell::new(true),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Stop the file being removed, because it has been renamed away.
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed.get() {
            if let Err(error) = fs::remove_file(&self.path) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %self.path.display(),
                        %error,
                        "could not remove an abandoned extraction temporary file"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_hex32;

    #[test]
    fn hex32_round_trips_and_rejects_rubbish() {
        let digest = [0xabu8; 32];
        let hex = crate::error::Hex32(&digest).to_string();
        assert_eq!(hex.len(), 64);
        assert_eq!(parse_hex32(&hex), Some(digest));

        assert_eq!(parse_hex32(""), None, "empty");
        assert_eq!(parse_hex32(&hex[..63]), None, "too short");
        assert_eq!(parse_hex32(&format!("{hex}0")), None, "too long");
        assert_eq!(
            parse_hex32(&"z".repeat(64)),
            None,
            "64 characters that are not hex"
        );
    }
}
