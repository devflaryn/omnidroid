//! **The ART boot image, uncompressed, so every process maps it instead of unpacking a copy**
//! (`OMNI_BOOT_IMAGE_UNCOMPRESSED=1`; off by default).
//!
//! The image's boot `.art` files (`/system/framework/<isa>/boot*.art`) are stored LZ4HC-compressed
//! (8.2 MiB on disk for ~30 MiB of image: `boot-framework.art` 4.1 MB for 13.9 MB). ART cannot map
//! a compressed image: every ART process -- each small app's host process and the game's --
//! reserves the image's range and **decompresses all of it into private anonymous memory**, then
//! relocates it by a random delta (writing pointers on nearly every page). MEASURED (vmcensus, a
//! small app host process, 2026-10-09): ~26 MiB of its ~72 MiB private memory is that range.
//!
//! Uncompressed, AOSP 15's `ImageSpace` (`LoadImageFile`) maps the file `MAP_PRIVATE` straight into
//! its reservation when no block is compressed; with relocation off (`-Xnorelocate`, ART's
//! `Opt::Relocate`, added to `dalvik.vm.extra-opts` by `crate::props` when this is on) the image
//! sits at the address it was compiled for and nothing writes it but the runtime's own updates.
//! This layer maps a private writable mapping of a sysroot file as a copy-on-write **view** of the
//! host file (`crate::mm`): pages no one writes are the host's file cache, shared by every host
//! process, and a written page becomes private alone (per 4 KiB page, `PAGE_WRITECOPY`).
//!
//! **What the rewrite changes, and what it keeps.** The file is rewritten, not the image: the
//! header's `data_size` becomes the image size less the header, `blocks_offset`/`blocks_count` 0,
//! and the bitmap section's offset where the bitmap now starts (the first 16 KiB boundary after
//! the image -- ART's `kElfSegmentAlignment`, from which `Loader::Init` computes it and checks
//! `end of bitmap == file size`). The image bytes are the LZ4 blocks decompressed at their
//! `image_offset`s; every checksum field (`image_checksum`, `oat_checksum`, `boot_image_checksum`)
//! is left as it was -- ART compares those fields with the oat files' and extensions' records, and
//! they describe the same image.
//!
//! **Where the files go.** Never over the sysroot's own objects: `<sysroot>/boot-uncompressed/
//! <source sha256>.art` (written beside and renamed, with `<source sha256>.sha256` holding the new
//! file's digest and size), substituted for the image's files in this process's view of the
//! sysroot (`crate::vfs::Sysroot::open`) only when the switch is on. A saved device is keyed with
//! [`key_suffix`] so one made without the switch is not booted with it.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use crate::manifest::{Entry, Manifest};

/// `OMNI_BOOT_IMAGE_UNCOMPRESSED=1`.
#[must_use]
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_BOOT_IMAGE_UNCOMPRESSED").as_deref() == Ok("1"))
}

/// What a saved device's name carries when the switch is on (`-bootu`), else nothing.
#[must_use]
pub fn key_suffix() -> &'static str {
    if enabled() {
        "-bootu"
    } else {
        ""
    }
}

/// The ART option added to `dalvik.vm.extra-opts` when the switch is on.
pub const NO_RELOCATE: &str = "-Xnorelocate";

/// The image header of this AOSP's ART (`art/runtime/oat/image.h`, version `111`): 66 words.
pub const HEADER_SIZE: usize = 264;
/// ART's `kElfSegmentAlignment` on this image (16 KiB pages supported): the bitmap starts at the
/// first such boundary after the stored data.
pub const SEGMENT_ALIGNMENT: usize = 16 * 1024;
const MAGIC: &[u8; 4] = b"art\n";
const VERSION: &[u8; 4] = b"111\0";
// Byte offsets of the header fields used here.
const IMAGE_BEGIN: usize = 16;
const IMAGE_SIZE: usize = 20;
/// `sections_[kSectionImageBitmap]` (the 13th section, after 18 words of fields).
const BITMAP_OFFSET: usize = (18 + 2 * 12) * 4;
const BITMAP_SIZE: usize = BITMAP_OFFSET + 4;
const DATA_SIZE: usize = 62 * 4;
const BLOCKS_OFFSET: usize = 63 * 4;
const BLOCKS_COUNT: usize = 64 * 4;
/// `ImageHeader::StorageMode`.
const STORAGE_UNCOMPRESSED: u32 = 0;
const STORAGE_LZ4: u32 = 1;
const STORAGE_LZ4HC: u32 = 2;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("four bytes"))
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// The address the image was compiled for (`image_begin`), from a header.
#[must_use]
pub fn image_begin(header: &[u8]) -> Option<u32> {
    (header.len() >= HEADER_SIZE && &header[..4] == MAGIC).then(|| u32_at(header, IMAGE_BEGIN))
}

/// An LZ4 *block* (no frame) decompressed: exactly `out_len` bytes, and the whole of `src` used.
///
/// # Errors
/// A malformed block: a match before the start, a run past the end, or a length other than
/// `out_len`.
pub fn lz4_block(src: &[u8], out_len: usize) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(out_len);
    let mut i = 0usize;
    let len_ext = |i: &mut usize, mut n: usize| -> Result<usize, String> {
        loop {
            let b = *src.get(*i).ok_or("a length runs past the block")?;
            *i += 1;
            n += usize::from(b);
            if b != 255 {
                return Ok(n);
            }
        }
    };
    while i < src.len() {
        let token = src[i];
        i += 1;
        let mut literals = usize::from(token >> 4);
        if literals == 15 {
            literals = len_ext(&mut i, literals)?;
        }
        let lit = src.get(i..i + literals).ok_or("literals run past the block")?;
        if out.len() + literals > out_len {
            return Err("more output than the block's image size".into());
        }
        out.extend_from_slice(lit);
        i += literals;
        if i == src.len() {
            break; // the last sequence has literals only
        }
        let offset = usize::from(*src.get(i).ok_or("a cut offset")?) | usize::from(*src.get(i + 1).ok_or("a cut offset")?) << 8;
        i += 2;
        if offset == 0 || offset > out.len() {
            return Err(format!("a match {offset} back from {} bytes", out.len()));
        }
        let mut length = usize::from(token & 15);
        if length == 15 {
            length = len_ext(&mut i, length)?;
        }
        length += 4;
        if out.len() + length > out_len {
            return Err("more output than the block's image size".into());
        }
        let from = out.len() - offset;
        for k in 0..length {
            let b = out[from + k];
            out.push(b);
        }
    }
    if out.len() != out_len {
        return Err(format!("{} bytes out, the block says {out_len}", out.len()));
    }
    Ok(out)
}

/// `art`, a boot image file, rewritten uncompressed; `Ok(None)` if no block of it is compressed.
///
/// # Errors
/// A file this ART's layout does not describe (magic, version, sizes, blocks), refused rather than
/// guessed at: the caller then keeps the image's own file.
pub fn uncompress(art: &[u8]) -> Result<Option<Vec<u8>>, String> {
    if art.len() < HEADER_SIZE || &art[..4] != MAGIC {
        return Err("not an ART image".into());
    }
    if &art[4..8] != VERSION {
        return Err(format!("image version {:?}, not 111", String::from_utf8_lossy(&art[4..7])));
    }
    let image_size = u32_at(art, IMAGE_SIZE) as usize;
    let data_size = u32_at(art, DATA_SIZE) as usize;
    let (blocks_offset, blocks_count) = (u32_at(art, BLOCKS_OFFSET) as usize, u32_at(art, BLOCKS_COUNT) as usize);
    let (bitmap_offset, bitmap_size) = (u32_at(art, BITMAP_OFFSET) as usize, u32_at(art, BITMAP_SIZE) as usize);
    if image_size < HEADER_SIZE {
        return Err(format!("image size {image_size}"));
    }
    // What ART's Loader::Init checks of the file as it is.
    if (HEADER_SIZE + data_size).next_multiple_of(SEGMENT_ALIGNMENT) != bitmap_offset || bitmap_offset + bitmap_size != art.len() {
        return Err(format!("the bitmap ({bitmap_offset}+{bitmap_size}) is not where the stored data ({data_size}) puts it in {} bytes", art.len()));
    }
    if blocks_count == 0 {
        return Ok(None);
    }
    let table = art.get(blocks_offset..blocks_offset + 20 * blocks_count).ok_or("the block table runs past the file")?;
    let mut image = vec![0u8; image_size];
    image[..HEADER_SIZE].copy_from_slice(&art[..HEADER_SIZE]);
    let mut covered = HEADER_SIZE;
    for block in table.chunks(20) {
        let (mode, data_offset, data_len, image_offset, image_len) =
            (u32_at(block, 0), u32_at(block, 4) as usize, u32_at(block, 8) as usize, u32_at(block, 12) as usize, u32_at(block, 16) as usize);
        let stored = art.get(data_offset..data_offset + data_len).ok_or("a block's data runs past the file")?;
        if image_offset != covered || image_offset + image_len > image_size {
            return Err(format!("a block at image offset {image_offset} (+{image_len}) after {covered} of {image_size}"));
        }
        let bytes = match mode {
            STORAGE_UNCOMPRESSED if data_len == image_len => stored.to_vec(),
            STORAGE_LZ4 | STORAGE_LZ4HC => lz4_block(stored, image_len)?,
            other => return Err(format!("storage mode {other}")),
        };
        image[image_offset..image_offset + image_len].copy_from_slice(&bytes);
        covered = image_offset + image_len;
    }
    if covered != image_size {
        return Err(format!("the blocks cover {covered} of {image_size} bytes"));
    }
    let new_bitmap = image_size.next_multiple_of(SEGMENT_ALIGNMENT);
    put_u32(&mut image, DATA_SIZE, (image_size - HEADER_SIZE) as u32);
    put_u32(&mut image, BLOCKS_OFFSET, 0);
    put_u32(&mut image, BLOCKS_COUNT, 0);
    put_u32(&mut image, BITMAP_OFFSET, new_bitmap as u32);
    image.resize(new_bitmap, 0);
    image.extend_from_slice(&art[bitmap_offset..bitmap_offset + bitmap_size]);
    Ok(Some(image))
}

/// Whether `guest` is one of the image's boot image files.
#[must_use]
pub fn is_boot_art(guest: &[u8]) -> bool {
    let p = String::from_utf8_lossy(guest);
    p.starts_with("/system/framework/") && p.rsplit('/').next().is_some_and(|n| n.starts_with("boot") && n.ends_with(".art"))
}

/// Substitute the uncompressed boot image files for the image's in `manifest` (and `overlay`,
/// where a file's content is on the host by digest), making them under `dir/boot-uncompressed`
/// (or the temporary directory, if that cannot be written) the first time. How many were
/// substituted; a file that cannot be rewritten keeps the image's own (and says why).
pub fn substitute(dir: &Path, manifest: &mut Manifest, overlay: &mut HashMap<String, PathBuf>) -> usize {
    let wanted: Vec<(Vec<u8>, u32, String)> = manifest
        .entries
        .iter()
        .filter_map(|(path, e)| match e {
            Entry::File { mode, sha256, .. } if is_boot_art(path) => Some((path.clone(), *mode, sha256.clone())),
            _ => None,
        })
        .collect();
    let mut done = 0;
    for (path, mode, sha) in wanted {
        let source = dir.join("objects").join(&sha[..2]).join(&sha);
        match cached(dir, &sha, &source) {
            Ok((host, digest, size)) => {
                overlay.insert(digest.clone(), host);
                manifest.entries.insert(path, Entry::File { mode, size, sha256: digest });
                done += 1;
            }
            Err(e) => eprintln!("[bootimage] {} kept compressed: {e}", String::from_utf8_lossy(&path)),
        }
    }
    done
}

/// The uncompressed file for source object `sha`: (where, its sha256, its size), made if missing.
fn cached(dir: &Path, sha: &str, source: &Path) -> Result<(PathBuf, String, u64), String> {
    let mut last = String::new();
    for root in [dir.join("boot-uncompressed"), std::env::temp_dir().join("omni-boot-uncompressed")] {
        let (file, note) = (root.join(format!("{sha}.art")), root.join(format!("{sha}.sha256")));
        if let (Ok(text), Ok(meta)) = (std::fs::read_to_string(&note), std::fs::metadata(&file)) {
            let mut it = text.split_whitespace();
            if let (Some(digest), Some(size)) = (it.next(), it.next().and_then(|s| s.parse::<u64>().ok())) {
                if size == meta.len() && digest.len() == 64 {
                    return Ok((file, digest.to_string(), size));
                }
            }
        }
        let bytes = std::fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?;
        let image = uncompress(&bytes)?.ok_or("not compressed")?;
        let digest = format!("{:x}", Sha256::digest(&image));
        let made = (|| -> std::io::Result<()> {
            std::fs::create_dir_all(&root)?;
            let tmp = root.join(format!("{sha}.art.{}", std::process::id()));
            std::fs::write(&tmp, &image)?;
            match std::fs::rename(&tmp, &file) {
                Ok(()) => {}
                // Another process made it first (and may have it mapped): keep that one.
                Err(_) if std::fs::metadata(&file).map(|m| m.len()).ok() == Some(image.len() as u64) => {
                    let _ = std::fs::remove_file(&tmp);
                }
                Err(e) => return Err(e),
            }
            let tmp = root.join(format!("{sha}.sha256.{}", std::process::id()));
            std::fs::write(&tmp, format!("{digest} {}\n", image.len()))?;
            let _ = std::fs::rename(&tmp, &note);
            Ok(())
        })();
        match made {
            Ok(()) => return Ok((file, digest, image.len() as u64)),
            Err(e) => last = format!("{}: {e}", root.display()),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lz4_literals_and_an_overlapping_match() {
        // "abc" then a match of 9 at offset 3 (overlapping): "abcabcabcabc", then literals "xy".
        let block = [0x35, b'a', b'b', b'c', 3, 0, 0x20, b'x', b'y'];
        assert_eq!(lz4_block(&block, 14).unwrap(), b"abcabcabcabcxy");
        assert!(lz4_block(&block, 13).is_err(), "a length other than the image size is refused");
        assert!(lz4_block(&[0x10, b'a', 2, 0], 10).is_err(), "a match from before the start is refused");
    }

    #[test]
    fn lz4_long_lengths() {
        // 20 literals (15 + 5), then a match of 4 + 15 + 255 + 1 at offset 1.
        let mut block = vec![0xFF, 5];
        block.extend(std::iter::repeat_n(b'z', 20));
        block.extend([1, 0, 255, 1]);
        block.push(0x00); // a final, empty, sequence of literals
        let out = lz4_block(&block, 20 + 275).unwrap();
        assert!(out.iter().all(|&b| b == b'z'));
    }

    /// A synthetic image file: header, one LZ4 block, the block table, the bitmap at 16 KiB.
    fn synthetic(image_len: usize) -> (Vec<u8>, Vec<u8>) {
        let mut image = vec![0u8; image_len];
        image[..4].copy_from_slice(MAGIC);
        image[4..8].copy_from_slice(VERSION);
        put_u32(&mut image, IMAGE_BEGIN, 0x7000_0000);
        put_u32(&mut image, IMAGE_SIZE, image_len as u32);
        for (i, b) in image[HEADER_SIZE..].iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        // Stored as one "LZ4" block of literals only.
        let body = &image[HEADER_SIZE..];
        let mut block = vec![0xF0];
        let mut n = body.len() - 15;
        while n >= 255 {
            block.push(255);
            n -= 255;
        }
        block.push(n as u8);
        block.extend_from_slice(body);
        let mut file = image[..HEADER_SIZE].to_vec();
        file.extend_from_slice(&block);
        while file.len() % 4 != 0 {
            file.push(0);
        }
        let table_at = file.len();
        for v in [STORAGE_LZ4HC, HEADER_SIZE as u32, block.len() as u32, HEADER_SIZE as u32, body.len() as u32] {
            file.extend_from_slice(&v.to_le_bytes());
        }
        let data = file.len() - HEADER_SIZE;
        put_u32(&mut file, DATA_SIZE, data as u32);
        put_u32(&mut file, BLOCKS_OFFSET, table_at as u32);
        put_u32(&mut file, BLOCKS_COUNT, 1);
        let bitmap_at = file.len().next_multiple_of(SEGMENT_ALIGNMENT);
        put_u32(&mut file, BITMAP_OFFSET, bitmap_at as u32);
        put_u32(&mut file, BITMAP_SIZE, 48);
        file.resize(bitmap_at, 0);
        file.extend(std::iter::repeat_n(0xB1, 48));
        (file, image)
    }

    #[test]
    fn a_rewritten_image_is_the_image_at_its_offsets_and_its_bitmap_where_art_looks() {
        let (file, image) = synthetic(70_000);
        let out = uncompress(&file).unwrap().expect("compressed");
        let image_size = image.len();
        // The image bytes after the header, at the same offsets as in memory.
        assert_eq!(&out[HEADER_SIZE..image_size], &image[HEADER_SIZE..]);
        // The header: as it was, but the storage fields.
        assert_eq!(u32_at(&out, DATA_SIZE) as usize, image_size - HEADER_SIZE);
        assert_eq!((u32_at(&out, BLOCKS_OFFSET), u32_at(&out, BLOCKS_COUNT)), (0, 0));
        assert_eq!(&out[..DATA_SIZE.min(BITMAP_OFFSET)], &file[..DATA_SIZE.min(BITMAP_OFFSET)]);
        // ART's check: the bitmap at the first 16 KiB boundary after header + data, ending the file.
        let bitmap_at = (HEADER_SIZE + u32_at(&out, DATA_SIZE) as usize).next_multiple_of(SEGMENT_ALIGNMENT);
        assert_eq!(u32_at(&out, BITMAP_OFFSET) as usize, bitmap_at);
        assert_eq!(bitmap_at + u32_at(&out, BITMAP_SIZE) as usize, out.len());
        assert!(out[bitmap_at..].iter().all(|&b| b == 0xB1));
        // Already uncompressed: left alone.
        assert_eq!(uncompress(&out).unwrap(), None);
    }

    #[test]
    fn foreign_layouts_are_refused() {
        let (mut file, _) = synthetic(20_000);
        file[4..8].copy_from_slice(b"110\0");
        assert!(uncompress(&file).is_err());
        let (mut file, _) = synthetic(20_000);
        let n = file.len();
        file.truncate(n - 1);
        assert!(uncompress(&file).is_err(), "a file that does not end at its bitmap");
    }

    #[test]
    fn substitution_replaces_the_entry_never_the_object_and_reuses_what_it_made() {
        let dir = std::env::temp_dir().join(format!("omni-bootimage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (file, _) = synthetic(40_000);
        let sha = format!("{:x}", Sha256::digest(&file));
        let object = dir.join("objects").join(&sha[..2]).join(&sha);
        std::fs::create_dir_all(object.parent().unwrap()).unwrap();
        std::fs::write(&object, &file).unwrap();
        let path = b"/system/framework/arm64/boot.art".to_vec();
        let mut manifest = Manifest::default();
        manifest.entries.insert(path.clone(), Entry::File { mode: 0o644, size: file.len() as u64, sha256: sha.clone() });
        manifest.entries.insert(b"/system/framework/arm64/boot.oat".to_vec(), Entry::File { mode: 0o644, size: 1, sha256: "00".repeat(32) });
        let mut overlay = HashMap::new();
        assert_eq!(substitute(&dir, &mut manifest, &mut overlay), 1, "the .art only");
        let Some(Entry::File { size, sha256, mode }) = manifest.entries.get(&path).cloned() else { panic!("an entry") };
        let host = overlay.get(&sha256).expect("the overlay names it").clone();
        let bytes = std::fs::read(&host).unwrap();
        assert_eq!((bytes.len() as u64, mode), (size, 0o644));
        assert_eq!(format!("{:x}", Sha256::digest(&bytes)), sha256);
        assert_eq!(std::fs::read(&object).unwrap(), file, "the source object is untouched");
        // Again (another process): the file made is found, not made again.
        let stamp = std::fs::metadata(&host).unwrap().modified().unwrap();
        let mut again = Manifest::default();
        again.entries.insert(path.clone(), Entry::File { mode: 0o644, size: file.len() as u64, sha256: sha });
        let mut overlay2 = HashMap::new();
        assert_eq!(substitute(&dir, &mut again, &mut overlay2), 1);
        assert_eq!(overlay2.get(&sha256), Some(&host));
        assert_eq!(std::fs::metadata(&host).unwrap().modified().unwrap(), stamp);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_saved_device_key_and_the_paths() {
        assert!(is_boot_art(b"/system/framework/arm64/boot-framework.art"));
        assert!(is_boot_art(b"/system/framework/arm64/boot.art"));
        assert!(!is_boot_art(b"/system/framework/oat/arm64/services.art"));
        assert!(!is_boot_art(b"/system/framework/arm64/boot.oat"));
        assert_eq!(key_suffix(), if enabled() { "-bootu" } else { "" });
    }
}
