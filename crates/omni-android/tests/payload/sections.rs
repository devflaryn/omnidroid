//! **What the gate reports about the APK's second native library**: its digest, for the report
//! line, and which section each of its constructors lives in. Nothing here decides whether a build
//! is loaded -- an APK's libraries are loaded as the APK carries them.

use std::io::Read as _;
use std::path::Path;

use omni_elf::ElfImage;
use sha2::{Digest, Sha256};

/// sha256 of a file, streamed.
pub fn sha256_of_file(path: &Path) -> [u8; 32] {
    let mut file = std::fs::File::open(path)
        .unwrap_or_else(|error| panic!("{} must be readable to hash it: {error}", path.display()));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer).expect("reading a file to hash it");
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
