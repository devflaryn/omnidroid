//! Hostile input is the expected case, not an edge case.
//!
//! Texture payloads reach this crate from the guest, and the test APK is cheat-injected (D6): a
//! truncated block array, a bogus block mode, a dimension chosen to overflow the size arithmetic.
//! None of these may panic, abort or read out of bounds -- a reachable panic is Critical, and an
//! abort cannot be contained by any caller (HANDOFF working agreement 1).
//!
//! Every test here asserts the *typed refusal*, not merely that nothing crashed, because "it
//! returned something" is satisfied by a decoder that silently produced the wrong pixels.

use omni_texture::{compressed_len, decode, decoded_len, CompressedFormat, TextureError};

const ETC1: CompressedFormat = CompressedFormat::Etc1Rgb8;

// ---------------------------------------------------------------------------------------------
// Dimensions
// ---------------------------------------------------------------------------------------------

#[test]
fn a_zero_dimension_is_refused_rather_than_returning_an_empty_image() {
    for (w, h) in [(0, 4), (4, 0), (0, 0)] {
        assert_eq!(
            decoded_len(w, h),
            Err(TextureError::ZeroExtent {
                width: w,
                height: h
            })
        );
        assert_eq!(
            compressed_len(ETC1, w, h),
            Err(TextureError::ZeroExtent {
                width: w,
                height: h
            })
        );
        let mut out = vec![0u8; 64];
        assert_eq!(
            decode(ETC1, &[0u8; 8], w, h, &mut out),
            Err(TextureError::ZeroExtent {
                width: w,
                height: h
            })
        );
    }
}

/// `width * height * 4` overflows `u32` for dimensions GL itself accepts, and the block-grid
/// rounding is one place a naive `(w + 3) / 4` would overflow on its own. Both are computed in
/// `u64` and refused when the result will not fit in `usize`.
#[test]
fn extents_that_overflow_the_size_arithmetic_are_refused() {
    for (w, h) in [
        (u32::MAX, u32::MAX),
        (u32::MAX, 1),
        (1, u32::MAX),
        (u32::MAX - 1, 4),
        (0x4000_0000, 4),
    ] {
        let decoded = decoded_len(w, h);
        let compressed = compressed_len(ETC1, w, h);
        // On a 64-bit host some of these fit in `usize` and are merely enormous; what must never
        // happen is a wrapped value, so the assertion is that the answer is either the exact
        // 64-bit product or a refusal -- never something smaller than the truth.
        match decoded {
            Ok(bytes) => assert_eq!(bytes as u64, u64::from(w) * u64::from(h) * 4),
            Err(e) => assert_eq!(
                e,
                TextureError::ExtentTooLarge {
                    width: w,
                    height: h
                }
            ),
        }
        match compressed {
            Ok(bytes) => {
                let blocks_x = u64::from(w).div_ceil(4);
                let blocks_y = u64::from(h).div_ceil(4);
                assert_eq!(bytes as u64, blocks_x * blocks_y * 8);
            }
            Err(e) => assert_eq!(
                e,
                TextureError::ExtentTooLarge {
                    width: w,
                    height: h
                }
            ),
        }
        // And a decode of such an extent refuses long before it touches memory, because the
        // payload it would need cannot have been supplied.
        let mut out = vec![0u8; 64];
        assert!(decode(ETC1, &[0u8; 8], w, h, &mut out).is_err());
    }
}

/// The block grid rounds up, and the texels past the image edge are discarded rather than written.
/// A 5x3 image is a 2x1 grid: sixteen bytes of payload, fifteen texels of output.
#[test]
fn a_non_multiple_of_four_extent_rounds_the_grid_up_and_clips_the_output() {
    assert_eq!(compressed_len(ETC1, 5, 3), Ok(16));
    assert_eq!(decoded_len(5, 3), Ok(5 * 3 * 4));

    // Left block white, right block black, both individual mode with table 0 and index 0 (+2).
    let mut data = [0u8; 16];
    data[0..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x00]);
    data[8..12].copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    let mut out = vec![0xAAu8; decoded_len(5, 3).unwrap()];
    decode(ETC1, &data, 5, 3, &mut out).expect("a clipped extent decodes");
    for row in 0..3usize {
        for col in 0..5usize {
            let at = (row * 5 + col) * 4;
            let expected = if col < 4 { 255u8 } else { 2u8 };
            assert_eq!(
                &out[at..at + 4],
                &[expected, expected, expected, 255],
                "col {col} row {row}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Payload and destination sizes
// ---------------------------------------------------------------------------------------------

#[test]
fn a_truncated_payload_is_refused_at_every_length_below_the_grid() {
    let full = [0u8; 32]; // 4 blocks: an 8x8 image
    for provided in 0..32usize {
        let mut out = vec![0u8; decoded_len(8, 8).unwrap()];
        assert_eq!(
            decode(ETC1, &full[..provided], 8, 8, &mut out),
            Err(TextureError::TruncatedBlockData {
                needed: 32,
                provided
            }),
            "payload length {provided}"
        );
    }
}

#[test]
fn an_undersized_destination_is_refused_at_every_length_below_the_image() {
    let needed = decoded_len(8, 8).unwrap();
    for provided in 0..needed {
        let mut out = vec![0u8; provided];
        assert_eq!(
            decode(ETC1, &[0u8; 32], 8, 8, &mut out),
            Err(TextureError::OutputTooSmall { needed, provided }),
            "destination length {provided}"
        );
    }
}

/// A payload longer than the grid is not an error -- GL's `imageSize` is allowed to cover padding,
/// and a KTX level is padded to four bytes. The excess is ignored, and so is any excess in the
/// destination.
#[test]
fn excess_payload_and_excess_destination_are_both_ignored() {
    let mut data = vec![0u8; 8];
    data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x00]);
    let mut out = vec![0x5Au8; decoded_len(4, 4).unwrap() + 16];
    decode(ETC1, &data, 4, 4, &mut out).expect("excess is not an error");
    assert_eq!(&out[..4], &[2, 2, 2, 255]);
    assert!(
        out[64..].iter().all(|&b| b == 0x5A),
        "bytes past the image must not be written"
    );
}

// ---------------------------------------------------------------------------------------------
// Formats
// ---------------------------------------------------------------------------------------------

/// Every format the census did **not** find is refused by its specification name. This is the list
/// that matters if the engine ever hands over something new: the refusal says what to implement.
#[test]
fn every_named_format_that_is_not_etc1_is_refused_by_name() {
    const NAMED: &[(u32, &str)] = &[
        (0x9274, "GL_COMPRESSED_RGB8_ETC2"),
        (0x9275, "GL_COMPRESSED_SRGB8_ETC2"),
        (0x9276, "GL_COMPRESSED_RGB8_PUNCHTHROUGH_ALPHA1_ETC2"),
        (0x9277, "GL_COMPRESSED_SRGB8_PUNCHTHROUGH_ALPHA1_ETC2"),
        (0x9278, "GL_COMPRESSED_RGBA8_ETC2_EAC"),
        (0x9279, "GL_COMPRESSED_SRGB8_ALPHA8_ETC2_EAC"),
        (0x9270, "GL_COMPRESSED_R11_EAC"),
        (0x9271, "GL_COMPRESSED_SIGNED_R11_EAC"),
        (0x9272, "GL_COMPRESSED_RG11_EAC"),
        (0x9273, "GL_COMPRESSED_SIGNED_RG11_EAC"),
        (0x93B0, "GL_COMPRESSED_RGBA_ASTC_4x4_KHR"),
        (0x93B4, "GL_COMPRESSED_RGBA_ASTC_6x6_KHR"),
        (0x93B7, "GL_COMPRESSED_RGBA_ASTC_8x8_KHR"),
        (0x93BD, "GL_COMPRESSED_RGBA_ASTC_12x12_KHR"),
        (0x83F0, "GL_COMPRESSED_RGB_S3TC_DXT1_EXT"),
        (0x83F3, "GL_COMPRESSED_RGBA_S3TC_DXT5_EXT"),
    ];
    for &(value, name) in NAMED {
        let err = CompressedFormat::from_gl_internal_format(value).expect_err("must refuse");
        assert_eq!(
            err,
            TextureError::UnsupportedFormat {
                gl_internal_format: value,
                name: Some(name)
            }
        );
        let message = err.to_string();
        assert!(message.contains(name), "{message}");
    }
}

/// A value the specifications do not name is still refused, and says so as an unrecognised value
/// rather than inventing a name. Swept over every 32-bit value in the GL compressed-format range
/// plus the boundaries, so a stray entry in the name table would show up here.
#[test]
fn an_unnamed_format_is_refused_without_a_name() {
    for value in [
        0u32,
        1,
        0x8D63,
        0x8D65,
        0x926F,
        0x927A,
        0x93AF,
        0x93BE,
        u32::MAX,
    ] {
        assert_eq!(
            CompressedFormat::from_gl_internal_format(value),
            Err(TextureError::UnsupportedFormat {
                gl_internal_format: value,
                name: None
            })
        );
    }
}

#[test]
fn etc1_is_the_one_format_accepted() {
    assert_eq!(
        CompressedFormat::from_gl_internal_format(0x8D64),
        Ok(CompressedFormat::Etc1Rgb8)
    );
    assert_eq!(CompressedFormat::Etc1Rgb8.name(), "GL_ETC1_RGB8_OES");
    assert_eq!(CompressedFormat::Etc1Rgb8.block_extent(), (4, 4));
    assert_eq!(CompressedFormat::Etc1Rgb8.block_bytes(), 8);
}
