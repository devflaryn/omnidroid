//! Which compressed formats exist, which one is decoded, and by what name the rest are refused.
//!
//! The census (`tools/texture_census.py`) found exactly one compressed mobile format in the APK:
//! `GL_ETC1_RGB8_OES`, in all 38 KTX1 containers, over 813,802 blocks, with **zero** blocks in any
//! ETC2-only mode. Everything else the APK ships in a block-compressed form is DXT1/DXT3/DXT5 or
//! `DXGI_FORMAT_R8_UNORM`, all of which the host GPU samples natively (`graphics-spike.md` §3).
//!
//! So exactly one format is decoded. The table below is nonetheless complete for ETC2, EAC, ASTC
//! and S3TC, and that is the point: an enum in it is refused **by its specification name**, not as
//! an unknown number. "GL_COMPRESSED_RGBA8_ETC2_EAC is not decoded here" is an instruction; "format
//! 0x9278 is unknown" is a mystery.

use crate::error::TextureError;

/// Every GL compressed internal format this crate can name, whether or not it decodes it.
///
/// Values are from the OpenGL ES 3.2 specification table 8.17 (ETC2 and EAC),
/// `OES_compressed_ETC1_RGB8_texture` (ETC1), `KHR_texture_compression_astc_ldr` (ASTC) and
/// `EXT_texture_compression_s3tc` (S3TC). They are transcribed from the specifications, and
/// `tools/texture_census.py` carries the same table independently -- the census reads the APK with
/// it, so a wrong value there would have produced an unnamed format in its output.
const NAMED_FORMATS: &[(u32, &str)] = &[
    (0x8D64, "GL_ETC1_RGB8_OES"),
    (0x9270, "GL_COMPRESSED_R11_EAC"),
    (0x9271, "GL_COMPRESSED_SIGNED_R11_EAC"),
    (0x9272, "GL_COMPRESSED_RG11_EAC"),
    (0x9273, "GL_COMPRESSED_SIGNED_RG11_EAC"),
    (0x9274, "GL_COMPRESSED_RGB8_ETC2"),
    (0x9275, "GL_COMPRESSED_SRGB8_ETC2"),
    (0x9276, "GL_COMPRESSED_RGB8_PUNCHTHROUGH_ALPHA1_ETC2"),
    (0x9277, "GL_COMPRESSED_SRGB8_PUNCHTHROUGH_ALPHA1_ETC2"),
    (0x9278, "GL_COMPRESSED_RGBA8_ETC2_EAC"),
    (0x9279, "GL_COMPRESSED_SRGB8_ALPHA8_ETC2_EAC"),
    (0x93B0, "GL_COMPRESSED_RGBA_ASTC_4x4_KHR"),
    (0x93B1, "GL_COMPRESSED_RGBA_ASTC_5x4_KHR"),
    (0x93B2, "GL_COMPRESSED_RGBA_ASTC_5x5_KHR"),
    (0x93B3, "GL_COMPRESSED_RGBA_ASTC_6x5_KHR"),
    (0x93B4, "GL_COMPRESSED_RGBA_ASTC_6x6_KHR"),
    (0x93B5, "GL_COMPRESSED_RGBA_ASTC_8x5_KHR"),
    (0x93B6, "GL_COMPRESSED_RGBA_ASTC_8x6_KHR"),
    (0x93B7, "GL_COMPRESSED_RGBA_ASTC_8x8_KHR"),
    (0x93B8, "GL_COMPRESSED_RGBA_ASTC_10x5_KHR"),
    (0x93B9, "GL_COMPRESSED_RGBA_ASTC_10x6_KHR"),
    (0x93BA, "GL_COMPRESSED_RGBA_ASTC_10x8_KHR"),
    (0x93BB, "GL_COMPRESSED_RGBA_ASTC_10x10_KHR"),
    (0x93BC, "GL_COMPRESSED_RGBA_ASTC_12x10_KHR"),
    (0x93BD, "GL_COMPRESSED_RGBA_ASTC_12x12_KHR"),
    (0x93D0, "GL_COMPRESSED_SRGB8_ALPHA8_ASTC_4x4_KHR"),
    (0x93D4, "GL_COMPRESSED_SRGB8_ALPHA8_ASTC_6x6_KHR"),
    (0x93D7, "GL_COMPRESSED_SRGB8_ALPHA8_ASTC_8x8_KHR"),
    (0x83F0, "GL_COMPRESSED_RGB_S3TC_DXT1_EXT"),
    (0x83F1, "GL_COMPRESSED_RGBA_S3TC_DXT1_EXT"),
    (0x83F2, "GL_COMPRESSED_RGBA_S3TC_DXT3_EXT"),
    (0x83F3, "GL_COMPRESSED_RGBA_S3TC_DXT5_EXT"),
];

/// The name the specification gives an internal format, if this crate knows one.
#[must_use]
pub fn gl_internal_format_name(gl_internal_format: u32) -> Option<&'static str> {
    let mut i = 0;
    while i < NAMED_FORMATS.len() {
        // Indexing is bounded by the loop condition; `get` keeps the clippy lint honest.
        if let Some(&(value, name)) = NAMED_FORMATS.get(i) {
            if value == gl_internal_format {
                return Some(name);
            }
        }
        i += 1;
    }
    None
}

/// The compressed formats this crate decodes. There is exactly one, and that is a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CompressedFormat {
    /// `GL_ETC1_RGB8_OES` (`0x8D64`): 4x4 blocks, 8 bytes each, RGB with no alpha channel.
    ///
    /// Decoding to RGBA8 sets alpha to 255, which is what GL's base internal format `GL_RGB`
    /// requires a sampler to see.
    Etc1Rgb8,
}

impl CompressedFormat {
    /// Recognise a guest-supplied `internalformat`, or refuse it by name.
    ///
    /// # Errors
    /// [`TextureError::UnsupportedFormat`], carrying the specification's name whenever the value
    /// has one.
    pub fn from_gl_internal_format(gl_internal_format: u32) -> Result<Self, TextureError> {
        if gl_internal_format == 0x8D64 {
            return Ok(CompressedFormat::Etc1Rgb8);
        }
        Err(TextureError::UnsupportedFormat {
            gl_internal_format,
            name: gl_internal_format_name(gl_internal_format),
        })
    }

    /// The GL enum value.
    #[must_use]
    pub const fn gl_internal_format(self) -> u32 {
        match self {
            CompressedFormat::Etc1Rgb8 => 0x8D64,
        }
    }

    /// The specification's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            CompressedFormat::Etc1Rgb8 => "GL_ETC1_RGB8_OES",
        }
    }

    /// Block footprint in texels, `(width, height)`.
    #[must_use]
    pub const fn block_extent(self) -> (u32, u32) {
        match self {
            CompressedFormat::Etc1Rgb8 => (4, 4),
        }
    }

    /// Compressed bytes per block.
    #[must_use]
    pub const fn block_bytes(self) -> usize {
        match self {
            CompressedFormat::Etc1Rgb8 => 8,
        }
    }
}
