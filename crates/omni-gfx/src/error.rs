//! Typed, diagnostic errors for the renderer.
//!
//! The same discipline as [`VmError`](omni_platform::vm::VmError): every variant names the
//! operation and the values it failed with (Global Constraint 7), and there is no catch-all
//! `Other(String)`.
//!
//! Graphics adds a reason of its own for being strict about this, and the graphics spike is the
//! evidence. This host has **no validation layers** (`docs/research/graphics-spike.md` §6 —
//! `vkEnumerateInstanceLayerProperties` returns five layers and `VK_LAYER_KHRONOS_validation` is
//! not among them), and the spike's own swapchain use-after-free produced **zero** diagnostic
//! output before it hard-crashed the NVIDIA driver. Everything this layer knows about a failure
//! is what it wrote down itself.

use core::fmt;

/// Result alias for every renderer operation.
pub type GfxResult<T> = Result<T, GfxError>;

/// A raw `VkResult`, rendered with its symbolic name when we have one.
///
/// A distinct type rather than a bare `i32`, for the reason
/// [`OsError`](omni_platform::vm::OsError) is one: the number is what can be looked up, and the
/// name is what makes a log line readable without looking it up. The list is the results this
/// renderer can actually produce — a swapchain, a queue submission and four allocations — rather
/// than the whole of `VkResult`, so that a name appearing here means someone considered how this
/// code reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VkError(pub i32);

impl VkError {
    /// The raw `VkResult` value.
    #[must_use]
    pub const fn code(self) -> i32 {
        self.0
    }

    /// The symbolic name, if it is one we have a name for.
    #[must_use]
    pub const fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            5 => "VK_INCOMPLETE",
            1_000_001_003 => "VK_SUBOPTIMAL_KHR",
            -1 => "VK_ERROR_OUT_OF_HOST_MEMORY",
            -2 => "VK_ERROR_OUT_OF_DEVICE_MEMORY",
            -3 => "VK_ERROR_INITIALIZATION_FAILED",
            -4 => "VK_ERROR_DEVICE_LOST",
            -5 => "VK_ERROR_MEMORY_MAP_FAILED",
            -6 => "VK_ERROR_LAYER_NOT_PRESENT",
            -7 => "VK_ERROR_EXTENSION_NOT_PRESENT",
            -8 => "VK_ERROR_FEATURE_NOT_PRESENT",
            -9 => "VK_ERROR_INCOMPATIBLE_DRIVER",
            -11 => "VK_ERROR_FORMAT_NOT_SUPPORTED",
            -1_000_000_000 => "VK_ERROR_SURFACE_LOST_KHR",
            -1_000_000_001 => "VK_ERROR_NATIVE_WINDOW_IN_USE_KHR",
            -1_000_001_004 => "VK_ERROR_OUT_OF_DATE_KHR",
            -1_000_003_001 => "VK_ERROR_INCOMPATIBLE_DISPLAY_KHR",
            -1_000_011_001 => "VK_ERROR_VALIDATION_FAILED_EXT",
            _ => return None,
        })
    }

    /// True when the driver reported that the whole device is gone.
    ///
    /// Worth distinguishing because it is the one failure no amount of retrying helps with: every
    /// object in this renderer is invalid afterwards, and the only correct response is to tear the
    /// renderer down. It is also the shape a driver crash takes when it *does not* take the
    /// process with it, which the spike showed is not guaranteed.
    #[must_use]
    pub const fn is_device_lost(self) -> bool {
        self.0 == -4
    }
}

impl fmt::Display for VkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{} ({name})", self.0),
            None => write!(f, "VkResult {}", self.0),
        }
    }
}

impl std::error::Error for VkError {}

/// Everything that can go wrong in the renderer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GfxError {
    /// The Vulkan loader could not be opened at all.
    ///
    /// This is a **host** problem, not a device problem, and it is reported separately from every
    /// other failure because the action it calls for is different: there is no `vulkan-1.dll`
    /// (or `libvulkan.so.1`, or an ICD for it to find), so no amount of falling back to a
    /// different device or a different format helps. This crate links Vulkan at run time
    /// precisely so that this is a message rather than a failure to start the process.
    #[error("the Vulkan loader could not be opened: {detail}; the host has no usable Vulkan runtime")]
    LoaderMissing {
        /// What the dynamic loader said.
        detail: String,
    },

    /// A Vulkan call failed.
    #[error("`{operation}`: {api} failed with {result}")]
    Vulkan {
        /// The renderer operation that was running, e.g. `"present_clear"`.
        operation: &'static str,
        /// The Vulkan entry point that failed.
        api: &'static str,
        /// What it returned.
        result: VkError,
    },

    /// An instance extension this renderer cannot work without is not present.
    ///
    /// Named rather than counted, because the two this renderer needs fail for completely
    /// different reasons: `VK_KHR_surface` missing means the loader found no ICD at all, and
    /// `VK_KHR_win32_surface` missing means it found one that is not a Windows driver.
    #[error("the Vulkan instance extension `{name}` is not available on this host; {why}")]
    MissingInstanceExtension {
        /// The extension's own name, as Vulkan spells it.
        name: &'static str,
        /// What its absence means, since a reader is unlikely to know.
        why: &'static str,
    },

    /// No physical device can both render and present to this surface.
    ///
    /// Carries how many devices were considered, because "no device" and "four devices, none of
    /// which can present to this window" are different problems with the same symptom.
    #[error(
        "no Vulkan physical device can both render and present to this surface: {considered} \
         device(s) were considered and rejected — {detail}"
    )]
    NoUsableDevice {
        /// How many physical devices the host reported.
        considered: usize,
        /// Why each was rejected, in one line.
        detail: String,
    },

    /// The surface offers no format this renderer can present through.
    ///
    /// The renderer's whole output path is a `vkCmdBlitImage` or a `vkCmdClearColorImage` into the
    /// swapchain image, so a format that is not a valid **blit destination** in optimal tiling is
    /// not usable however well it would serve a shader.
    #[error(
        "none of the {offered} surface format(s) `{device}` offers can be a blit destination in \
         optimal tiling, which this renderer's present path requires"
    )]
    NoUsableSurfaceFormat {
        /// The device's own reported name.
        device: String,
        /// How many formats the surface offered.
        offered: usize,
    },

    /// The swapchain cannot be given the usage the present path needs.
    ///
    /// `vkCmdClearColorImage` and `vkCmdBlitImage` both write the swapchain image as a transfer
    /// destination, which the surface must permit. Presenting through a render pass instead would
    /// not need it — and would need a shader, which this renderer deliberately does not have; see
    /// [`crate::vulkan`].
    #[error("this surface does not support VK_IMAGE_USAGE_TRANSFER_DST_BIT on its swapchain images, which this renderer's present path requires")]
    SurfaceCannotBeTransferDestination,

    /// No memory type satisfies what an allocation needs.
    ///
    /// The host's memory types are measured in `docs/research/graphics-spike.md` §3: five types
    /// across three heaps, of which exactly one is host-visible **and** device-local and it is
    /// only ~214 MiB. So this is a real possibility on a smaller or stranger GPU, not a defensive
    /// branch — and the requirement is named so the message says which of the two allocations
    /// (the host-visible staging buffer or the device-local staging image) could not be placed.
    #[error(
        "no Vulkan memory type satisfies {required} among the types allowed by the resource \
         (allowed mask {type_bits:#010x}); see docs/research/graphics-spike.md §3 for this host's \
         five types"
    )]
    NoUsableMemoryType {
        /// The property flags the allocation required, spelled out.
        required: &'static str,
        /// The `memoryTypeBits` mask the resource permitted.
        type_bits: u32,
    },

    /// The window handle is for a windowing system this renderer has no surface code for.
    ///
    /// [`RawWindow`](omni_platform::window::RawWindow) is `#[non_exhaustive]`, so a Wayland or
    /// AppKit variant added to the seam later compiles here and arrives as this error **naming
    /// itself** rather than as a match that silently stopped being exhaustive.
    #[error(
        "this renderer has no Vulkan surface implementation for the `{system}` windowing system; \
         only win32 (VK_KHR_win32_surface) is implemented"
    )]
    UnsupportedWindowSystem {
        /// The windowing system's short name, from
        /// [`RawWindow::system_name`](omni_platform::window::RawWindow::system_name).
        system: &'static str,
    },

    /// An RGBA8 image's pixel buffer is not the length its dimensions require.
    ///
    /// Rejected here rather than clamped: the buffer is about to be `memcpy`d into mapped device
    /// memory, so a short one reads past its end and a long one silently presents a crop of what
    /// the caller meant. `omni_texture::decoded_len` computes the number this checks against.
    #[error(
        "an RGBA8 image of {width}x{height} needs exactly {expected} bytes and {provided} were \
         provided; omni_texture::decoded_len computes this length"
    )]
    ImageLengthMismatch {
        /// The image's width in texels.
        width: u32,
        /// The image's height in texels.
        height: u32,
        /// The length the dimensions require.
        expected: usize,
        /// The length that was provided.
        provided: usize,
    },

    /// An RGBA8 image has a zero dimension.
    ///
    /// Separate from [`GfxError::ImageLengthMismatch`] because zero is not a short buffer: a
    /// `0 x 0` image with an empty slice is length-consistent and still cannot be blitted, since
    /// `VkImageBlit`'s source region must have a non-zero extent.
    #[error("an RGBA8 image of {width}x{height} has a zero dimension and cannot be presented")]
    ZeroExtentImage {
        /// The image's width in texels.
        width: u32,
        /// The image's height in texels.
        height: u32,
    },
}

impl GfxError {
    /// Build a [`GfxError::Vulkan`] from an `ash` result.
    pub(crate) fn vk(
        operation: &'static str,
        api: &'static str,
    ) -> impl Fn(ash::vk::Result) -> GfxError {
        move |result| GfxError::Vulkan { operation, api, result: VkError(result.as_raw()) }
    }

    /// True when the driver reported the device gone and the renderer must be torn down.
    ///
    /// Exists so that a caller can act on that without matching two levels of enum, and because
    /// it is the one failure where retrying the same call is guaranteed to fail again.
    #[must_use]
    pub fn is_device_lost(&self) -> bool {
        matches!(self, GfxError::Vulkan { result, .. } if result.is_device_lost())
    }
}
