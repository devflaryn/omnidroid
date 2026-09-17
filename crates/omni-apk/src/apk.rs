//! The [`Apk`] reader: opens an APK, resolves every entry, and reads entries out of it.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use sha2::{Digest, Sha256};

use crate::error::{show, ApkError, ApkResult};
use crate::zip::{
    apply_zip64, check_local_against_central, decode_name, find_end_of_central_directory,
    parse_central_directory, parse_local_file_header, without_zip64, CompressionMethod,
    EndOfCentralDirectory, ZipEntry, EOCD_LEN, LOCAL_FILE_HEADER_LEN, MAX_COMMENT_LEN,
    ZIP64_EOCD_LEN, ZIP64_LOCATOR_LEN,
};

/// The entry that holds the binary AndroidManifest.
pub const ANDROID_MANIFEST_ENTRY: &str = "AndroidManifest.xml";

/// The prefix under which `AAssetManager` assets live.
pub const ASSETS_PREFIX: &str = "assets/";

/// The prefix under which native libraries live, as `lib/<abi>/<name>.so`.
pub const LIB_PREFIX: &str = "lib/";

/// Buffer size for streaming a payload. Large enough that a 109 MB library is ~430 reads rather
/// than ~13,000.
const STREAM_BUFFER: usize = 256 * 1024;

/// An open APK, with every entry resolved.
///
/// Opening resolves *all* local file headers up front, because the payload offset — the thing
/// Omnidroid actually needs (D11) — lives there and nowhere else. On
/// `Roblox-2.738.1397.apk` that is 2,365 headers and it costs tens of milliseconds once, which
/// buys a fully-resolved, immutable entry table that can then be shared and queried without
/// touching the disk again.
///
/// # Concurrency, and the latency hazard in it
///
/// `Apk` is `Send + Sync`, but one APK is one file handle, and a read is a seek followed by a
/// stream, so **the handle's lock is held for the whole of a read, not just the seek**. Reads are
/// therefore fully serialised: concurrent readers are correct but do not overlap, and a reader that
/// arrives while `lib/arm64-v8a/libroblox.so` is being inflated waits for all 109 MB of it — about
/// 413 ms in a release build, several seconds in a debug one. Nothing here is fair or preemptible
/// either, so that wait is unbounded in the presence of a steady stream of large reads.
///
/// A caller that needs a small read not to queue behind a big one should **open a second `Apk`**;
/// opening costs about 6.4 ms including all 2,365 local file headers, so a reader per thread is
/// cheap. Positional reads (`pread`, or `ReadFile` with an `OVERLAPPED` offset) would remove the
/// lock entirely, but they are only reachable through `std::os::unix` / `std::os::windows`, and
/// Global Constraint 4 keeps OS-specific code out of every crate but `omni-platform`; widening that
/// crate's surface for a bottleneck nobody has measured in anger is not a trade worth making yet.
pub struct Apk {
    path: PathBuf,
    canonical_path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    file: Mutex<File>,
    eocd: EndOfCentralDirectory,
    entries: Vec<ZipEntry>,
    by_name: FxHashMap<Box<str>, usize>,
}

impl std::fmt::Debug for Apk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Apk")
            .field("path", &self.path)
            .field("len", &self.len)
            .field("entries", &self.entries.len())
            .field("zip64", &self.eocd.zip64)
            .finish()
    }
}

impl Apk {
    /// Open an APK and resolve every entry.
    ///
    /// This reads the end-of-central-directory record, the central directory, and then one local
    /// file header per entry. It does not read, decompress or map any payload.
    pub fn open(path: impl Into<PathBuf>) -> ApkResult<Self> {
        let path = path.into();
        let mut file = File::open(&path).map_err(|e| ApkError::io("opening the APK", &path, e))?;
        let metadata = file
            .metadata()
            .map_err(|e| ApkError::io("reading the metadata of the APK", &path, e))?;
        let len = metadata.len();
        // Identity of *this* archive, for the extraction cache's probe key. Taken here because the
        // stat has already happened, and kept for the lifetime of the `Apk` so that the key a
        // lookup computes and the key the following extraction writes cannot disagree.
        let modified = metadata.modified().ok();
        let canonical_path = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

        if len < EOCD_LEN as u64 {
            return Err(ApkError::TooShort {
                path: show(&path),
                len,
                minimum: EOCD_LEN as u64,
            });
        }

        let eocd = read_end_of_central_directory(&mut file, &path, len)?;

        let cd_end = eocd
            .central_directory_offset
            .saturating_add(eocd.central_directory_size);
        if cd_end > len {
            return Err(ApkError::CentralDirectoryOutOfBounds {
                path: show(&path),
                offset: eocd.central_directory_offset,
                size: eocd.central_directory_size,
                end: cd_end,
                len,
            });
        }

        let cd_size = usize::try_from(eocd.central_directory_size).map_err(|_| {
            ApkError::TooLargeForAddressSpace {
                name: "the central directory".to_owned(),
                size: eocd.central_directory_size,
                pointer_width: usize::BITS,
            }
        })?;
        // Bounded by the file (`cd_end > len` was refused above), but still fallible: a 4 GB
        // central directory in a 4 GB archive must be an error, not an abort.
        let mut cd = Vec::new();
        cd.try_reserve_exact(cd_size)
            .map_err(|source| ApkError::Allocation {
                name: "the central directory".to_owned(),
                bytes: cd_size,
                source,
            })?;
        cd.resize(cd_size, 0);
        read_exact_at(
            &mut file,
            eocd.central_directory_offset,
            &mut cd,
            "reading the central directory",
            &path,
        )?;

        let records = parse_central_directory(
            &cd,
            eocd.central_directory_offset,
            eocd.entry_count,
        )?;
        drop(cd);

        let entries = resolve_payload_offsets(&mut file, &path, len, &records)?;

        let mut by_name = FxHashMap::with_capacity_and_hasher(entries.len(), Default::default());
        for (index, entry) in entries.iter().enumerate() {
            // First spelling wins. Android's own reader rejects duplicate names outright; we keep
            // the archive readable but never let a later record shadow an earlier one.
            by_name.entry(entry.name.as_str().into()).or_insert(index);
        }

        tracing::debug!(
            path = %path.display(),
            len,
            entries = entries.len(),
            zip64 = eocd.zip64,
            "opened APK"
        );

        Ok(Self {
            path,
            canonical_path,
            len,
            modified,
            file: Mutex::new(file),
            eocd,
            entries,
            by_name,
        })
    }

    /// The path the APK was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path the APK was opened from, with symlinks and relative components resolved.
    ///
    /// Resolved once at open time. Part of the extraction cache's identity for this archive, so that
    /// a hint index entry written while reading one APK is never consulted while reading another —
    /// see [`LibraryCache`](crate::LibraryCache). Falls back to the path as given if the platform
    /// refuses to canonicalise it.
    #[must_use]
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// The length of the APK file in bytes.
    #[must_use]
    pub const fn file_len(&self) -> u64 {
        self.len
    }

    /// The APK's last-modified time, as reported when it was opened.
    ///
    /// `None` where the platform does not report one. Also part of the cache's identity for this
    /// archive: replacing an APK's contents changes this, and so invalidates every cache hint
    /// derived from the old contents.
    #[must_use]
    pub const fn modified(&self) -> Option<std::time::SystemTime> {
        self.modified
    }

    /// The located end-of-central-directory record.
    #[must_use]
    pub const fn end_of_central_directory(&self) -> &EndOfCentralDirectory {
        &self.eocd
    }

    /// Every entry, in central-directory order.
    #[must_use]
    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// Look up an entry by its exact name.
    #[must_use]
    pub fn entry(&self, name: &str) -> Option<&ZipEntry> {
        self.by_name.get(name).map(|&index| &self.entries[index])
    }

    /// Look up an entry by its exact name, or fail saying which archive was searched.
    pub fn require_entry(&self, name: &str) -> ApkResult<&ZipEntry> {
        self.entry(name).ok_or_else(|| ApkError::EntryNotFound {
            name: name.to_owned(),
            path: show(&self.path),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------------------------------

    /// Read an entry into memory, verifying its CRC-32.
    ///
    /// Fails rather than truncating when the entry does not fit in this target's address space; use
    /// [`copy_entry_to`](Self::copy_entry_to) for anything that large.
    ///
    /// # Hostile sizes
    ///
    /// The buffer is sized from [`ZipEntry::plausible_uncompressed_size`] — the declared size
    /// clamped to what the payload that is actually present could produce — and reserved with
    /// `try_reserve_exact`, so a declared size of 70 TB out of a 200-byte archive returns
    /// [`ApkError::Allocation`] or [`ApkError::UncompressedSizeMismatch`] rather than aborting the
    /// process. That distinction matters: an allocation failure in Rust is an **abort**, not a
    /// panic, so no caller could catch it.
    pub fn read_entry(&self, entry: &ZipEntry) -> ApkResult<Vec<u8>> {
        let plausible = entry.plausible_uncompressed_size();
        let capacity = usize::try_from(plausible).map_err(|_| ApkError::TooLargeForAddressSpace {
            name: entry.name().to_owned(),
            size: plausible,
            pointer_width: usize::BITS,
        })?;
        let mut out = Vec::new();
        out.try_reserve_exact(capacity)
            .map_err(|source| ApkError::Allocation {
                name: entry.name().to_owned(),
                bytes: capacity,
                source,
            })?;
        self.stream(entry, &mut out, "an in-memory buffer", None)?;
        Ok(out)
    }

    /// Read an entry by name into memory, verifying its CRC-32.
    pub fn read_named(&self, name: &str) -> ApkResult<Vec<u8>> {
        let entry = self.require_entry(name)?;
        self.read_entry(entry)
    }

    /// Stream an entry to a writer, verifying its CRC-32 and uncompressed size.
    ///
    /// `destination` describes where the bytes are going and is used only to make a write failure
    /// legible, e.g. the path of the file being written.
    ///
    /// Verification happens **after** the last byte is written, which is unavoidable for a stream:
    /// a caller that must never expose unverified bytes has to write somewhere private and publish
    /// only on success. That is exactly what [`LibraryCache`](crate::LibraryCache) does.
    pub fn copy_entry_to(
        &self,
        entry: &ZipEntry,
        out: &mut dyn Write,
        destination: &str,
    ) -> ApkResult<()> {
        self.stream(entry, out, destination, None)
    }

    /// Stream an entry to a writer, verifying it and returning the SHA-256 of the bytes written.
    ///
    /// The digest is of the **uncompressed** bytes, which is what the extraction cache is keyed on.
    pub(crate) fn copy_entry_hashing(
        &self,
        entry: &ZipEntry,
        out: &mut dyn Write,
        destination: &str,
    ) -> ApkResult<[u8; 32]> {
        let mut hasher = Sha256::new();
        self.stream(entry, out, destination, Some(&mut hasher))?;
        Ok(hasher.finalize().into())
    }

    /// The one read path: STORED entries are copied byte for byte, DEFLATED entries are inflated,
    /// and either way the output is measured, CRC-checked and optionally hashed as it goes.
    fn stream(
        &self,
        entry: &ZipEntry,
        out: &mut dyn Write,
        destination: &str,
        sha: Option<&mut Sha256>,
    ) -> ApkResult<()> {
        let mut sink = Sink {
            out,
            crc: crc32fast::Hasher::new(),
            sha,
            written: 0,
        };

        {
            let mut file = self.file.lock();
            file.seek(SeekFrom::Start(entry.payload_offset()))
                .map_err(|e| ApkError::io("seeking to an entry payload", &self.path, e))?;
            let payload = (&mut *file).take(entry.compressed_size());
            let reader = BufReader::with_capacity(STREAM_BUFFER, payload);

            match entry.method() {
                CompressionMethod::Stored => {
                    self.pump(entry, reader, &mut sink, destination, false)?;
                }
                CompressionMethod::Deflated => {
                    let inflater = flate2::read::DeflateDecoder::new(reader);
                    self.pump(entry, inflater, &mut sink, destination, true)?;
                }
                CompressionMethod::Other(method) => {
                    return Err(ApkError::UnsupportedCompressionMethod {
                        name: entry.name().to_owned(),
                        method,
                        method_name: crate::error::compression_method_name(method),
                    });
                }
            }
        }

        if sink.written != entry.uncompressed_size() {
            return Err(ApkError::UncompressedSizeMismatch {
                name: entry.name().to_owned(),
                expected: entry.uncompressed_size(),
                actual: sink.written,
            });
        }
        let crc = sink.crc.finalize();
        if crc != entry.crc32() {
            return Err(ApkError::CrcMismatch {
                name: entry.name().to_owned(),
                expected: entry.crc32(),
                actual: crc,
                len: sink.written,
            });
        }

        Ok(())
    }

    /// Copy a reader into the sink, attributing failures to the right side.
    fn pump(
        &self,
        entry: &ZipEntry,
        mut source: impl Read,
        sink: &mut Sink<'_>,
        destination: &str,
        inflating: bool,
    ) -> ApkResult<()> {
        let mut buffer = vec![0u8; STREAM_BUFFER];
        loop {
            let read = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(source_error) => {
                    return Err(if inflating {
                        ApkError::Inflate {
                            name: entry.name().to_owned(),
                            produced: sink.written,
                            expected: entry.uncompressed_size(),
                            source: source_error,
                        }
                    } else {
                        ApkError::io("reading a stored entry payload", &self.path, source_error)
                    });
                }
            };
            let before = sink.written;
            if let Err(sink_error) = sink.write_all(&buffer[..read]) {
                return Err(ApkError::Sink {
                    name: entry.name().to_owned(),
                    destination: destination.to_owned(),
                    written: before,
                    expected: entry.uncompressed_size(),
                    source: sink_error,
                });
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Android-shaped views of the archive
    // -----------------------------------------------------------------------------------------

    /// The `AndroidManifest.xml` entry.
    pub fn manifest_entry(&self) -> ApkResult<&ZipEntry> {
        self.require_entry(ANDROID_MANIFEST_ENTRY)
    }

    /// The **bytes** of `AndroidManifest.xml`, which are Android binary XML (AXML), not text.
    ///
    /// Deliberately undecoded. Binary-XML decoding belongs to whatever consumes the manifest, and
    /// putting it here would make this crate the owner of a resource-table format it has no other
    /// reason to know about.
    pub fn read_manifest(&self) -> ApkResult<Vec<u8>> {
        self.read_named(ANDROID_MANIFEST_ENTRY)
    }

    /// Look up an asset by its `AAssetManager`-relative name, i.e. without the `assets/` prefix.
    #[must_use]
    pub fn asset_entry(&self, name: &str) -> Option<&ZipEntry> {
        let mut full = String::with_capacity(ASSETS_PREFIX.len() + name.len());
        full.push_str(ASSETS_PREFIX);
        full.push_str(name);
        self.entry(&full)
    }

    /// Read an asset by its `AAssetManager`-relative name.
    pub fn read_asset(&self, name: &str) -> ApkResult<Vec<u8>> {
        let entry = self.asset_entry(name).ok_or_else(|| ApkError::EntryNotFound {
            name: format!("{ASSETS_PREFIX}{name}"),
            path: show(&self.path),
        })?;
        self.read_entry(entry)
    }

    /// Every entry under `assets/`, in central-directory order.
    #[must_use]
    pub fn assets(&self) -> Vec<&ZipEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.name().starts_with(ASSETS_PREFIX))
            .collect()
    }

    /// Every entry under `lib/`, whether or not it looks like a library.
    ///
    /// Separate from [`native_libraries`](Self::native_libraries) so that a stray non-`.so` file
    /// under `lib/` is visible rather than silently dropped.
    #[must_use]
    pub fn lib_entries(&self) -> Vec<&ZipEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.name().starts_with(LIB_PREFIX))
            .collect()
    }

    /// Every `lib/<abi>/<name>.so`, in central-directory order.
    #[must_use]
    pub fn native_libraries(&self) -> Vec<NativeLibrary<'_>> {
        self.entries
            .iter()
            .filter_map(NativeLibrary::from_entry)
            .collect()
    }

    /// The ABI directories that actually contain libraries, sorted and deduplicated.
    #[must_use]
    pub fn abis(&self) -> Vec<&str> {
        let mut abis: Vec<&str> = self
            .native_libraries()
            .into_iter()
            .map(|lib| lib.abi)
            .collect();
        abis.sort_unstable();
        abis.dedup();
        abis
    }

    /// Every library for one ABI, in central-directory order.
    #[must_use]
    pub fn native_libraries_for_abi(&self, abi: &str) -> Vec<NativeLibrary<'_>> {
        self.native_libraries()
            .into_iter()
            .filter(|lib| lib.abi == abi)
            .collect()
    }
}

/// A `lib/<abi>/<name>.so` entry, with its path already split.
#[derive(Debug, Clone, Copy)]
pub struct NativeLibrary<'a> {
    abi: &'a str,
    file_name: &'a str,
    entry: &'a ZipEntry,
}

impl<'a> NativeLibrary<'a> {
    /// Recognise `lib/<abi>/<name>.so`, and nothing else.
    fn from_entry(entry: &'a ZipEntry) -> Option<Self> {
        let (abi, file_name) = split_lib_path(entry.name())?;
        if !file_name.ends_with(".so") {
            return None;
        }
        Some(Self {
            abi,
            file_name,
            entry,
        })
    }

    /// The ABI directory the library sits in, e.g. `arm64-v8a`.
    #[must_use]
    pub const fn abi(&self) -> &'a str {
        self.abi
    }

    /// The library's file name, e.g. `libroblox.so`.
    #[must_use]
    pub const fn file_name(&self) -> &'a str {
        self.file_name
    }

    /// The underlying zip entry.
    #[must_use]
    pub const fn entry(&self) -> &'a ZipEntry {
        self.entry
    }
}

/// Split `lib/<abi>/<file>` into its ABI and file name, rejecting anything deeper or shallower.
pub(crate) fn split_lib_path(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix(LIB_PREFIX)?;
    let (abi, file_name) = rest.split_once('/')?;
    if abi.is_empty() || file_name.is_empty() || file_name.contains('/') {
        return None;
    }
    Some((abi, file_name))
}

// ---------------------------------------------------------------------------------------------
// Opening helpers
// ---------------------------------------------------------------------------------------------

/// A writer that measures, CRC-32s and optionally SHA-256s everything passing through it.
struct Sink<'a> {
    out: &'a mut dyn Write,
    crc: crc32fast::Hasher,
    sha: Option<&'a mut Sha256>,
    written: u64,
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.out.write(buf)?;
        let accepted = &buf[..written];
        self.crc.update(accepted);
        if let Some(sha) = &mut self.sha {
            sha.update(accepted);
        }
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

fn read_exact_at(
    file: &mut File,
    offset: u64,
    buf: &mut [u8],
    operation: &'static str,
    path: &Path,
) -> ApkResult<()> {
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| ApkError::io(operation, path, e))?;
    file.read_exact(buf)
        .map_err(|e| ApkError::io(operation, path, e))
}

/// Locate the end-of-central-directory record, consulting zip64 only when there is a locator.
fn read_end_of_central_directory(
    file: &mut File,
    path: &Path,
    len: u64,
) -> ApkResult<EndOfCentralDirectory> {
    let window = EOCD_LEN as u64 + MAX_COMMENT_LEN + ZIP64_LOCATOR_LEN as u64;
    let tail_len = window.min(len);
    let tail_base = len - tail_len;
    let mut tail = vec![0u8; usize::try_from(tail_len).unwrap_or(usize::MAX)];
    read_exact_at(
        file,
        tail_base,
        &mut tail,
        "reading the end of the APK",
        path,
    )?;

    let path_string = show(path);
    let legacy = find_end_of_central_directory(&tail, tail_base, &path_string)?;

    match legacy.zip64_record_offset {
        Some(offset) => {
            let mut record = [0u8; ZIP64_EOCD_LEN];
            read_exact_at(
                file,
                offset,
                &mut record,
                "reading the zip64 end-of-central-directory record",
                path,
            )?;
            apply_zip64(&legacy, &record, offset)
        }
        None if legacy.needs_zip64() => Err(ApkError::Zip64LocatorMissing {
            eocd_offset: legacy.offset,
        }),
        None => without_zip64(&legacy),
    }
}

/// Read one local file header per entry and turn central-directory records into [`ZipEntry`]s.
///
/// This is the pass that makes an entry mappable. The central directory says where each *local file
/// header* is; only that header says how long its own name and extra field are; so only that header
/// places the payload. Records are visited in central-directory order, which in an APK is also
/// ascending local-header order, so the reads walk the file forwards.
fn resolve_payload_offsets(
    file: &mut File,
    path: &Path,
    len: u64,
    records: &[crate::zip::CentralRecord],
) -> ApkResult<Vec<ZipEntry>> {
    let mut entries = Vec::with_capacity(records.len());
    let mut header = [0u8; LOCAL_FILE_HEADER_LEN];
    let mut name_bytes: Vec<u8> = Vec::new();

    for record in records {
        let header_offset = record.local_header_offset;
        let available = len.saturating_sub(header_offset);
        if available < LOCAL_FILE_HEADER_LEN as u64 {
            return Err(ApkError::Truncated {
                structure: "local file header",
                offset: header_offset,
                field: "the fixed 30-byte header",
                needed: LOCAL_FILE_HEADER_LEN,
                available: usize::try_from(available).unwrap_or(usize::MAX),
            });
        }
        read_exact_at(
            file,
            header_offset,
            &mut header,
            "reading a local file header",
            path,
        )?;
        let local = parse_local_file_header(&header, header_offset)?;

        // The name follows the 30 fixed bytes, and the read above left the cursor exactly there.
        name_bytes.resize(usize::from(local.name_len), 0);
        file.read_exact(&mut name_bytes)
            .map_err(|e| ApkError::io("reading a local file header's name", path, e))?;
        let name_offset = header_offset + LOCAL_FILE_HEADER_LEN as u64;
        let local_name = decode_name(&name_bytes, name_offset)?;
        check_local_against_central(record, &local, &local_name)?;

        let payload_offset = local.payload_offset(header_offset);
        let payload_end = payload_offset.saturating_add(record.compressed_size);
        if payload_end > len {
            return Err(ApkError::PayloadOutOfBounds {
                name: record.name.clone(),
                path: show(path),
                payload_offset,
                compressed_size: record.compressed_size,
                end: payload_end,
                len,
            });
        }

        entries.push(ZipEntry {
            name: record.name.clone(),
            method: record.method,
            flags: record.flags,
            crc32: record.crc32,
            compressed_size: record.compressed_size,
            uncompressed_size: record.uncompressed_size,
            local_header_offset: header_offset,
            local_extra_len: local.extra_len,
            payload_offset,
            central_record_offset: record.central_record_offset,
        });
    }

    Ok(entries)
}
