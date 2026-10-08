//! `omni_linux::boot_image` on the pinned image's own boot `.art` files (`sysroot/aosp-35`, when
//! the checkout has it): each rewrites uncompressed, and the result is what ART's `ImageSpace`
//! loader checks it to be -- no boot needed.
use std::path::{Path, PathBuf};

use omni_linux::boot_image::{image_begin, is_boot_art, lz4_block, uncompress, HEADER_SIZE, SEGMENT_ALIGNMENT};

/// `OMNI_SYSROOT`, or the checkout's `sysroot/aosp-35` (as the gates find it).
fn sysroot() -> Option<PathBuf> {
    let dir = std::env::var_os("OMNI_SYSROOT").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sysroot/aosp-35"), PathBuf::from);
    dir.join("sysroot.manifest").exists().then_some(dir)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// (guest path, file bytes) of every boot `.art` file of the image.
fn boot_images(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let text = std::fs::read_to_string(dir.join("sysroot.manifest")).unwrap();
    let manifest = omni_linux::manifest::parse(&text).unwrap();
    manifest
        .entries
        .iter()
        .filter(|(p, _)| is_boot_art(p))
        .filter_map(|(p, e)| match e {
            omni_linux::manifest::Entry::File { sha256, .. } => {
                Some((String::from_utf8_lossy(p).into_owned(), std::fs::read(dir.join("objects").join(&sha256[..2]).join(sha256)).unwrap()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn every_boot_image_file_rewrites_to_what_art_maps() {
    let Some(dir) = sysroot() else {
        eprintln!("no sysroot/aosp-35 in this checkout: skipped");
        return;
    };
    let images = boot_images(&dir);
    assert_eq!(images.len(), 14, "the pinned image's 14 boot image components");
    let (mut stored, mut mapped) = (0usize, 0usize);
    let mut begins = Vec::new();
    for (path, file) in &images {
        let out = uncompress(file).unwrap_or_else(|e| panic!("{path}: {e}")).unwrap_or_else(|| panic!("{path} was not compressed"));
        let image_size = u32_at(file, 20) as usize;
        // ART's Loader::Init: the bitmap at the first 16 KiB boundary after header + stored data,
        // ending the file; LoadImageFile maps image_size bytes (rounded up) from offset 0.
        let data_size = u32_at(&out, 248) as usize;
        assert_eq!(data_size, image_size - HEADER_SIZE, "{path}");
        let bitmap = (HEADER_SIZE + data_size).next_multiple_of(SEGMENT_ALIGNMENT);
        assert_eq!(u32_at(&out, 168) as usize, bitmap, "{path}");
        assert_eq!(bitmap + u32_at(&out, 172) as usize, out.len(), "{path}");
        assert!(out.len() >= image_size.next_multiple_of(SEGMENT_ALIGNMENT), "{path}: the mapping fits in the file");
        assert_eq!((u32_at(&out, 252), u32_at(&out, 256)), (0, 0), "{path}: no blocks");
        // The bitmap is the original's.
        let (old_at, size) = (u32_at(file, 168) as usize, u32_at(file, 172) as usize);
        assert_eq!(&out[bitmap..], &file[old_at..old_at + size], "{path}");
        // Every other header field as it was: the checksums ART compares with the oat files'.
        assert_eq!(&out[..168], &file[..168], "{path}: the header up to the sections");
        assert_eq!(&out[176..248], &file[176..248], "{path}: the image methods");
        // The image bytes: the one block decompressed independently of `uncompress`.
        let table = u32_at(file, 252) as usize;
        let (data_offset, data_len, image_offset, image_len) =
            (u32_at(file, table + 4) as usize, u32_at(file, table + 8) as usize, u32_at(file, table + 12) as usize, u32_at(file, table + 16) as usize);
        assert_eq!(u32_at(file, 256), 1, "{path}: one block");
        let block = lz4_block(&file[data_offset..data_offset + data_len], image_len).unwrap();
        assert_eq!(&out[image_offset..image_offset + image_len], &block[..], "{path}");
        assert_eq!(image_offset + image_len, image_size, "{path}");
        assert_eq!(uncompress(&out).unwrap(), None, "{path}: idempotent");
        stored += file.len();
        mapped += image_size;
        begins.push((image_begin(&out).unwrap(), image_size, path.clone()));
    }
    // The components lie one after another from 0x70000000, none overlapping.
    begins.sort();
    assert_eq!(begins[0].0, 0x7000_0000, "boot.art first, at ART_BASE_ADDRESS");
    for w in begins.windows(2) {
        assert!(w[0].0 as usize + w[0].1 <= w[1].0 as usize, "{} overlaps {}", w[0].2, w[1].2);
    }
    eprintln!("{} files: {:.1} MiB stored, {:.1} MiB of image mapped instead of unpacked", images.len(), stored as f64 / 1048576.0, mapped as f64 / 1048576.0);
}
