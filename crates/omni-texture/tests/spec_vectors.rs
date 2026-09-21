//! Known-answer tests for `GL_ETC1_RGB8_OES`, every expected value derived from the specification.
//!
//! **No second decoder was used as an oracle.** Each vector below states the bit fields it sets,
//! the specification rule that turns them into a colour, and the arithmetic. That is deliberate and
//! it is the expensive lesson this project already paid for: `Ipv6Addr::Display` was used as the
//! oracle for bionic's `inet_ntop` and disagreed on 43 of 200,000 addresses, because Rust had
//! dropped a deprecated form BIND still prints (D25). Two implementations agreeing proves they
//! agree.
//!
//! Sources: `OES_compressed_ETC1_RGB8_texture`, and OpenGL ES 3.2 section 8.7.3 with tables 8.15
//! (intensity modifier sets) and 8.16 (pixel index to modifier).

use omni_texture::{decode, decoded_len, CompressedFormat, Etc2Mode, TextureError};

const ETC1: CompressedFormat = CompressedFormat::Etc1Rgb8;

/// Decode one 4x4 block and return its 64 RGBA bytes.
fn block(bytes: [u8; 8]) -> Vec<u8> {
    let mut out = vec![0u8; decoded_len(4, 4).expect("4x4 is a valid extent")];
    decode(ETC1, &bytes, 4, 4, &mut out).expect("this vector must decode");
    out
}

/// The RGBA texel at (col, row) of a decoded 4x4 block.
fn texel(rgba: &[u8], col: usize, row: usize) -> [u8; 4] {
    let at = (row * 4 + col) * 4;
    [rgba[at], rgba[at + 1], rgba[at + 2], rgba[at + 3]]
}

/// Build the four index bytes of a block in which every pixel carries the same 2-bit index.
///
/// Bytes 4-7 are a big-endian word whose high half holds every pixel's most-significant index bit
/// and whose low half holds every least-significant bit, so "all pixels = p" is two half-words of
/// all-ones or all-zeroes.
fn uniform_indices(p: u8) -> [u8; 4] {
    let msb: u16 = if p & 0b10 != 0 { 0xFFFF } else { 0 };
    let lsb: u16 = if p & 0b01 != 0 { 0xFFFF } else { 0 };
    [
        (msb >> 8) as u8,
        (msb & 0xFF) as u8,
        (lsb >> 8) as u8,
        (lsb & 0xFF) as u8,
    ]
}

// ---------------------------------------------------------------------------------------------
// Base colours
// ---------------------------------------------------------------------------------------------

/// Individual mode: the high nibble is sub-block 1, the low nibble sub-block 2, each extended by
/// replication, and `flipbit = 0` splits the block left/right.
///
/// Block `F0 0F 88 04 00 00 00 00`:
/// * byte 3 = `0x04` -> table1 = 0, table2 = 1, diffbit = 0, flipbit = 0.
/// * base 1 = (extend4(0xF), extend4(0x0), extend4(0x8)) = (255, 0, 136).
/// * base 2 = (extend4(0x0), extend4(0xF), extend4(0x8)) = (0, 255, 136).
/// * all indices 0 -> modifier `a`, which is 2 for table 0 and 5 for table 1.
#[test]
fn individual_mode_splits_the_nibbles_and_the_block_left_and_right() {
    let rgba = block([0xF0, 0x0F, 0x88, 0x04, 0x00, 0x00, 0x00, 0x00]);
    for row in 0..4 {
        // Left half: base 1 with table 0's +2, red saturating at 255.
        assert_eq!(texel(&rgba, 0, row), [255, 2, 138, 255], "row {row} col 0");
        assert_eq!(texel(&rgba, 1, row), [255, 2, 138, 255], "row {row} col 1");
        // Right half: base 2 with table 1's +5, green saturating at 255.
        assert_eq!(texel(&rgba, 2, row), [5, 255, 141, 255], "row {row} col 2");
        assert_eq!(texel(&rgba, 3, row), [5, 255, 141, 255], "row {row} col 3");
    }
}

/// Differential mode with a positive delta, and `flipbit = 1` splitting the block top/bottom.
///
/// Block `81 00 F8 FF 00 00 00 00`:
/// * byte 3 = `0xFF` -> table1 = 7, table2 = 7, diffbit = 1, flipbit = 1.
/// * red: base 16, delta +1 -> 17. extend5(16) = 132, extend5(17) = 140.
/// * green: base 0, delta 0. extend5(0) = 0.
/// * blue: base 31, delta 0. extend5(31) = 255.
/// * all indices 0 -> table 7's `a`, which is 47.
#[test]
fn differential_mode_adds_a_positive_delta_and_splits_top_and_bottom() {
    let rgba = block([0x81, 0x00, 0xF8, 0xFF, 0x00, 0x00, 0x00, 0x00]);
    for col in 0..4 {
        assert_eq!(texel(&rgba, col, 0), [179, 47, 255, 255], "top, col {col}");
        assert_eq!(texel(&rgba, col, 1), [179, 47, 255, 255], "top, col {col}");
        assert_eq!(
            texel(&rgba, col, 2),
            [187, 47, 255, 255],
            "bottom, col {col}"
        );
        assert_eq!(
            texel(&rgba, col, 3),
            [187, 47, 255, 255],
            "bottom, col {col}"
        );
    }
}

/// Differential mode with a negative delta.
///
/// Block `2E 00 00 02 00 00 00 00`:
/// * byte 0 = `0x2E` -> base 5, delta bits `0b110` = -2 -> 3. extend5(5) = 41, extend5(3) = 24.
/// * byte 3 = `0x02` -> both tables 0, diffbit = 1, flipbit = 0 (left/right).
/// * all indices 0 -> table 0's `a`, which is 2.
#[test]
fn differential_mode_subtracts_a_negative_delta() {
    let rgba = block([0x2E, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00]);
    for row in 0..4 {
        assert_eq!(texel(&rgba, 0, row), [43, 2, 2, 255], "row {row} col 0");
        assert_eq!(texel(&rgba, 1, row), [43, 2, 2, 255], "row {row} col 1");
        assert_eq!(texel(&rgba, 2, row), [26, 2, 2, 255], "row {row} col 2");
        assert_eq!(texel(&rgba, 3, row), [26, 2, 2, 255], "row {row} col 3");
    }
}

// ---------------------------------------------------------------------------------------------
// Pixel index layout -- the transposition bug, pinned one bit at a time
// ---------------------------------------------------------------------------------------------

/// The specification numbers the sixteen pixels **column first** (`a b c d` is the left column),
/// so pixel `i` is at `(x, y) = (i / 4, i % 4)`. A decoder that walks them row-first produces a
/// transposed block, which decodes without error and is simply wrong.
///
/// Block `00 00 00 FC 00 00 00 02`: black bases, both tables 7, individual mode, `flipbit = 0`.
/// The index word `0x00000002` sets the least-significant index bit of pixel 1 and nothing else,
/// so pixel 1 has index 1 (`+b` = 183) and the other fifteen have index 0 (`+a` = 47). Pixel 1 is
/// column 0, row 1.
#[test]
fn one_lsb_bit_lands_on_the_pixel_the_specification_numbers_one() {
    let rgba = block([0x00, 0x00, 0x00, 0xFC, 0x00, 0x00, 0x00, 0x02]);
    for row in 0..4 {
        for col in 0..4 {
            let expected = if (col, row) == (0, 1) {
                [183, 183, 183, 255]
            } else {
                [47, 47, 47, 255]
            };
            assert_eq!(texel(&rgba, col, row), expected, "col {col} row {row}");
        }
    }
}

/// The same pixel through the *other* bit plane. `0x00020000` sets bit 17, which is the
/// most-significant index bit of pixel 1, giving it index 2 (`-a` = -47) and clamping to 0 on a
/// black base. A decoder that swapped the two halves of the word would put 0 at pixel 1 in the
/// previous test and 183 here; a decoder that ignored the high half would leave this uniform.
#[test]
fn one_msb_bit_selects_the_negative_modifier_for_that_same_pixel() {
    let rgba = block([0x00, 0x00, 0x00, 0xFC, 0x00, 0x02, 0x00, 0x00]);
    for row in 0..4 {
        for col in 0..4 {
            let expected = if (col, row) == (0, 1) {
                [0, 0, 0, 255]
            } else {
                [47, 47, 47, 255]
            };
            assert_eq!(texel(&rgba, col, row), expected, "col {col} row {row}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The modifier tables, all 8 x 4 entries
// ---------------------------------------------------------------------------------------------

/// Every entry of specification table 8.15 through specification table 8.16's mapping.
///
/// Table 8.15 gives each table codeword a set `{-b, -a, a, b}`; table 8.16 maps pixel index
/// `00 -> a`, `01 -> b`, `10 -> -a`, `11 -> -b`. The `(a, b)` pairs below are transcribed from
/// 8.15 and the four expected values per table are that mapping applied.
///
/// Each case is decoded from a uniform block whose base is chosen so the result cannot clamp: a
/// black base for the positive modifiers, white for the negative ones. Clamping is tested
/// separately -- a case that saturates would assert 255 or 0 no matter which modifier it used, and
/// would therefore check nothing.
#[test]
fn every_intensity_modifier_reaches_the_pixel_the_mapping_says() {
    const TABLES: [(i32, i32); 8] = [
        (2, 8),
        (5, 17),
        (9, 29),
        (13, 42),
        (18, 60),
        (24, 80),
        (33, 106),
        (47, 183),
    ];

    for (codeword, (a, b)) in TABLES.iter().copied().enumerate() {
        for pixel_index in 0u8..4 {
            let modifier = match pixel_index {
                0 => a,
                1 => b,
                2 => -a,
                _ => -b,
            };
            // Black base for a positive modifier, white for a negative one, so neither clamps.
            let nibbles = if modifier >= 0 { 0x00 } else { 0xFF };
            let base = i32::from(nibbles);
            let codeword = codeword as u8;
            let byte3 = (codeword << 5) | (codeword << 2); // diffbit = 0, flipbit = 0
            let idx = uniform_indices(pixel_index);
            let rgba = block([
                nibbles, nibbles, nibbles, byte3, idx[0], idx[1], idx[2], idx[3],
            ]);
            let expected = u8::try_from(base + modifier).expect("chosen not to clamp");
            for row in 0..4 {
                for col in 0..4 {
                    assert_eq!(
                        texel(&rgba, col, row),
                        [expected, expected, expected, 255],
                        "table {codeword}, pixel index {pixel_index}, col {col} row {row}"
                    );
                }
            }
        }
    }
}

/// The result saturates rather than wrapping. 255 + 183 is 438 and 0 - 183 is -183; a decoder that
/// truncated to `u8` would produce 182 and 73.
///
/// This is asserted in both profiles by `cargo test --workspace --release` and by the mutation
/// harness's debug runs, because release wrapping and debug panicking are different bugs (HANDOFF
/// working agreement 4).
#[test]
fn modifiers_saturate_at_both_ends_instead_of_wrapping() {
    let white = block([0xFF, 0xFF, 0xFF, 0xFC, 0x00, 0x00, 0xFF, 0xFF]); // index 1 -> +183
    let black = block([0x00, 0x00, 0x00, 0xFC, 0xFF, 0xFF, 0xFF, 0xFF]); // index 3 -> -183
    for row in 0..4 {
        for col in 0..4 {
            assert_eq!(texel(&white, col, row), [255, 255, 255, 255]);
            assert_eq!(texel(&black, col, row), [0, 0, 0, 255]);
        }
    }
}

/// `GL_ETC1_RGB8_OES` has no alpha channel, and GL requires a sampler to read 1.0 for an `RGB`
/// base internal format. Every decoded texel is opaque.
#[test]
fn alpha_is_always_opaque() {
    let rgba = block([0x3B, 0x6F, 0xE1, 0x23, 0xFF, 0x40, 0xC4, 0x0F]);
    for texel_index in 0..16 {
        assert_eq!(rgba[texel_index * 4 + 3], 0xFF, "texel {texel_index}");
    }
}

// ---------------------------------------------------------------------------------------------
// Real content, hand-decoded
// ---------------------------------------------------------------------------------------------

/// The first block of `assets/android/textures/sky/sky512_up.tex`, decoded by hand.
///
/// Committed as bytes so this assertion holds on a clone with no APK in it. Block
/// `7F B7 D0 0A 00 00 00 00`:
/// * byte 3 = `0x0A` -> table1 = 0, table2 = 2, diffbit = 1, flipbit = 0 (left/right).
/// * red:   base 15, delta `0b111` = -1 -> 14. extend5(15) = 123, extend5(14) = 115.
/// * green: base 22, delta -1 -> 21.          extend5(22) = 181, extend5(21) = 173.
/// * blue:  base 26, delta 0.                 extend5(26) = 214 for both.
/// * all indices 0 -> `a`: 2 for table 0, 9 for table 2.
#[test]
fn a_real_skybox_block_decodes_to_the_hand_derived_colours() {
    let rgba = block([0x7F, 0xB7, 0xD0, 0x0A, 0x00, 0x00, 0x00, 0x00]);
    for row in 0..4 {
        assert_eq!(
            texel(&rgba, 0, row),
            [125, 183, 216, 255],
            "row {row} col 0"
        );
        assert_eq!(
            texel(&rgba, 1, row),
            [125, 183, 216, 255],
            "row {row} col 1"
        );
        assert_eq!(
            texel(&rgba, 2, row),
            [124, 182, 223, 255],
            "row {row} col 2"
        );
        assert_eq!(
            texel(&rgba, 3, row),
            [124, 182, 223, 255],
            "row {row} col 3"
        );
    }
}

/// The first block of `assets/android/textures/water/normal_01.ktx`, decoded by hand.
///
/// This one uses all four pixel-index values, both modifier tables and the top/bottom split, which
/// is why it is worth the arithmetic. Block `3B 6F E1 23 FF 40 C4 0F`:
/// * byte 3 = `0x23` -> table1 = 1, table2 = 0, diffbit = 1, flipbit = 1 (top/bottom).
/// * red:   base 7,  delta +3 -> 10. extend5(7) = 57,  extend5(10) = 82.
/// * green: base 13, delta -1 -> 12. extend5(13) = 107, extend5(12) = 99.
/// * blue:  base 28, delta +1 -> 29. extend5(28) = 231, extend5(29) = 239.
/// * index word `0xFF40C40F`: low half `0xC40F` is the lsb plane, high half `0xFF40` the msb
///   plane, giving pixel indices (pixel 0 through 15)
///   `1 1 1 1  0 0 2 0  2 2 3 2  2 2 3 3`, and pixel `i` is at column `i / 4`, row `i % 4`.
/// * modifiers: table 1 is (a, b) = (5, 17) for rows 0-1, table 0 is (2, 8) for rows 2-3.
#[test]
fn a_real_water_normal_block_decodes_to_the_hand_derived_colours() {
    let rgba = block([0x3B, 0x6F, 0xE1, 0x23, 0xFF, 0x40, 0xC4, 0x0F]);
    #[rustfmt::skip]
    let expected: [[[u8; 4]; 4]; 4] = [
        // row 0: pixels 0, 4, 8, 12 -> indices 1, 0, 2, 2 over base 1 (57,107,231), table 1
        [[74, 124, 248, 255], [62, 112, 236, 255], [52, 102, 226, 255], [52, 102, 226, 255]],
        // row 1: pixels 1, 5, 9, 13 -> indices 1, 0, 2, 2, same sub-block
        [[74, 124, 248, 255], [62, 112, 236, 255], [52, 102, 226, 255], [52, 102, 226, 255]],
        // row 2: pixels 2, 6, 10, 14 -> indices 1, 2, 3, 3 over base 2 (82,99,239), table 0
        [[90, 107, 247, 255], [80,  97, 237, 255], [74,  91, 231, 255], [74,  91, 231, 255]],
        // row 3: pixels 3, 7, 11, 15 -> indices 1, 0, 2, 3, same sub-block
        [[90, 107, 247, 255], [84, 101, 241, 255], [80,  97, 237, 255], [74,  91, 231, 255]],
    ];
    for (row, expected_row) in expected.iter().enumerate() {
        for (col, expected_texel) in expected_row.iter().enumerate() {
            assert_eq!(
                texel(&rgba, col, row),
                *expected_texel,
                "col {col} row {row}"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The modes that are refused rather than approximated
// ---------------------------------------------------------------------------------------------

/// ETC2's three added modes are selected by an out-of-range base-plus-delta, tested per channel in
/// the order red, green, blue. Each is refused by name; none produces pixels.
#[test]
fn etc2_mode_escapes_are_refused_by_name() {
    // Red base 31 + delta +1 = 32, out of range -> T mode.
    let t = [0xF9, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    // Red in range, green base 31 + 1 -> H mode.
    let h = [0x00, 0xF9, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    // Red and green in range, blue base 31 + 1 -> planar mode.
    let planar = [0x00, 0x00, 0xF9, 0x02, 0x00, 0x00, 0x00, 0x00];
    for (bytes, mode) in [
        (t, Etc2Mode::T),
        (h, Etc2Mode::H),
        (planar, Etc2Mode::Planar),
    ] {
        let mut out = vec![0u8; 64];
        let err = decode(ETC1, &bytes, 4, 4, &mut out).expect_err("must refuse");
        assert_eq!(
            err,
            TextureError::Etc2ModeInEtc1Data {
                block_index: 0,
                mode
            }
        );
        assert!(err.to_string().contains(mode.name()), "{err}");
    }
}

/// The escape is an *underflow* as well as an overflow: base 0 with delta -1 is -1, which is
/// equally out of range and equally T mode. A decoder that only tested the upper bound would
/// silently decode this as a differential block with a wrapped base.
#[test]
fn a_negative_base_plus_delta_is_also_an_etc2_escape() {
    let bytes = [0x07, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    let mut out = vec![0u8; 64];
    assert_eq!(
        decode(ETC1, &bytes, 4, 4, &mut out),
        Err(TextureError::Etc2ModeInEtc1Data {
            block_index: 0,
            mode: Etc2Mode::T
        })
    );
}

/// When more than one channel is out of range the refusal names the mode ETC2 would actually have
/// decoded, which is the first out-of-range channel in red, green, blue order.
#[test]
fn the_first_out_of_range_channel_names_the_mode() {
    let red_and_green = [0xF9, 0xF9, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00];
    let mut out = vec![0u8; 64];
    assert_eq!(
        decode(ETC1, &red_and_green, 4, 4, &mut out),
        Err(TextureError::Etc2ModeInEtc1Data {
            block_index: 0,
            mode: Etc2Mode::T
        })
    );
    let green_and_blue = [0x00, 0xF9, 0xF9, 0x02, 0x00, 0x00, 0x00, 0x00];
    assert_eq!(
        decode(ETC1, &green_and_blue, 4, 4, &mut out),
        Err(TextureError::Etc2ModeInEtc1Data {
            block_index: 0,
            mode: Etc2Mode::H
        })
    );
}

/// Individual mode has no escape: `diffbit = 0` means two 4-bit colours and nothing else, so no
/// byte pattern in that mode can select an ETC2 mode.
///
/// Swept over all 65,536 `(byte0, byte1)` pairs with `byte2 = byte0 ^ byte1`, which reaches every
/// value of every one of the three base-colour bytes. The exhaustive 2^24 version of this, and the
/// matching sweep over differential mode, are in `hostile.rs`.
#[test]
fn individual_mode_never_escapes_whatever_the_base_bytes_are() {
    let mut out = vec![0u8; 64];
    for byte0 in 0u8..=255 {
        for byte1 in 0u8..=255 {
            let bytes = [
                byte0,
                byte1,
                byte0 ^ byte1,
                0x00, // both tables 0, diffbit 0, flipbit 0
                0x00,
                0x00,
                0x00,
                0x00,
            ];
            decode(ETC1, &bytes, 4, 4, &mut out).expect("individual mode always decodes");
        }
    }
}
