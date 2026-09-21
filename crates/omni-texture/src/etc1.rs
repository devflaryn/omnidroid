//! `GL_ETC1_RGB8_OES` decoding, derived from the specification rather than from another decoder.
//!
//! Provenance of every constant and every rule below: `OES_compressed_ETC1_RGB8_texture` and
//! OpenGL ES 3.2 section 8.7.3, "ETC Compressed Texture Image Formats". No second implementation
//! was consulted as an oracle, because that only proves two decoders agree -- this project has
//! already been caught by exactly that, when `Ipv6Addr::Display` was used as the oracle for
//! bionic's `inet_ntop` and disagreed on 43 of 200,000 addresses (D25).
//!
//! # The block
//!
//! Each 4x4 texel block is 8 bytes, most-significant byte first. Bytes 0-3 carry the two base
//! colours and the mode bits; bytes 4-7 carry sixteen 2-bit pixel indices.
//!
//! ```text
//! byte 3:  table1(3) | table2(3) | diffbit(1) | flipbit(1)
//! ```
//!
//! With `diffbit = 0` -- *individual* mode -- bytes 0-2 hold two 4-bit values per channel, the high
//! nibble for sub-block 1 and the low nibble for sub-block 2, each extended to 8 bits by
//! replication (`v << 4 | v`).
//!
//! With `diffbit = 1` -- *differential* mode -- bytes 0-2 hold a 5-bit base for sub-block 1 and a
//! 3-bit two's-complement delta for sub-block 2, each 5-bit result extended by `v << 3 | v >> 2`.
//! **The sum is not allowed to leave `[0, 31]`**: ETC1 forbids an encoder from producing it, and
//! ETC2 reuses those encodings as its T, H and planar modes. This decoder detects that and refuses
//! by name ([`TextureError::Etc2ModeInEtc1Data`]); it never clamps or wraps, because either would
//! emit a plausible wrong colour and no error.
//!
//! # The pixel indices
//!
//! Bytes 4-7 form a 32-bit big-endian word. The low half holds the least-significant bit of each
//! pixel's index and the high half the most-significant bit, so pixel `i` has index
//! `((word >> (i + 16)) & 1) << 1 | ((word >> i) & 1)`. The specification's figure lays the
//! sixteen pixels out **column first**:
//!
//! ```text
//!   a e i m        i = 0 4  8 12
//!   b f j n            1 5  9 13
//!   c g k o            2 6 10 14
//!   d h l p            3 7 11 15
//! ```
//!
//! so `i = x * 4 + y`. Getting this transposed is the classic silent ETC bug -- it decodes without
//! error and the texture is simply wrong -- which is why there are single-bit test vectors for it.
//!
//! # The sub-blocks
//!
//! `flipbit = 0` splits the block into two 2x4 halves side by side (`x < 2` is sub-block 1);
//! `flipbit = 1` splits it into two 4x2 halves stacked (`y < 2` is sub-block 1). Each half uses its
//! own base colour and its own intensity-modifier table.

use crate::error::{Etc2Mode, TextureError};

/// Intensity modifier sets, specification table 8.15, written in the order the specification
/// prints them: ascending, `[-b, -a, a, b]`.
const MODIFIER_SETS: [[i32; 4]; 8] = [
    [-8, -2, 2, 8],
    [-17, -5, 5, 17],
    [-29, -9, 9, 29],
    [-42, -13, 13, 42],
    [-60, -18, 18, 60],
    [-80, -24, 24, 80],
    [-106, -33, 33, 106],
    [-183, -47, 47, 183],
];

/// Specification table 8.16, the mapping from a 2-bit pixel index to an element of the set above.
///
/// The specification states it as `00 -> a`, `01 -> b`, `10 -> -a`, `11 -> -b`; in the ascending
/// `[-b, -a, a, b]` ordering of [`MODIFIER_SETS`] that is elements 2, 3, 1, 0. Kept as an explicit
/// composition rather than folded into a flat `[2, 8, -2, -8]` table so the derivation is visible:
/// the flattened form is what most implementations carry, and it is exactly the sort of constant
/// that gets copied with its order silently wrong.
const PIXEL_INDEX_TO_SET_ELEMENT: [usize; 4] = [2, 3, 1, 0];

/// Texels along one edge of an ETC block.
pub(crate) const BLOCK_EXTENT: u32 = 4;
/// Compressed bytes per ETC block.
pub(crate) const BLOCK_BYTES: usize = 8;
/// Decoded bytes per ETC block: 16 texels of RGBA8.
pub(crate) const BLOCK_RGBA_BYTES: usize = 64;

/// A 3-bit two's-complement delta.
const fn signed3(bits: u8) -> i32 {
    if bits >= 4 {
        bits as i32 - 8
    } else {
        bits as i32
    }
}

/// 4 bits to 8 by replication, the specification's `extend_4to8bits`.
const fn extend4(value: u8) -> u8 {
    (value << 4) | value
}

/// 5 bits to 8 by replication, the specification's `extend_5to8bits`.
const fn extend5(value: u8) -> u8 {
    (value << 3) | (value >> 2)
}

/// `base + modifier`, saturated into `[0, 255]`.
const fn modify(base: u8, modifier: i32) -> u8 {
    let value = base as i32 + modifier;
    if value < 0 {
        0
    } else if value > 255 {
        255
    } else {
        value as u8
    }
}

/// The two base colours a block encodes, already extended to 8 bits per channel.
struct BaseColours {
    sub: [[u8; 3]; 2],
}

/// Read the two base colours, or identify the ETC2 mode the encoding escapes into.
fn base_colours(block: &[u8; BLOCK_BYTES]) -> Result<BaseColours, Etc2Mode> {
    // `block[3] >> 1 & 1` is the diffbit.
    if (block[3] >> 1) & 1 == 0 {
        return Ok(BaseColours {
            sub: [
                [
                    extend4(block[0] >> 4),
                    extend4(block[1] >> 4),
                    extend4(block[2] >> 4),
                ],
                [
                    extend4(block[0] & 0x0F),
                    extend4(block[1] & 0x0F),
                    extend4(block[2] & 0x0F),
                ],
            ],
        });
    }

    // Differential: a 5-bit base and a 3-bit signed delta per channel. The channels are tested in
    // the specification's order -- red selects T, green selects H, blue selects planar -- so a
    // block that is out of range in more than one channel is reported as the first one, which is
    // the mode ETC2 would actually decode it as.
    let mut first = [0u8; 3];
    let mut second = [0u8; 3];
    let modes = [Etc2Mode::T, Etc2Mode::H, Etc2Mode::Planar];
    for channel in 0..3usize {
        let byte = block[channel];
        let base = byte >> 3;
        let sum = base as i32 + signed3(byte & 0x07);
        if !(0..=31).contains(&sum) {
            return Err(modes[channel]);
        }
        first[channel] = extend5(base);
        // `sum` is in [0, 31] here, so the cast cannot truncate.
        second[channel] = extend5(sum as u8);
    }
    Ok(BaseColours {
        sub: [first, second],
    })
}

/// Decode one block to 16 RGBA8 texels in row-major order, alpha 255.
///
/// `block_index` appears only in the refusal, so a caller decoding a whole image can say which
/// block it stopped at.
///
/// # Errors
/// [`TextureError::Etc2ModeInEtc1Data`] when the block escapes into one of ETC2's added modes.
pub fn decode_block(
    block: &[u8; BLOCK_BYTES],
    block_index: usize,
) -> Result<[u8; BLOCK_RGBA_BYTES], TextureError> {
    let colours = base_colours(block)
        .map_err(|mode| TextureError::Etc2ModeInEtc1Data { block_index, mode })?;

    let flip = block[3] & 1 == 1;
    let tables = [
        MODIFIER_SETS[usize::from((block[3] >> 5) & 0x07)],
        MODIFIER_SETS[usize::from((block[3] >> 2) & 0x07)],
    ];

    let indices = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);

    let mut out = [0u8; BLOCK_RGBA_BYTES];
    for y in 0..BLOCK_EXTENT {
        for x in 0..BLOCK_EXTENT {
            // Column-first pixel numbering, specification section 8.7.3.
            let i = x * BLOCK_EXTENT + y;
            let lsb = (indices >> i) & 1;
            let msb = (indices >> (i + 16)) & 1;
            let pixel_index = ((msb << 1) | lsb) as usize;

            let sub = usize::from(if flip { y >= 2 } else { x >= 2 });
            let modifier = tables[sub][PIXEL_INDEX_TO_SET_ELEMENT[pixel_index]];
            let base = colours.sub[sub];

            let at = ((y * BLOCK_EXTENT + x) * 4) as usize;
            out[at] = modify(base[0], modifier);
            out[at + 1] = modify(base[1], modifier);
            out[at + 2] = modify(base[2], modifier);
            out[at + 3] = 0xFF;
        }
    }
    Ok(out)
}
