//! Every way a decode can refuse, and nothing that approximates.
//!
//! The rule this module exists to make structural: **a format or block mode that cannot be decoded
//! correctly fails by name and never emits approximate pixels.** A wrong decoder does not crash --
//! it produces a plausible, silently wrong texture, which is the failure mode this project refuses
//! everywhere else. So there is no "unknown block mode, use the average colour" arm anywhere, and
//! no variant here is recoverable into pixels.

use core::fmt;

/// The three block modes ETC2 adds to the ETC1 bit layout by reusing encodings ETC1 forbids.
///
/// They are named individually rather than collapsed into one "ETC2 block" variant because the
/// three are separate pieces of work with separate specifications, and a refusal that says *which*
/// one appeared is the difference between "implement T mode" and "implement ETC2".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Etc2Mode {
    /// Selected when the red base-plus-delta leaves `[0, 31]`. Two base colours and a distance
    /// table; OpenGL ES 3.2 section 8.7.3, "T mode".
    T,
    /// Selected when red is in range and green is not. "H mode", same section.
    H,
    /// Selected when red and green are in range and blue is not. Three colours interpolated
    /// bilinearly across the block; "Planar mode", same section.
    Planar,
}

impl Etc2Mode {
    /// The name to print. Kept as a method so the refusal text and any log agree by construction.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Etc2Mode::T => "ETC2 T mode",
            Etc2Mode::H => "ETC2 H mode",
            Etc2Mode::Planar => "ETC2 planar mode",
        }
    }
}

impl fmt::Display for Etc2Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A refusal. Never a degraded result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextureError {
    /// A compressed format this crate does not decode. `name` is `Some` whenever the value is a
    /// GL enum the specification names, so the refusal reads "GL_COMPRESSED_RGBA8_ETC2_EAC is not
    /// decoded here" rather than "unknown format 0x9278".
    ///
    /// Every ETC2, EAC, ASTC and S3TC enum is in that named set deliberately: the census found
    /// none of them in the APK, so none is implemented, and the one thing worse than not
    /// implementing them would be failing to say so by name when one appears.
    UnsupportedFormat {
        /// The `internalformat` argument as the guest passed it.
        gl_internal_format: u32,
        /// The specification's name for it, when it has one.
        name: Option<&'static str>,
    },

    /// A zero width or height. GL treats a zero-extent compressed image as valid-but-empty; this
    /// crate refuses it instead of returning an empty buffer, because every caller here is about
    /// to allocate and upload, and a silent empty texture is a black one.
    ZeroExtent {
        /// Requested width in texels.
        width: u32,
        /// Requested height in texels.
        height: u32,
    },

    /// The decoded size does not fit in `usize` on this host. Guest-supplied dimensions are
    /// untrusted: `width * height * 4` overflows `u32` above 32,768 x 32,768 and `usize` on a
    /// 32-bit host well before that.
    ExtentTooLarge {
        /// Requested width in texels.
        width: u32,
        /// Requested height in texels.
        height: u32,
    },

    /// Fewer compressed bytes than the block grid needs. A truncated payload is the commonest
    /// hostile input there is, and it must not be padded with anything.
    TruncatedBlockData {
        /// Bytes the block grid requires.
        needed: usize,
        /// Bytes the caller supplied.
        provided: usize,
    },

    /// The destination buffer is smaller than [`decoded_len`](crate::decoded_len).
    OutputTooSmall {
        /// Bytes the decoded image requires.
        needed: usize,
        /// Bytes the caller supplied.
        provided: usize,
    },

    /// A block in an ETC1 payload selects one of ETC2's three added modes.
    ///
    /// This is the refusal the whole crate is shaped around. In 813,802 blocks of real APK content
    /// it never happens (`tools/texture_census.py`), so the modes are not implemented; but the
    /// encodings are reachable from guest-controlled bytes, and an ETC1 decoder that read one as a
    /// differential block would emit a wrong colour and no error at all.
    Etc2ModeInEtc1Data {
        /// Index of the offending block in raster order over the block grid.
        block_index: usize,
        /// Which of the three it is.
        mode: Etc2Mode,
    },
}

impl fmt::Display for TextureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TextureError::UnsupportedFormat {
                gl_internal_format,
                name,
            } => match name {
                Some(name) => write!(
                    f,
                    "{name} ({gl_internal_format:#06x}) is not decoded by omni-texture"
                ),
                None => write!(
                    f,
                    "compressed internal format {gl_internal_format:#06x} is not a format \
                     omni-texture recognises"
                ),
            },
            TextureError::ZeroExtent { width, height } => {
                write!(f, "texture extent {width}x{height} has a zero dimension")
            }
            TextureError::ExtentTooLarge { width, height } => write!(
                f,
                "decoded size of a {width}x{height} RGBA8 image does not fit in usize"
            ),
            TextureError::TruncatedBlockData { needed, provided } => write!(
                f,
                "compressed payload is truncated: {needed} bytes needed, {provided} provided"
            ),
            TextureError::OutputTooSmall { needed, provided } => write!(
                f,
                "destination is too small: {needed} bytes needed, {provided} provided"
            ),
            TextureError::Etc2ModeInEtc1Data { block_index, mode } => write!(
                f,
                "block {block_index} of this GL_ETC1_RGB8_OES payload selects {mode}, which \
                 omni-texture does not decode"
            ),
        }
    }
}

impl core::error::Error for TextureError {}
