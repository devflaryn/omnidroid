//! Tests against the real fixture, `Roblox-2.738.1397.apk`.
//!
//! Every number asserted here is exact and comes from `docs/research/apk-analysis.md`, which was
//! produced by an independent Python parser reading the same bytes. Two independent
//! implementations agreeing on 2,365 entries, 954 STORED, one 4 KB-aligned payload and a
//! `{0: 1660, 1: 242, 2: 231, 3: 232}` extra-field histogram is what makes either of them
//! believable (Global Constraints 2 and 3).
//!
//! The fixture is git-ignored, so every test here **skips** when it is absent rather than failing.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use omni_apk::{
    Apk, ApkError, CacheOutcome, CompressionMethod, LibraryCache, MAPPING_ALIGNMENT, LIBS_DIR,
};
use sha2::{Digest, Sha256};

const FIXTURE: &str = "Roblox-2.738.1397.apk";

/// Open the fixture, or explain why the test is doing nothing.
///
/// Returning `None` rather than failing is deliberate: a fresh clone has no 160 MB APK in it, and a
/// test suite that fails on a clean checkout teaches people to ignore it.
fn fixture() -> Option<Apk> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(FIXTURE);
    if !path.exists() {
        eprintln!(
            "skipping: {} is not present (it is git-ignored; see docs/research/apk-analysis.md)",
            path.display()
        );
        return None;
    }
    Some(Apk::open(&path).expect("the fixture must open"))
}

/// A throwaway directory that removes itself, so a failed run does not leave 109 MB behind.
struct TempDir {
    path: PathBuf,
}

static TEMP_DIRS: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "omni-apk-test-{label}-{}-{}",
            std::process::id(),
            TEMP_DIRS.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("a temporary directory must be creatable");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn sha256_of_file(path: &Path) -> [u8; 32] {
    let mut file = fs::File::open(path).expect("the cache file must be readable");
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("reading the cache file");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
}

/// Which alignment bucket a payload offset falls in, using the same buckets as
/// `docs/research/apk-analysis.md` §1.3: 16 KiB, 4 KiB, 4-byte, unaligned, each exclusive.
fn alignment_bucket(alignment: u64) -> usize {
    if alignment >= 16384 {
        0
    } else if alignment >= 4096 {
        1
    } else if alignment >= 4 {
        2
    } else {
        3
    }
}

// -------------------------------------------------------------------------------------------
// Container
// -------------------------------------------------------------------------------------------

#[test]
fn container_facts_match_the_forensic_analysis() {
    let Some(apk) = fixture() else { return };

    assert_eq!(apk.file_len(), 159_853_296, "file length");
    let eocd = apk.end_of_central_directory();
    assert_eq!(eocd.offset, 159_853_274, "end-of-central-directory offset");
    assert_eq!(
        eocd.central_directory_offset, 159_629_312,
        "central directory offset"
    );
    assert_eq!(eocd.central_directory_size, 223_962, "central directory size");
    assert_eq!(eocd.entry_count, 2_365, "declared entry count");
    assert_eq!(eocd.comment_len, 0, "archive comment length");
    assert!(
        !eocd.zip64,
        "this APK has no zip64 end-of-central-directory record: the PK\\x06\\x06 pattern a naive \
         search finds is inside compressed data (apk-analysis.md section 1.1)"
    );
    assert_eq!(
        eocd.zip64_record_offset, None,
        "and therefore no zip64 locator either"
    );

    assert_eq!(apk.entries().len(), 2_365, "parsed entry count");

    let stored = apk.entries().iter().filter(|e| e.is_stored()).count();
    let deflated = apk.entries().iter().filter(|e| e.is_deflated()).count();
    assert_eq!(stored, 954, "STORED entries");
    assert_eq!(deflated, 1_411, "DEFLATED entries");
    assert_eq!(
        stored + deflated,
        2_365,
        "no entry uses any method other than 0 and 8"
    );

    let total_uncompressed: u64 = apk.entries().iter().map(|e| e.uncompressed_size()).sum();
    let total_compressed: u64 = apk.entries().iter().map(|e| e.compressed_size()).sum();
    assert_eq!(total_uncompressed, 247_032_045, "total uncompressed size");
    assert_eq!(total_compressed, 159_413_043, "total compressed size");
}

#[test]
fn payload_alignment_matches_the_measured_histogram() {
    let Some(apk) = fixture() else { return };

    // [16 KiB, 4 KiB, 4-byte, unaligned] for STORED and for DEFLATED, from apk-analysis.md §1.3.
    let mut buckets = [[0u64; 4]; 2];
    for entry in apk.entries() {
        let row = usize::from(!entry.is_stored());
        buckets[row][alignment_bucket(entry.payload_alignment())] += 1;
    }
    assert_eq!(
        buckets[0],
        [0, 1, 953, 0],
        "STORED entries by payload alignment: exactly one lands on a 4 KiB boundary and it is an \
         accident of zipalign -f 4, not page alignment"
    );
    assert_eq!(
        buckets[1],
        [0, 0, 377, 1034],
        "DEFLATED entries by payload alignment"
    );
}

#[test]
fn exactly_one_entry_in_the_whole_apk_is_directly_mappable() {
    let Some(apk) = fixture() else { return };

    let mappable: Vec<&str> = apk
        .entries()
        .iter()
        .filter(|entry| entry.is_directly_mappable())
        .map(|entry| entry.name())
        .collect();

    // One 1,447-byte PNG, by luck. This is the whole of D11's case: the predicate is implemented
    // and does fire, and on this APK it fires for nothing anybody wants to map.
    assert_eq!(
        mappable,
        vec!["res/drawable-mdpi-v4/notification_icon.png"],
        "directly mappable entries"
    );

    let png = apk
        .require_entry("res/drawable-mdpi-v4/notification_icon.png")
        .expect("the one mappable entry");
    assert!(png.is_stored());
    assert_eq!(png.payload_offset(), 5_042_176);
    assert_eq!(png.payload_offset() % MAPPING_ALIGNMENT, 0);
    assert_eq!(png.uncompressed_size(), 1_447);
}

#[test]
fn local_extra_field_histogram_shows_plain_four_byte_zipalign() {
    let Some(apk) = fixture() else { return };

    // Alignment padding lives in the *local* extra field and nowhere else, so this histogram is
    // the direct fingerprint of `zipalign -f 4`: never more than 3 bytes of padding, which can
    // only ever reach a 4-byte boundary.
    let mut histogram: BTreeMap<u16, u64> = BTreeMap::new();
    for entry in apk.entries() {
        *histogram.entry(entry.local_extra_len()).or_default() += 1;
    }
    let expected: BTreeMap<u16, u64> = [(0, 1660), (1, 242), (2, 231), (3, 232)].into_iter().collect();
    assert_eq!(histogram, expected, "local extra field length histogram");
}

#[test]
fn most_entries_defer_their_sizes_to_a_data_descriptor() {
    let Some(apk) = fixture() else { return };

    const FLAG_DATA_DESCRIPTOR: u16 = 1 << 3;
    let deferred = apk
        .entries()
        .iter()
        .filter(|e| e.flags() & FLAG_DATA_DESCRIPTOR != 0)
        .count();
    // 1,408 entries declare that their sizes and CRC-32 live in a trailing data descriptor, which
    // is why this crate takes all three from the central directory and never from the local header.
    assert_eq!(deferred, 1_408, "entries with general-purpose flag bit 3");

    // And one of them still reads correctly, CRC and all.
    let entry = apk.require_entry("res/anim/stay.xml").expect("res/anim/stay.xml");
    assert_ne!(entry.flags() & FLAG_DATA_DESCRIPTOR, 0);
    let bytes = apk.read_entry(entry).expect("a deferred-size entry must read");
    assert_eq!(bytes.len() as u64, entry.uncompressed_size());
}

// -------------------------------------------------------------------------------------------
// Native libraries
// -------------------------------------------------------------------------------------------

/// `(file name, uncompressed size, in-APK compressed size)` for all 11, from apk-analysis.md §2.2.
const LIBRARIES: [(&str, u64, u64); 11] = [
    ("libroblox.so", 109_193_800, 46_516_719),
    ("libzstd-jni-1.5.7-6.so", 18_440_296, 11_721_092),
    ("libbacktrace-native.so", 5_339_704, 2_078_803),
    ("librenderscript-toolkit.so", 394_112, 129_704),
    ("libeigen_blas.so", 251_784, 81_495),
    ("libimage_processing_util_jni.so", 32_544, 15_630),
    ("libdatastore_shared_counter.so", 7_112, 2_630),
    ("libtrampoline.so", 5_104, 1_900),
    ("libsurface_util_jni.so", 4_896, 1_756),
    ("libeigen_lapack.so", 4_032, 1_338),
    ("libyuv_shared.so", 3_752, 1_254),
];

#[test]
fn there_are_exactly_eleven_arm64_v8a_libraries_and_no_other_abi() {
    let Some(apk) = fixture() else { return };

    assert_eq!(
        apk.abis(),
        vec!["arm64-v8a"],
        "this is a single-ABI, 64-bit-ARM-only APK"
    );
    for abi in ["armeabi-v7a", "x86", "x86_64", "riscv64"] {
        assert!(
            apk.native_libraries_for_abi(abi).is_empty(),
            "no lib/{abi}/ directory may exist"
        );
    }

    let libraries = apk.native_libraries();
    assert_eq!(libraries.len(), 11, "libraries under lib/arm64-v8a/");
    assert_eq!(
        apk.lib_entries().len(),
        11,
        "and nothing under lib/ that is not one of them"
    );

    let mut by_name: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    for library in &libraries {
        assert_eq!(library.abi(), "arm64-v8a");
        by_name.insert(
            library.file_name(),
            (
                library.entry().uncompressed_size(),
                library.entry().compressed_size(),
            ),
        );
    }
    for (name, uncompressed, compressed) in LIBRARIES {
        assert_eq!(
            by_name.get(name),
            Some(&(uncompressed, compressed)),
            "sizes of {name}"
        );
    }

    let total_uncompressed: u64 = libraries
        .iter()
        .map(|l| l.entry().uncompressed_size())
        .sum();
    let total_compressed: u64 = libraries.iter().map(|l| l.entry().compressed_size()).sum();
    assert_eq!(total_uncompressed, 133_677_136, "total library bytes");
    assert_eq!(total_compressed, 60_552_321, "total library bytes in the APK");
}

#[test]
fn libroblox_is_exactly_109_193_800_bytes() {
    let Some(apk) = fixture() else { return };
    let entry = apk
        .require_entry("lib/arm64-v8a/libroblox.so")
        .expect("libroblox.so");
    assert_eq!(entry.uncompressed_size(), 109_193_800);
    assert_eq!(entry.compressed_size(), 46_516_719);
    assert_eq!(entry.crc32(), 0x3918_c831);
}

/// **The measurement that forced D11.** Every library is DEFLATED and none is page aligned, so the
/// direct-mapping predicate is false for all 11 and an extraction cache is not optional.
#[test]
fn no_native_library_can_be_mapped_out_of_the_apk() {
    let Some(apk) = fixture() else { return };

    let libraries = apk.native_libraries();
    assert_eq!(libraries.len(), 11);
    for library in libraries {
        let entry = library.entry();
        assert_eq!(
            entry.method(),
            CompressionMethod::Deflated,
            "{} must be DEFLATED",
            entry.name()
        );
        assert!(
            !entry.is_stored(),
            "{} must not be STORED",
            entry.name()
        );
        assert!(
            !entry.is_payload_aligned(MAPPING_ALIGNMENT),
            "{} payload offset {} must not be 4 KB aligned",
            entry.name(),
            entry.payload_offset()
        );
        assert!(
            !entry.is_directly_mappable(),
            "{} must not be directly mappable",
            entry.name()
        );
        assert!(
            entry.payload_alignment() < 4096,
            "{} is aligned to only {} bytes",
            entry.name(),
            entry.payload_alignment()
        );
    }
}

// -------------------------------------------------------------------------------------------
// Reading and CRC-32
// -------------------------------------------------------------------------------------------

#[test]
fn crc32_is_verified_on_read_and_corruption_is_rejected() {
    let Some(apk) = fixture() else { return };

    // A DEFLATED library of 18.4 MB, a STORED dex of 9.1 MB, a small DEFLATED xml, and the
    // manifest: both methods, and sizes spanning four orders of magnitude.
    for name in [
        "lib/arm64-v8a/libzstd-jni-1.5.7-6.so",
        "classes.dex",
        "AndroidManifest.xml",
        "lib/arm64-v8a/libyuv_shared.so",
    ] {
        let entry = apk.require_entry(name).expect(name);
        let bytes = apk.read_entry(entry).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            bytes.len() as u64,
            entry.uncompressed_size(),
            "{name} length"
        );
        entry
            .verify_crc32(&bytes)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    // Now corrupt one byte in the middle and require the same check to refuse it.
    let entry = apk
        .require_entry("lib/arm64-v8a/libeigen_blas.so")
        .expect("libeigen_blas.so");
    let mut bytes = apk.read_entry(entry).expect("libeigen_blas.so must read");
    let victim = bytes.len() / 2;
    bytes[victim] ^= 0x01;
    match entry.verify_crc32(&bytes) {
        Err(ApkError::CrcMismatch {
            expected,
            actual,
            len,
            ..
        }) => {
            assert_eq!(expected, entry.crc32());
            assert_ne!(actual, expected, "a flipped bit must change the CRC-32");
            assert_eq!(len, entry.uncompressed_size());
        }
        Err(other) => panic!("wrong error for a corrupted buffer: {other}"),
        Ok(()) => panic!("a corrupted buffer was accepted"),
    }

    // Truncation must be caught too, not just bit flips.
    bytes.truncate(bytes.len() - 1);
    assert!(
        entry.verify_crc32(&bytes).is_err(),
        "a truncated buffer was accepted"
    );
}

#[test]
fn the_manifest_is_returned_as_undecoded_binary_xml() {
    let Some(apk) = fixture() else { return };

    let entry = apk.manifest_entry().expect("AndroidManifest.xml");
    assert_eq!(entry.uncompressed_size(), 56_204);
    assert_eq!(entry.compressed_size(), 10_573);
    assert_eq!(entry.method(), CompressionMethod::Deflated);

    let bytes = apk.read_manifest().expect("the manifest must read");
    assert_eq!(bytes.len(), 56_204);
    // AXML: RES_XML_TYPE (0x0003), header size 8, then the chunk size.
    assert_eq!(&bytes[..4], &[0x03, 0x00, 0x08, 0x00], "AXML magic");
    let chunk_size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    assert_eq!(
        chunk_size, 56_204,
        "the AXML chunk spans the whole entry, which is as much as this crate knows about it"
    );
}

#[test]
fn stored_assets_and_dex_files_are_where_the_analysis_says() {
    let Some(apk) = fixture() else { return };

    // The asset-mapping fast path depends on this one: 14.7 MB of SPIR-V, STORED.
    let pack = apk
        .asset_entry("shaders/shaders_vulkan_mobile.pack")
        .expect("the Vulkan shader pack");
    assert_eq!(pack.name(), "assets/shaders/shaders_vulkan_mobile.pack");
    assert_eq!(pack.method(), CompressionMethod::Stored);
    assert!(pack.is_stored());
    assert_eq!(pack.uncompressed_size(), 14_724_671);
    assert_eq!(
        pack.compressed_size(),
        pack.uncompressed_size(),
        "a STORED entry occupies exactly its own size"
    );
    assert_eq!(pack.payload_offset(), 83_248_228);

    // It is STORED but not page aligned, so it cannot be mapped *at* its payload offset — and does
    // not need to be. Map the aligned window below it and skip the delta.
    assert!(!pack.is_directly_mappable());
    let window = pack
        .stored_map_window(MAPPING_ALIGNMENT)
        .expect("a STORED entry always has a map window");
    assert_eq!(window.file_offset % MAPPING_ALIGNMENT, 0);
    assert_eq!(window.file_offset, 83_247_104);
    assert_eq!(window.payload_delta, 1_124);
    assert_eq!(
        window.file_offset + window.payload_delta,
        pack.payload_offset()
    );
    assert_eq!(window.len, window.payload_delta + pack.uncompressed_size());
    assert!(
        window.file_offset + window.len <= apk.file_len(),
        "the window must lie inside the APK"
    );

    // A DEFLATED entry has no such window; there is nothing to map.
    let manifest = apk.manifest_entry().expect("AndroidManifest.xml");
    assert!(manifest.stored_map_window(MAPPING_ALIGNMENT).is_none());
    // Neither does a nonsensical alignment.
    assert!(pack.stored_map_window(0).is_none());
    assert!(pack.stored_map_window(3000).is_none());

    // All four dex files are STORED, which is what ART requires, and so is the resource table.
    for (name, size) in [
        ("classes.dex", 9_105_132u64),
        ("classes2.dex", 6_178_724),
        ("classes3.dex", 6_998_496),
        ("classes4.dex", 7_348),
        ("resources.arsc", 4_068_236),
    ] {
        let entry = apk.require_entry(name).expect(name);
        assert!(entry.is_stored(), "{name} must be STORED");
        assert_eq!(entry.uncompressed_size(), size, "{name} size");
    }
    assert_eq!(
        apk.require_entry("resources.arsc")
            .expect("resources.arsc")
            .payload_offset(),
        44,
        "resources.arsc is the first entry in the archive"
    );

    let assets = apk.assets();
    assert_eq!(assets.len(), 596, "entries under assets/");
    assert_eq!(
        assets.iter().filter(|e| e.is_stored()).count(),
        391,
        "STORED assets"
    );
    assert_eq!(
        assets.iter().filter(|e| e.is_deflated()).count(),
        205,
        "DEFLATED assets"
    );

    // And an asset reads by its AAssetManager-relative name, CRC and all.
    let bytes = apk
        .read_asset("shaders/shaders_vulkan_mobile.pack")
        .expect("the shader pack must read");
    assert_eq!(bytes.len(), 14_724_671);
    assert_eq!(&bytes[..4], b"RBXS", "the pack's container magic");
}

#[test]
fn a_missing_entry_is_a_typed_error_naming_the_archive() {
    let Some(apk) = fixture() else { return };

    match apk.require_entry("lib/x86_64/libroblox.so") {
        Err(ApkError::EntryNotFound { name, path }) => {
            assert_eq!(name, "lib/x86_64/libroblox.so");
            assert!(path.ends_with(FIXTURE), "the error must name the archive");
        }
        Err(other) => panic!("wrong error: {other}"),
        Ok(entry) => panic!("found an entry that cannot exist: {}", entry.name()),
    }
    assert!(apk.entry("lib/x86_64/libroblox.so").is_none());
    assert!(apk.asset_entry("no/such/asset").is_none());
}

// -------------------------------------------------------------------------------------------
// The extraction cache
// -------------------------------------------------------------------------------------------

#[test]
fn extracting_libroblox_produces_an_aligned_content_addressed_file_then_hits() {
    let Some(apk) = fixture() else { return };
    let temp = TempDir::new("libroblox");
    let cache = LibraryCache::new(temp.path());
    let entry = apk
        .require_entry("lib/arm64-v8a/libroblox.so")
        .expect("libroblox.so");

    assert!(
        cache.lookup(&apk, entry).expect("an empty cache must look up cleanly").is_none(),
        "an empty cache cannot have a hit"
    );

    let started = std::time::Instant::now();
    let cached = cache.extract(&apk, entry).expect("extraction must succeed");
    let extract_elapsed = started.elapsed();
    println!(
        "extracted libroblox.so ({} bytes) in {:?}",
        cached.len(),
        extract_elapsed
    );

    assert_eq!(cached.outcome(), CacheOutcome::Extracted);
    assert_eq!(cached.len(), 109_193_800, "extracted length");
    assert_eq!(
        cached.payload_offset(),
        0,
        "the cache file is the library and nothing else"
    );
    assert!(cached.is_payload_aligned(MAPPING_ALIGNMENT), "4 KB aligned");
    assert!(cached.is_payload_aligned(16_384), "and 16 KB aligned");
    assert!(cached.is_directly_mappable());

    // The path is the content hash, under <root>/libs/<hex>/<file name>.
    let hex = cached.sha256_hex();
    assert_eq!(hex.len(), 64);
    assert_eq!(
        cached.path(),
        temp.path().join(LIBS_DIR).join(&hex).join("libroblox.so"),
        "cache layout"
    );

    // The file on disk really is that long, and really does hash to its own name.
    let metadata = fs::metadata(cached.path()).expect("the cache file must exist");
    assert!(metadata.is_file());
    assert_eq!(metadata.len(), 109_193_800, "on-disk length");
    let on_disk = sha256_of_file(cached.path());
    assert_eq!(&on_disk, cached.sha256(), "cache key is the content hash");
    assert_eq!(hex_lower(&on_disk), hex, "and the directory name is that hash");

    // Extracting again must not decompress anything.
    let started = std::time::Instant::now();
    let again = cache.extract(&apk, entry).expect("the second call must succeed");
    let hit_elapsed = started.elapsed();
    println!("second call took {hit_elapsed:?}");
    assert_eq!(
        again.outcome(),
        CacheOutcome::Reused,
        "the second call must be an observable cache hit"
    );
    assert!(again.outcome().is_hit());
    assert!(!again.outcome().did_work());
    assert_eq!(again.path(), cached.path());
    assert_eq!(again.sha256(), cached.sha256());
    assert_eq!(again.len(), cached.len());
    assert!(
        hit_elapsed * 10 < extract_elapsed,
        "a hit ({hit_elapsed:?}) must be dramatically cheaper than an extraction \
         ({extract_elapsed:?}); anything else means it re-extracted"
    );

    // And `lookup` alone sees it, without extracting.
    let looked_up = cache
        .lookup(&apk, entry)
        .expect("lookup must succeed")
        .expect("the entry must now be cached");
    assert_eq!(looked_up.path(), cached.path());
    assert_eq!(looked_up.outcome(), CacheOutcome::Reused);
}

#[test]
fn a_cache_entry_of_the_wrong_size_is_not_reused() {
    let Some(apk) = fixture() else { return };
    let temp = TempDir::new("truncated");
    let cache = LibraryCache::new(temp.path());
    let entry = apk
        .require_entry("lib/arm64-v8a/libyuv_shared.so")
        .expect("libyuv_shared.so");

    let first = cache.extract(&apk, entry).expect("first extraction");
    assert_eq!(first.outcome(), CacheOutcome::Extracted);

    // Truncate the cache file, as an interrupted writer that did *not* use temp-then-rename would
    // have left it.
    fs::write(first.path(), b"partial").expect("truncating the cache file");
    match cache.lookup(&apk, entry) {
        Err(ApkError::CacheSizeMismatch {
            expected, actual, ..
        }) => {
            assert_eq!(expected, entry.uncompressed_size());
            assert_eq!(actual, 7);
        }
        Err(other) => panic!("wrong error for a short cache file: {other}"),
        Ok(hit) => panic!("a 7-byte cache file was accepted: {hit:?}"),
    }

    // `extract` must repair it rather than trusting it or failing.
    let repaired = cache.extract(&apk, entry).expect("re-extraction");
    assert_eq!(repaired.outcome(), CacheOutcome::Extracted);
    assert_eq!(repaired.path(), first.path());
    assert_eq!(
        fs::metadata(repaired.path()).expect("metadata").len(),
        entry.uncompressed_size()
    );
    entry
        .verify_crc32(&fs::read(repaired.path()).expect("reading the repaired file"))
        .expect("the repaired cache file must match the APK's CRC-32");
}

#[test]
fn a_missing_cache_file_behind_a_live_index_is_re_extracted() {
    let Some(apk) = fixture() else { return };
    let temp = TempDir::new("stale-index");
    let cache = LibraryCache::new(temp.path());
    let entry = apk
        .require_entry("lib/arm64-v8a/libeigen_lapack.so")
        .expect("libeigen_lapack.so");

    let first = cache.extract(&apk, entry).expect("first extraction");
    fs::remove_file(first.path()).expect("deleting the cache file");
    assert!(
        cache.lookup(&apk, entry).expect("lookup must not fail").is_none(),
        "an index entry pointing at nothing is not a hit"
    );
    let second = cache.extract(&apk, entry).expect("re-extraction");
    assert_eq!(second.outcome(), CacheOutcome::Extracted);
    assert_eq!(second.path(), first.path());
}

#[test]
fn concurrent_extraction_of_one_library_publishes_exactly_one_file() {
    let Some(apk) = fixture() else { return };
    let temp = TempDir::new("race");
    let cache = LibraryCache::new(temp.path());
    let entry = apk
        .require_entry("lib/arm64-v8a/libsurface_util_jni.so")
        .expect("libsurface_util_jni.so");

    const THREADS: usize = 8;
    let results: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| scope.spawn(|| cache.extract(&apk, entry)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("no extractor may panic"))
            .collect()
    });

    let first = results[0]
        .as_ref()
        .unwrap_or_else(|e| panic!("extraction 0 failed: {e}"));
    for (index, result) in results.iter().enumerate() {
        let cached = result
            .as_ref()
            .unwrap_or_else(|e| panic!("extraction {index} failed: {e}"));
        assert_eq!(cached.path(), first.path(), "all racers agree on the path");
        assert_eq!(cached.sha256(), first.sha256(), "and on the content hash");
        assert_eq!(cached.len(), entry.uncompressed_size());
    }

    // Exactly one file was published, it is complete, and it matches the APK.
    let dir = first.path().parent().expect("the hash directory");
    let published: Vec<_> = fs::read_dir(dir)
        .expect("reading the hash directory")
        .map(|e| e.expect("a directory entry").file_name())
        .collect();
    assert_eq!(
        published.len(),
        1,
        "exactly one file under the content hash, found {published:?}"
    );
    let bytes = fs::read(first.path()).expect("reading the published file");
    assert_eq!(bytes.len() as u64, entry.uncompressed_size());
    entry
        .verify_crc32(&bytes)
        .expect("the published file must match the APK's CRC-32");

    // No temporary file survived. This is the thing that makes the cache safe to map: the only
    // place a partial file can ever exist is the tmp directory, and nothing is left there.
    let leftovers: Vec<_> = fs::read_dir(temp.path().join("tmp"))
        .expect("the tmp directory must exist")
        .map(|e| e.expect("a directory entry").file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
}

#[test]
fn the_cache_refuses_entries_that_are_not_native_libraries() {
    let Some(apk) = fixture() else { return };
    let temp = TempDir::new("not-a-library");
    let cache = LibraryCache::new(temp.path());

    for name in ["classes.dex", "AndroidManifest.xml", "resources.arsc"] {
        let entry = apk.require_entry(name).expect(name);
        match cache.extract(&apk, entry) {
            Err(ApkError::NotANativeLibrary { name: reported }) => assert_eq!(reported, name),
            Err(other) => panic!("wrong error for {name}: {other}"),
            Ok(cached) => panic!("{name} was extracted as a library: {cached:?}"),
        }
        assert!(matches!(
            cache.lookup(&apk, entry),
            Err(ApkError::NotANativeLibrary { .. })
        ));
    }
}

/// Render 32 bytes as lowercase hex. The crate does this internally; the test does it independently
/// so that "the directory is named after the hash" is checked rather than assumed.
fn hex_lower(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
