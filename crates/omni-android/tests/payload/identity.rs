//! **Which build of the APK's compression JNI library a file holds, and the image to load for it.**
//!
//! # Two builds, and a length cannot tell them apart
//!
//! `lib/arm64-v8a/libzstd-jni-1.5.7-6.so` is the app's compression JNI library, and two builds of
//! that name are in play here. Both were measured out of the APKs themselves, not from any document
//! about them:
//!
//! | build | uncompressed length | sha256 | what it is |
//! |---|---:|---|---|
//! | [`GENUINE`] | 603,960 | `e5ebc383…` | the Zstandard JNI library, 32 imports, no `DT_INIT_ARRAY`, no `JNI_OnLoad` |
//! | [`SUBSTITUTED`] | 9,756,464 | `5b4c66d9…` | a third party's binary under the same name, in a re-signed APK |
//!
//! The gate decides between them by **sha256**, and the length is asserted beside it rather than
//! used as the test. A count cannot see a substitution — that is this project's most expensive
//! lesson, paid three times — and 603,960 against 9,756,464 is a length, so it goes in as a
//! corroborating figure and never as the decision. A pair that matches *neither* build is a third
//! file, and [`build_of`] refuses it by name rather than falling through to "not the known one".
//!
//! # Why the substituted build is loaded already decrypted
//!
//! Its `.text` is encrypted on disk (entropy 8.0 bits/byte against 6.82 decrypted). The decryptor is
//! its own `init_array[0..1]`, living in a section named `.dyncall`, and it issues a raw `svc`
//! `mprotect(.text, RWX)` before writing the plaintext. **This runtime answers that raw `mprotect`**
//! — number 226 is in `sysroute::ROUTES`, dispatched on the exit path like any other route — and
//! **the answer is a refusal**: write and execute at once is not a protection this runtime grants
//! (D12; `omni_android::bionic::guestmem::protection_for` rejects it). So in-place decryption inside
//! the guest is not an available path, and the invariant is not relaxed to make one.
//!
//! What is loaded instead is [`DECRYPTED`]: **the same file with `.text` decrypted in place**,
//! produced offline by emulating those two constructors, and asserted here by digest. The two
//! `.dyncall` entries are then skipped rather than run — they have already run, in the pass that
//! produced the plaintext, and running them again would re-issue the `mprotect` this runtime
//! refuses. [`crate::guest::load`] asserts both skipped addresses lie inside `.dyncall` before
//! skipping them, so "these two and no others" is a measurement rather than a convention.
//!
//! # What would falsify any of this
//!
//! A different digest for either build — a re-signed rebuild of the same version changes the
//! compression library's own bytes only if the library changed, so a new digest here means a new
//! library, not a new signature. A decrypted image whose digest is not [`DECRYPTED`] is a
//! different plaintext than the one this project measured, and the run stops rather than loading it.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use omni_elf::ElfImage;
use sha2::{Digest, Sha256};

/// The entry both APKs carry under this name.
pub const LIBRARY: &str = "libzstd-jni-1.5.7-6.so";

/// A measured build: what it weighs, and what it hashes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Measured {
    /// Uncompressed length, in bytes.
    pub len: u64,
    /// sha256 of exactly those bytes.
    pub sha256: &'static str,
}

/// The stock library, out of `Roblox-2.738.1397.apk`. MEASURED 2026-09-26.
///
/// The digest is new to the record: until now this project's only handle on this build was its
/// length (603,960) and its import count (32).
pub const GENUINE: Measured =
    Measured { len: 603_960, sha256: "e5ebc383da2f10794b1ca23964fb7e202fcb351e83942f0c8fe5d082a119b2c6" };

/// The substituted build, out of `Roblox-2.739.691.apk`. MEASURED 2026-09-26.
pub const SUBSTITUTED: Measured =
    Measured { len: 9_756_464, sha256: "5b4c66d9628056ae8f432c86c25a3ea1a0354358893894040af5b98ce1b9d0a2" };

/// The same bytes as [`SUBSTITUTED`] with `.text` decrypted in place. MEASURED 2026-09-26.
///
/// **Same length as the file it came from**, deliberately: decryption is in place, so the image
/// this project loads is the shipped file with its code section rewritten and nothing else moved.
/// A different length would mean the offline pass did something to the file's shape.
pub const DECRYPTED: Measured =
    Measured { len: 9_756_464, sha256: "68368534b264608ca8adfa1048a42e9f9deddc96d3f9230e2c5c3588dd792286" };

/// Which of the two builds a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// [`GENUINE`]: the stock compression library.
    Genuine,
    /// [`SUBSTITUTED`]: a third party's binary, whose `.text` is encrypted on disk.
    Substituted,
}

impl Build {
    /// The build whose measurement this is.
    #[must_use]
    pub fn measured(self) -> &'static Measured {
        match self {
            Build::Genuine => &GENUINE,
            Build::Substituted => &SUBSTITUTED,
        }
    }

    /// A word for it, for a report line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Build::Genuine => "the stock compression library",
            Build::Substituted => "a substituted build under the same name",
        }
    }

    /// The `init_array` indices this build's image must **not** run.
    ///
    /// Empty for the genuine build: it declares no `DT_INIT_ARRAY` at all, so there is nothing to
    /// skip. For the substituted build, the two `.dyncall` entries — already run by the offline pass
    /// that produced [`DECRYPTED`], and the code that re-issues the `mprotect` this runtime
    /// refuses. Every other entry is a real constructor and is run.
    #[must_use]
    pub fn skipped_constructors(self) -> &'static [usize] {
        match self {
            Build::Genuine => &[],
            Build::Substituted => &[0, 1],
        }
    }
}

/// sha256 of a file, streamed.
pub fn sha256_of_file(path: &Path) -> [u8; 32] {
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("{} must be readable to identify it: {error}", path.display()));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("reading a file to identify it");
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher.finalize().into()
}

/// Lowercase hex, for a report line.
#[must_use]
pub fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Which build a file of this length and digest is.
///
/// `digest_hex` is [`hex`]'s output rather than raw bytes, because that is the form a report line
/// carries and re-deriving the bytes from it here would be a second way to be wrong.
///
/// # Panics
///
/// When the pair is neither [`GENUINE`] nor [`SUBSTITUTED`]. The message names both, because the
/// useful question then is *which file is this* — and a third build of this name in an APK is
/// exactly the case a digest exists to catch.
#[must_use]
pub fn build_of(len: u64, digest_hex: &str) -> Build {
    for (build, measured) in [(Build::Genuine, &GENUINE), (Build::Substituted, &SUBSTITUTED)] {
        if len == measured.len && digest_hex == measured.sha256 {
            return build;
        }
    }
    panic!(
        "this APK's `{LIBRARY}` is neither build this project has measured: {len} bytes, sha256 \
         {digest_hex}.\n  the stock one:        {} bytes, sha256 {}\n  the substituted one: {} bytes, \
         sha256 {}",
        GENUINE.len, GENUINE.sha256, SUBSTITUTED.len, SUBSTITUTED.sha256
    );
}

/// The decrypted image for the substituted build, staged inside this repository.
///
/// `work/extracted/` rather than the analysis workspace it was produced in, so a run never writes
/// back into someone else's tree and a checkout carries the artifact it needs. Asserted by digest
/// and by length before it is returned, so a truncated copy or a stale one is a named failure.
///
/// # Panics
///
/// When the file is absent, naming the path looked for and what the file is.
#[must_use]
pub fn decrypted_image(repo_root: &Path) -> PathBuf {
    let at = repo_root.join("work").join("extracted").join("libzstd-jni-1.5.7-6.decrypted.so");
    assert!(
        at.is_file(),
        "this APK carries the MODIFIED build of `{LIBRARY}` (9.75 MB, `.text` encrypted), not the \
         stock one (604 KB) -- its identity was matched by sha256. Running a modified build with a \
         real account cookie hands the account to whoever repacked the APK, so this run stops here. \
         Use the verified stock APK instead (Roblox 2.738.1397, sha256 \
         bbe00ae306cc251c4ea55b7a932d9c524ecb0d6d9203c2a6161bcf0fae792742), whose stock library \
         needs no staged image. (Only an authorized analysis workspace stages {} for offline study \
         of the modified build.)",
        at.display()
    );
    let len = std::fs::metadata(&at).expect("the decrypted image's length").len();
    let digest = sha256_of_file(&at);
    assert_eq!(
        (len, hex(&digest)),
        (DECRYPTED.len, DECRYPTED.sha256.to_string()),
        "the decrypted image at {} is not the one this project measured",
        at.display()
    );
    at
}

/// Every section's name, resolved through the header's `e_shstrndx`.
///
/// `None` for a header whose name is not inside the table, which is a malformed image rather than
/// an absent name — and it is left as `None` so the caller can say so instead of indexing nothing.
#[must_use]
pub fn section_names<'a>(bytes: &'a [u8], image: &ElfImage<'_>) -> Vec<Option<&'a str>> {
    let sections = image.sections();
    let Some(table) = sections.get(image.header().e_shstrndx as usize) else {
        return vec![None; sections.len()];
    };
    let Some(table) = bytes.get(table.sh_offset as usize..(table.sh_offset + table.sh_size) as usize)
    else {
        return vec![None; sections.len()];
    };
    sections
        .iter()
        .map(|section| {
            let start = section.sh_name as usize;
            let rest = table.get(start..)?;
            let end = rest.iter().position(|byte| *byte == 0)?;
            std::str::from_utf8(&rest[..end]).ok()
        })
        .collect()
}

/// The name of the section a link-time address falls inside.
///
/// The name is borrowed from `names` — the strings live in the image's own section-name table, and
/// [`section_names`] ties them to the bytes it was handed.
#[must_use]
pub fn section_holding<'a>(
    names: &'a [Option<&'a str>],
    image: &ElfImage<'_>,
    addr: u64,
) -> Option<&'a str> {
    image
        .sections()
        .iter()
        .enumerate()
        .filter(|(_, section)| section.sh_addr <= addr && addr < section.sh_addr + section.sh_size)
        .find_map(|(index, _)| names.get(index).copied().flatten())
        .or(Some("<no section>"))
}
