//! **A PNG encoder for the screenshot command, and nothing more.**
//!
//! Eight-bit RGBA, one IDAT, filter type 0 on every row: the smallest file the format permits that
//! every viewer opens, written with the two crates the workspace already has for the APK's zip
//! reading (`flate2` for the zlib stream, `crc32fast` for the chunk checksums) -- so a screenshot
//! costs no new dependency.

use std::io::Write as _;

/// The eight bytes every PNG starts with.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// `rgba` (`width * height * 4` bytes, top row first) as a PNG file.
///
/// # Panics
///
/// If `rgba` is not exactly `width * height * 4` bytes, or a side is zero -- a caller's mistake
/// that would otherwise be a file whose header disagrees with its pixels.
#[must_use]
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    assert!(width > 0 && height > 0, "a {width}x{height} image");
    let stride = width as usize * 4;
    assert_eq!(rgba.len(), stride * height as usize, "RGBA bytes for {width}x{height}");

    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    // Bit depth 8, colour type 6 (RGBA), deflate, adaptive filtering, no interlace.
    header.extend_from_slice(&[8, 6, 0, 0, 0]);

    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in rgba.chunks_exact(stride) {
        raw.push(0); // Filter type 0: the row as it is.
        raw.extend_from_slice(row);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    z.write_all(&raw).expect("writing to a Vec cannot fail");
    let data = z.finish().expect("finishing into a Vec cannot fail");

    let mut out = Vec::with_capacity(data.len() + 64);
    out.extend_from_slice(&SIGNATURE);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &data);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
}

/// Reverse the row order of a `width`-pixel RGBA image in place: GL reads bottom row first.
pub fn flip_rows(width: u32, rgba: &mut [u8]) {
    let stride = width as usize * 4;
    if stride == 0 {
        return;
    }
    let rows = rgba.len() / stride;
    for top in 0..rows / 2 {
        let bottom = rows - 1 - top;
        let (upper, lower) = rgba.split_at_mut(bottom * stride);
        upper[top * stride..top * stride + stride].swap_with_slice(&mut lower[..stride]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// Decode what [`encode_rgba`] writes -- enough of PNG to check it, chunk CRCs included.
    fn decode(file: &[u8]) -> (u32, u32, Vec<u8>) {
        assert_eq!(&file[..8], &SIGNATURE);
        let mut at = 8;
        let (mut width, mut height, mut idat) = (0, 0, Vec::new());
        let mut kinds = Vec::new();
        while at < file.len() {
            let len = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as usize;
            let kind = &file[at + 4..at + 8];
            let data = &file[at + 8..at + 8 + len];
            let crc = u32::from_be_bytes(file[at + 8 + len..at + 12 + len].try_into().unwrap());
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(kind);
            hasher.update(data);
            assert_eq!(hasher.finalize(), crc, "chunk {:?}'s CRC", std::str::from_utf8(kind));
            match kind {
                b"IHDR" => {
                    width = u32::from_be_bytes(data[0..4].try_into().unwrap());
                    height = u32::from_be_bytes(data[4..8].try_into().unwrap());
                    assert_eq!(&data[8..], &[8, 6, 0, 0, 0]);
                }
                b"IDAT" => idat.extend_from_slice(data),
                _ => {}
            }
            kinds.push(String::from_utf8_lossy(kind).into_owned());
            at += 12 + len;
        }
        assert_eq!(kinds, ["IHDR", "IDAT", "IEND"]);
        let mut raw = Vec::new();
        flate2::read::ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).unwrap();
        let stride = width as usize * 4;
        let mut rgba = Vec::new();
        for row in raw.chunks_exact(stride + 1) {
            assert_eq!(row[0], 0, "filter type 0");
            rgba.extend_from_slice(&row[1..]);
        }
        (width, height, rgba)
    }

    #[test]
    fn an_image_round_trips_through_the_encoder() {
        let (width, height) = (7u32, 3u32);
        let rgba: Vec<u8> = (0..width * height * 4).map(|i| (i * 37 % 251) as u8).collect();
        let file = encode_rgba(width, height, &rgba);
        assert_eq!(decode(&file), (width, height, rgba));
    }

    #[test]
    fn rows_flip_top_to_bottom() {
        let mut rgba: Vec<u8> = (0..3u8).flat_map(|row| [row; 8]).collect(); // 2 px wide, 3 rows
        flip_rows(2, &mut rgba);
        let rows: Vec<u8> = rgba.chunks_exact(8).map(|row| row[0]).collect();
        assert_eq!(rows, [2, 1, 0]);
    }
}
