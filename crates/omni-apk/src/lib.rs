//! APK reading and the 4 KB-aligned native-library extraction cache.
//!
//! Two jobs, and they are the same job seen from two ends.
//!
//! * [`Apk`] parses the zip container far enough to answer the only question that matters at this
//!   layer: for each entry, **where do its bytes start in the file, and can they be used where they
//!   lie?** That is [`ZipEntry::payload_offset`] and [`ZipEntry::is_directly_mappable`].
//! * [`LibraryCache`] answers the cases where they cannot. For every `lib/<abi>/*.so` it produces a
//!   file whose contents are the library and nothing else, so the bytes start at offset 0 and are
//!   aligned to any page size, shared between instances, and mappable executable (D11).
//!
//! # What this crate does not do
//!
//! * **No signature verification.** Nothing here reads `META-INF/`, the APK Signing Block, or any
//!   certificate, and nothing here reports on them. The supplied test APK is signed by a
//!   non-Roblox key (D6); this crate neither knows nor cares. Anything that needs to trust an APK's
//!   provenance must verify it somewhere else.
//! * **No binary XML decoding.** [`Apk::read_manifest`] hands back the raw AXML bytes of
//!   `AndroidManifest.xml`. Decoding them is somebody else's format.
//! * **No mapping.** The cache returns a *path*. On Windows a file that will ever be mapped
//!   executable must be opened `GENERIC_READ | GENERIC_EXECUTE` and sectioned `PAGE_EXECUTE_READ`
//!   from the very start, because a view of a read-only section can never be made executable
//!   afterwards (D11). Only the caller knows whether it wants that, so only the caller opens the
//!   file.
//!
//! # Portability
//!
//! Pure file and compute work: no `cfg(target_os)`, no OS crate, no `unsafe` anywhere (Global
//! Constraints 4 and 5). It does not even depend on `omni-platform`.
//!
//! # Example
//!
//! ```no_run
//! use omni_apk::{Apk, LibraryCache};
//!
//! let apk = Apk::open("Roblox-2.738.1397.apk")?;
//! let cache = LibraryCache::new("cache");
//!
//! for library in apk.native_libraries_for_abi("arm64-v8a") {
//!     if library.entry().is_directly_mappable() {
//!         // STORED and page-aligned: map it straight out of the APK, no extraction needed.
//!         continue;
//!     }
//!     let cached = cache.extract(&apk, library.entry())?;
//!     assert!(cached.is_directly_mappable());
//!     // `cached.path()` is now the caller's to open, with execute intent if it needs one.
//! }
//! # Ok::<(), omni_apk::ApkError>(())
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod apk;
mod cache;
mod error;
mod zip;

pub use crate::apk::{
    Apk, NativeLibrary, ANDROID_MANIFEST_ENTRY, ASSETS_PREFIX, LIB_PREFIX,
};
pub use crate::cache::{CacheOutcome, CachedLibrary, LibraryCache, LIBS_DIR};
pub use crate::error::{compression_method_name, ApkError, ApkResult};
pub use crate::zip::{
    mapping_alignment, CompressionMethod, EndOfCentralDirectory, MapWindow, ZipEntry,
    MAX_DEFLATE_EXPANSION, MAX_INTERESTING_ALIGNMENT,
};
