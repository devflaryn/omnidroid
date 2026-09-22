//! The pixels the guest hands over.

use crate::error::{GfxError, GfxResult};

/// Bytes one RGBA8 texel occupies. Fixed by the format, not a tuning knob.
const BYTES_PER_TEXEL: usize = 4;

/// A borrowed, tightly-packed RGBA8 image, top row first.
///
/// # Why this is the renderer's input, and why it is borrowed
///
/// This is the shape `omni_texture` produces. D27 scoped texture transcoding to exactly one
/// format — `GL_ETC1_RGB8_OES`, the only compressed format the APK ships
/// (`tools/texture_census.py`) — decoded to RGBA8, because this host's GPU samples **neither ETC2
/// nor ASTC** (`docs/research/graphics-spike.md` §3, measured across both families). So RGBA8 is
/// not a convenience format chosen here; it is what the decode step downstream of the guest
/// already emits, and `omni_texture::decoded_len` computes the length this type checks.
///
/// Borrowed rather than owned because the frame path must not copy: the pixels go straight into
/// mapped device memory, and an owning type would mean an allocation and a `memcpy` per frame
/// before the one that actually matters.
///
/// # What it is *not*
///
/// Not a texture and not a resource. This is a whole frame to put on the screen, used by
/// [`Renderer::present_rgba8`](crate::vulkan::Renderer::present_rgba8). Uploading the guest's
/// textures is a different path that does not exist yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba8Image<'a> {
    width: u32,
    height: u32,
    pixels: &'a [u8],
}

impl<'a> Rgba8Image<'a> {
    /// Wrap `pixels` as a `width` x `height` RGBA8 image.
    ///
    /// # Errors
    ///
    /// [`GfxError::ZeroExtentImage`] if either dimension is zero — which is *not* a length
    /// problem, since a `0 x 0` image with an empty slice is perfectly length-consistent and still
    /// cannot be the source region of a blit.
    ///
    /// [`GfxError::ImageLengthMismatch`] if `pixels` is not exactly `width * height * 4` bytes.
    /// **Exactly**, in both directions: a short buffer would be read past its end into mapped
    /// device memory, and a long one would silently present a crop of what the caller meant while
    /// looking entirely correct. The product is computed in `u64` because `width * height * 4`
    /// overflows `u32` at dimensions Vulkan and GL both accept, and this host's
    /// `maxImageDimension2D` is 32,768 — so a 32,768-square image is 4 GiB and is *inside* the
    /// range a guest can ask for.
    pub fn new(width: u32, height: u32, pixels: &'a [u8]) -> GfxResult<Self> {
        if width == 0 || height == 0 {
            return Err(GfxError::ZeroExtentImage { width, height });
        }
        let expected = u64::from(width)
            .checked_mul(u64::from(height))
            .and_then(|texels| texels.checked_mul(BYTES_PER_TEXEL as u64))
            .and_then(|bytes| usize::try_from(bytes).ok())
            // An extent whose byte count does not fit in `usize` cannot match any slice's length,
            // so reporting it as a length mismatch against an unreachable `usize::MAX` is both
            // true and the most useful thing to say.
            .unwrap_or(usize::MAX);
        if pixels.len() != expected {
            return Err(GfxError::ImageLengthMismatch {
                width,
                height,
                expected,
                provided: pixels.len(),
            });
        }
        Ok(Rgba8Image { width, height, pixels })
    }

    /// Width in texels. Never zero.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in texels. Never zero.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The pixels, tightly packed, `width * height * 4` bytes, top row first.
    #[must_use]
    pub const fn pixels(&self) -> &'a [u8] {
        self.pixels
    }
}
