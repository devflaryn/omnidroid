//! The host framebuffer the composer presents into, and its screenshot as a PNG a standard decoder
//! reads back pixel for pixel.
use std::io::Read;
use std::time::Duration;

use omni_linux::hal::framebuffer::Framebuffer;

/// Decode a PNG with no dependency but zlib (flate2): 8-bit RGBA, filter type 0 rows. Enough to
/// check what `Framebuffer::png` writes; a real decoder is `png`'s job, not this test's.
fn decode(png: &[u8]) -> (u32, u32, Vec<u8>) {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "the signature");
    let (mut at, mut idat, mut size) = (8, Vec::new(), (0, 0));
    while at + 8 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().unwrap()) as usize;
        let kind = &png[at + 4..at + 8];
        let body = &png[at + 8..at + 8 + len];
        let crc = u32::from_be_bytes(png[at + 8 + len..at + 12 + len].try_into().unwrap());
        let mut hasher = flate2::Crc::new();
        hasher.update(&png[at + 4..at + 8 + len]);
        assert_eq!(hasher.sum(), crc, "{:?} chunk crc", String::from_utf8_lossy(kind));
        match kind {
            b"IHDR" => {
                size = (u32::from_be_bytes(body[0..4].try_into().unwrap()), u32::from_be_bytes(body[4..8].try_into().unwrap()));
                assert_eq!(&body[8..13], &[8, 6, 0, 0, 0], "8-bit RGBA, no interlace");
            }
            b"IDAT" => idat.extend_from_slice(body),
            _ => {}
        }
        at += 12 + len;
    }
    let mut raw = Vec::new();
    flate2::read::ZlibDecoder::new(&idat[..]).read_to_end(&mut raw).expect("zlib");
    let (w, h) = size;
    let mut px = Vec::new();
    for y in 0..h as usize {
        let row = &raw[y * (1 + w as usize * 4)..(y + 1) * (1 + w as usize * 4)];
        assert_eq!(row[0], 0, "filter type 0");
        px.extend_from_slice(&row[1..]);
    }
    (w, h, px)
}

#[test]
fn a_presented_frame_is_counted_and_screenshotted() {
    let fb = Framebuffer::new(5, 3);
    assert_eq!(fb.frames(), 0);
    let frame: Vec<u8> = (0..5 * 3).flat_map(|i| [i as u8, 2 * i as u8, 255 - i as u8, 255]).collect();
    // A source with a longer stride (8 pixels): only the visible 5 of each row are taken.
    let mut src = vec![0xEEu8; 8 * 3 * 4];
    for y in 0..3 {
        src[y * 32..y * 32 + 20].copy_from_slice(&frame[y * 20..y * 20 + 20]);
    }
    fb.present_rgba(&src, 8);
    assert_eq!(fb.frames(), 1);
    assert!(fb.wait_frame(1, Duration::from_millis(10)), "the frame is there");
    assert!(!fb.wait_frame(2, Duration::from_millis(10)), "no second frame");
    let (w, h, px) = decode(&fb.png());
    assert_eq!((w, h), (5, 3));
    assert_eq!(*px, frame);
}

/// A frame of another size replaces the held one whole: the size, the pixels and the screenshot
/// are the new frame's, and the frame count goes on.
#[test]
fn a_frame_of_a_new_size_replaces_the_old_one() {
    let fb = Framebuffer::new(5, 3);
    fb.present_rgba(&[7; 5 * 3 * 4], 5);
    let frame: Vec<u8> = (0..4 * 2).flat_map(|i| [i as u8, 0, 0, 255]).collect();
    fb.present_frame(&frame, 4, 2, 4);
    assert_eq!(fb.size(), (4, 2));
    let (n, w, h, px) = fb.frame();
    assert_eq!((n, w, h), (2, 4, 2));
    assert_eq!(*px, frame);
    assert_eq!(decode(&fb.png()), (4, 2, frame.clone()));
    // A present at the current size keeps it.
    fb.present_rgba(&frame, 4);
    assert_eq!((fb.size(), fb.frames()), ((4, 2), 3));
}
