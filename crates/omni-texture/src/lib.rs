//! Runtime transcoding of the compressed texture formats the guest uses and the host cannot sample.
//!
//! # Why this crate exists
//!
//! `docs/research/graphics-spike.md` §3 measured the development host's GPU with
//! `vkGetPhysicalDeviceFormatProperties`: **ETC2 and ASTC are not supported for sampled images;
//! BC1, BC3 and BC7 are.** Roblox is an Android application and ships compressed textures in the
//! mobile formats. So every such texture has to be turned into something the host can sample
//! before it is uploaded. That is not an optimisation -- without it there is no correct first
//! frame, only a wrongly-coloured one.
//!
//! # What it decodes, and why that is one format and not twelve
//!
//! *The import list is the specification.* `tools/texture_census.py` applies that principle to
//! textures, classifying every APK entry by its leading bytes and parsing each container down to
//! its format field. Run it with `--check` to reproduce all of this:
//!
//! | finding | value |
//! |---|---|
//! | KTX1 containers in the APK | **38**, every one `GL_ETC1_RGB8_OES` (`0x8D64`) |
//! | ETC2, EAC, ASTC, PVRTC, KTX2 anywhere in the APK | **0** |
//! | DDS containers | 28 -- DXT1/DXT3/DXT5 and `DXGI_FORMAT_R8_UNORM`, all natively sampled |
//! | 4x4 ETC blocks examined | **813,802** |
//! | of those, in ETC2's T, H or planar modes | **0** |
//! | compression vocabulary the engine negotiates streamed assets in | `dxt`, `etc`, `etc2`, `uncompressed` -- `"astc"` does not occur in `libroblox.so` at all |
//!
//! ETC1, then. Not ETC2, not EAC, not ASTC. Every one of those is refused **by its specification
//! name** rather than by number ([`CompressedFormat::from_gl_internal_format`]), and no block mode
//! is ever approximated: a payload that escapes into one of ETC2's three added modes fails with
//! [`TextureError::Etc2ModeInEtc1Data`] naming the mode and the block. "Close enough" colour is
//! precisely the believable wrong answer this project refuses everywhere else.
//!
//! # Why RGBA8 and not a direct transcode to BC1
//!
//! ETC1 and BC1 are both 4x4 blocks in 8 bytes, so an ETC1 -> BC1 transcode would hold the 6:1
//! compression and the 8x VRAM difference is real: the APK's ETC payload is 6,510,416 B compressed
//! and **52,079,224 B** as RGBA8. (Whole blocks would be 52,083,328 B; the 4,104-byte gap is the
//! 2x2 and 1x1 mip level of each of the 38 textures, which still occupies a full 4x4 block. The
//! census reported only the padded figure at first and `tests/real_assets.rs` disagreed with it,
//! which is what a second implementation is for.)
//!
//! It is still the wrong thing to build first, and the reason is testability rather than taste.
//! ETC1 -> RGBA8 is **exact**: the specification defines an integer result for every input, so a
//! known-answer test derived from the specification either passes or finds a bug. ETC1 -> BC1 is
//! an *encode*: two ETC sub-blocks with independent luminance modulation have to be refitted onto
//! BC1's single pair of endpoints, no output is uniquely correct, and the only available oracle
//! would be somebody else's encoder -- which proves agreement, not correctness. This project has
//! already paid for that mistake once (D25). So: an exact decoder now, and if VRAM ever forces a
//! re-encode, it is a second stage behind the same API, verified against this one, with its quality
//! loss measured rather than assumed.
//!
//! # Where this sits, and why it is its own crate
//!
//! The same argument as **D19**, applied here and decided explicitly rather than inherited. D19
//! kept `omni-bionic` separate because zero dependencies make "no OS access" checkable by
//! `cargo tree` instead of by review. Texture transcoding is pure computation over a byte slice --
//! no guest, no boundary, no JNI, no OS -- and putting it inside `omni-gfx` would place it in a
//! crate that transitively links Vulkan and the windowing system, downgrading the guarantee from
//! *impossible* to *against the rules*.
//!
//! This crate goes one step further than D19's: it is `#![no_std]`. Zero dependencies means it
//! cannot reach an OS primitive through a crate; `no_std` means it cannot name one at all, and it
//! allocates nothing -- [`decode`] writes into a caller-supplied buffer. `cargo tree -p
//! omni-texture -e normal` is one line. (`omni-apk` is a dev-dependency, used only by the sweep
//! over the real APK, and dev edges are not normal edges.)
//!
//! # Targets
//!
//! There is no `cfg(target_os)` here, no OS crate, and nothing target-specific: this is integer
//! arithmetic over a slice. Following D22's distinction, that makes it genuinely correct on all
//! five targets in the same sense that `std`'s own arithmetic is -- writing an `unsupported` arm
//! for a non-Windows host would be a false claim in the other direction. What remains unclaimed is
//! what has been **run**, which is Windows x86-64 only; nothing here has been executed on Linux or
//! macOS.
//!
//! # Hostile input
//!
//! Texture data is guest-controlled and the test APK is cheat-injected (D6). Every entry point
//! takes untrusted dimensions and an untrusted payload, and returns a typed refusal for a
//! truncated payload, a zero or overflowing extent, an undersized destination and an ETC2-mode
//! block. Nothing panics, aborts or reads out of bounds.
//!
//! ```
//! use omni_texture::{decode, decoded_len, CompressedFormat};
//!
//! // One opaque-black 4x4 block, individual mode, modifier table 0, all pixel indices 0.
//! let block = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
//! let mut rgba = vec![0u8; decoded_len(4, 4)?];
//! decode(CompressedFormat::Etc1Rgb8, &block, 4, 4, &mut rgba)?;
//! assert_eq!(&rgba[..4], &[2, 2, 2, 255]); // 0 + the table-0 "+a" modifier of 2
//! # Ok::<(), omni_texture::TextureError>(())
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod etc1;
pub mod format;

pub use crate::error::{Etc2Mode, TextureError};
pub use crate::format::{gl_internal_format_name, CompressedFormat};

use crate::etc1::{BLOCK_BYTES, BLOCK_EXTENT};

/// Bytes an RGBA8 image of these dimensions occupies, tightly packed.
///
/// # Errors
/// [`TextureError::ZeroExtent`] for a zero dimension, [`TextureError::ExtentTooLarge`] when the
/// product does not fit in `usize` on this host.
pub fn decoded_len(width: u32, height: u32) -> Result<usize, TextureError> {
    if width == 0 || height == 0 {
        return Err(TextureError::ZeroExtent { width, height });
    }
    // u64 throughout: `width * height * 4` overflows u32 well inside the range GL accepts, and
    // usize is 32 bits on two of the five targets' 32-bit variants. Debug builds would panic on
    // the overflow and release builds would wrap -- HANDOFF's working agreement 4 records exactly
    // that asymmetry hiding a bug in `gmtime`.
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|texels| texels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(TextureError::ExtentTooLarge { width, height })
}

/// Compressed bytes a complete mip level of these dimensions occupies.
///
/// The block grid is `ceil(width / 4) x ceil(height / 4)`: a compressed image whose dimensions are
/// not multiples of the block size still stores whole blocks, and the texels past the edge are
/// discarded on decode.
///
/// # Errors
/// [`TextureError::ZeroExtent`] for a zero dimension, [`TextureError::ExtentTooLarge`] when the
/// product does not fit in `usize`.
pub fn compressed_len(
    format: CompressedFormat,
    width: u32,
    height: u32,
) -> Result<usize, TextureError> {
    let (blocks_x, blocks_y) = block_grid(format, width, height)?;
    u64::from(blocks_x)
        .checked_mul(u64::from(blocks_y))
        .and_then(|blocks| blocks.checked_mul(format.block_bytes() as u64))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .ok_or(TextureError::ExtentTooLarge { width, height })
}

/// `(blocks_x, blocks_y)` for an image of these dimensions.
fn block_grid(
    format: CompressedFormat,
    width: u32,
    height: u32,
) -> Result<(u32, u32), TextureError> {
    if width == 0 || height == 0 {
        return Err(TextureError::ZeroExtent { width, height });
    }
    let (bw, bh) = format.block_extent();
    // `width + bw - 1` would overflow for a width within `bw` of u32::MAX, which is a reachable
    // guest argument. Division first, then a remainder test, cannot.
    let blocks_x = width / bw + u32::from(width % bw != 0);
    let blocks_y = height / bh + u32::from(height % bh != 0);
    Ok((blocks_x, blocks_y))
}

/// Decode a complete compressed mip level into `out` as tightly-packed RGBA8, top row first.
///
/// `out` must be at least [`decoded_len`] bytes; any excess is left untouched. Alpha is 255
/// everywhere, which is what GL requires a sampler to see for an `RGB` base internal format.
///
/// # Errors
/// - [`TextureError::ZeroExtent`] / [`TextureError::ExtentTooLarge`] for dimensions that are empty
///   or do not fit.
/// - [`TextureError::TruncatedBlockData`] when `data` is shorter than [`compressed_len`].
/// - [`TextureError::OutputTooSmall`] when `out` is shorter than [`decoded_len`].
/// - [`TextureError::Etc2ModeInEtc1Data`] when a block escapes into one of ETC2's added modes,
///   naming the mode and the block index. Partial output written before that point is left in
///   `out`; the error is the result, and a caller must not upload a refused image.
pub fn decode(
    format: CompressedFormat,
    data: &[u8],
    width: u32,
    height: u32,
    out: &mut [u8],
) -> Result<(), TextureError> {
    let (blocks_x, blocks_y) = block_grid(format, width, height)?;
    let needed_in = compressed_len(format, width, height)?;
    if data.len() < needed_in {
        return Err(TextureError::TruncatedBlockData {
            needed: needed_in,
            provided: data.len(),
        });
    }
    let needed_out = decoded_len(width, height)?;
    if out.len() < needed_out {
        return Err(TextureError::OutputTooSmall {
            needed: needed_out,
            provided: out.len(),
        });
    }

    let CompressedFormat::Etc1Rgb8 = format;
    let stride = width as usize * 4;

    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let block_index = (by as usize) * (blocks_x as usize) + (bx as usize);
            let at = block_index * BLOCK_BYTES;
            // `data` was length-checked against the whole grid above, so this slice exists.
            let Some(chunk) = data.get(at..at + BLOCK_BYTES) else {
                return Err(TextureError::TruncatedBlockData {
                    needed: needed_in,
                    provided: data.len(),
                });
            };
            let mut block = [0u8; BLOCK_BYTES];
            block.copy_from_slice(chunk);
            let texels = etc1::decode_block(&block, block_index)?;

            for row in 0..BLOCK_EXTENT {
                let y = by * BLOCK_EXTENT + row;
                if y >= height {
                    break;
                }
                for col in 0..BLOCK_EXTENT {
                    let x = bx * BLOCK_EXTENT + col;
                    if x >= width {
                        break;
                    }
                    let src = ((row * BLOCK_EXTENT + col) * 4) as usize;
                    let dst = y as usize * stride + x as usize * 4;
                    // Both ends are inside their buffers: `dst + 4 <= needed_out` because
                    // `y < height` and `x < width`, and `src + 4 <= 64` because row, col < 4.
                    if let (Some(d), Some(s)) =
                        (out.get_mut(dst..dst + 4), texels.get(src..src + 4))
                    {
                        d.copy_from_slice(s);
                    }
                }
            }
        }
    }
    Ok(())
}
