//! Every choice the renderer makes, as pure functions over what Vulkan reported.
//!
//! # Why this module exists at all
//!
//! A renderer's real defects are usually *choices*, not calls: the wrong surface format, an image
//! count the driver silently clamps, an extent that ignored `currentExtent`, a memory type that
//! happened to work on the machine it was written on. Every one of those is a function of data the
//! driver hands over, and none of them needs a GPU to decide — so they are separated out here,
//! where a test can feed them the exact tables `docs/research/graphics-spike.md` §3 and §4
//! measured on this host **and** tables no machine anyone has runs, and assert what comes out.
//!
//! That matters more here than it would elsewhere for a measured reason: this host has **no
//! validation layers** (spike §6), and the spike's own swapchain bug was silent until it crashed
//! the driver. A choice that is only ever exercised against one driver on one machine is a choice
//! nobody has checked.
//!
//! Everything here is `pub` for that reason — these are the crate's testable surface, not an
//! implementation detail.

use ash::vk;

/// Which presentation mode to ask for.
///
/// All four were **granted** on this host with none silently falling back
/// (`docs/research/graphics-spike.md` §4), which is not something to rely on elsewhere: only
/// [`PresentMode::Fifo`] is required by the Vulkan specification to exist, so the other three are
/// requests and [`present_mode`] degrades them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PresentMode {
    /// `VK_PRESENT_MODE_FIFO_KHR`. Vertical sync, no tearing, and the only mode every Vulkan
    /// implementation must offer — which is why it is the default and the fallback.
    #[default]
    Fifo,
    /// `VK_PRESENT_MODE_MAILBOX_KHR`. Latest-frame-wins triple buffering: no tearing and no
    /// blocking, at the cost of rendering frames that are never shown.
    Mailbox,
    /// `VK_PRESENT_MODE_IMMEDIATE_KHR`. No synchronisation at all; tears.
    ///
    /// Measured at **0.24 ms mean per frame (n = 300, median 0.15 ms, max 6.6 ms)** on this host
    /// against FIFO's 5.96 ms — but the spike also found the FIFO figure **contaminated**: the
    /// surface extent drifted across 41 spontaneous swapchain recreations during that run with
    /// the window untouched, apparently a Parsec virtual display renegotiating. The honest
    /// reading is that MAILBOX/IMMEDIATE measure sub-millisecond submission overhead and the FIFO
    /// number on this machine says nothing about frame pacing.
    Immediate,
}

impl PresentMode {
    /// Every variant, in order. Exists so that invariants can be asserted over all of them.
    pub const ALL: [PresentMode; 3] =
        [PresentMode::Fifo, PresentMode::Mailbox, PresentMode::Immediate];

    /// The Vulkan enumerator this asks for.
    #[must_use]
    pub const fn to_vk(self) -> vk::PresentModeKHR {
        match self {
            PresentMode::Fifo => vk::PresentModeKHR::FIFO,
            PresentMode::Mailbox => vk::PresentModeKHR::MAILBOX,
            PresentMode::Immediate => vk::PresentModeKHR::IMMEDIATE,
        }
    }
}

/// The present mode to create the swapchain with: `requested` if the surface offers it, else FIFO.
///
/// FIFO rather than "the next best thing" is deliberate. The specification guarantees FIFO is
/// present, so the fallback can never itself fail — and a silent downgrade from MAILBOX to
/// IMMEDIATE would swap a tear-free mode for a tearing one, which is a *visible* change nobody
/// asked for. Downgrading to FIFO changes latency, which is not.
#[must_use]
pub fn present_mode(requested: PresentMode, available: &[vk::PresentModeKHR]) -> vk::PresentModeKHR {
    let wanted = requested.to_vk();
    if available.contains(&wanted) { wanted } else { vk::PresentModeKHR::FIFO }
}

/// The swapchain format to use, or `None` if the surface offers nothing usable.
///
/// # Why `UNORM` is preferred over `SRGB`, which is the opposite of the usual advice
///
/// This renderer does not shade. Its two present paths are `vkCmdClearColorImage` with a colour
/// the caller chose and `vkCmdBlitImage` from an RGBA8 image the caller handed over — and for
/// D27's path those pixels are `omni_texture`'s ETC1 decode output, which is already
/// sRGB-encoded, exactly as it was in the APK. Blitting them into an `*_SRGB` swapchain image
/// makes the driver apply an sRGB **encode** to values that are already encoded, which washes the
/// image out. A `*_UNORM` swapchain stores what it is given.
///
/// The usual advice — prefer `SRGB` — is right for a renderer whose shaders work in linear space
/// and want the hardware to do the final encode. When D8's real forwarding path arrives and the
/// guest's own pipeline is driving, that choice belongs to it, and this function is where it
/// changes.
///
/// `candidates` must be the subset of the surface's formats that are valid blit destinations in
/// optimal tiling; this function does not query, so the caller does the filtering and this stays
/// pure. The colour space is not a preference but a **requirement**: `SRGB_NONLINEAR` is the only
/// one every implementation offers, and this host also offers HDR10 and extended-linear variants
/// (spike §4, 7 formats offered) which would change the meaning of every pixel written.
#[must_use]
pub fn surface_format(candidates: &[vk::SurfaceFormatKHR]) -> Option<vk::SurfaceFormatKHR> {
    let usable = |f: &&vk::SurfaceFormatKHR| f.color_space == vk::ColorSpaceKHR::SRGB_NONLINEAR;
    // In preference order. BGRA first because it is what this host's surface reports first and
    // what Windows compositors natively want; the RGBA spellings are for drivers that offer only
    // those.
    for preferred in
        [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM, vk::Format::A8B8G8R8_UNORM_PACK32]
    {
        if let Some(found) = candidates.iter().filter(usable).find(|f| f.format == preferred) {
            return Some(*found);
        }
    }
    // Nothing preferred. Take the first `SRGB_NONLINEAR` format rather than failing: an unusual
    // format still presents, where refusing presents nothing. The colour may be wrong; a black
    // window is not better.
    candidates.iter().filter(usable).copied().next()
}

/// How many images to ask the swapchain for.
///
/// `minImageCount + 1`, so that the application always has an image to render into while the
/// presentation engine holds one — asking for exactly the minimum means every acquire waits on the
/// presentation engine. Clamped to `maxImageCount` when the surface declares one; **zero means no
/// limit**, which is the trap in this call: clamping to a literal `maxImageCount` of 0 asks for a
/// swapchain with no images.
///
/// This host reports `minImageCount`/`maxImageCount` = 2/8 (`docs/research/graphics-spike.md` §4),
/// so it takes 3.
#[must_use]
pub fn image_count(caps: &vk::SurfaceCapabilitiesKHR) -> u32 {
    let wanted = caps.min_image_count.saturating_add(1);
    if caps.max_image_count == 0 { wanted } else { wanted.min(caps.max_image_count) }
}

/// The extent to create the swapchain with.
///
/// `currentExtent` is authoritative whenever the surface gives one, and on Win32 it always does —
/// it *is* the window's client area, and a swapchain created at any other size is immediately
/// out of date. The `0xFFFFFFFF` sentinel means "you choose", which some Wayland compositors
/// report, and only then is `desired` used, clamped into the surface's own range.
///
/// Returns a zero extent unchanged rather than clamping it up to the minimum, because zero is how
/// a minimised window is reported and the caller must skip the frame rather than build a
/// swapchain it cannot present. The spike measured how little this can be trusted to stay still:
/// the surface's `currentExtent` drifted from 1024 to 1173 pixels across 41 spontaneous
/// recreations in five seconds **with the window untouched** (§4).
// No `#[must_use]`: `vk::Extent2D` already carries one, and clippy's `double_must_use` is right
// that repeating it says nothing.
pub fn clamp_extent(desired: (u32, u32), caps: &vk::SurfaceCapabilitiesKHR) -> vk::Extent2D {
    const LET_THE_APPLICATION_CHOOSE: u32 = u32::MAX;
    if caps.current_extent.width != LET_THE_APPLICATION_CHOOSE {
        return caps.current_extent;
    }
    if desired.0 == 0 || desired.1 == 0 {
        return vk::Extent2D { width: 0, height: 0 };
    }
    vk::Extent2D {
        width: desired.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
        height: desired.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
    }
}

/// The index of a memory type that is allowed by `type_bits` and has every flag in `required`.
///
/// `type_bits` is the resource's own `memoryTypeBits`: bit *i* set means type *i* may back it.
/// The search takes the **first** match, which is the ordering Vulkan guarantees is
/// most-preferred-first — types earlier in the array are not slower than later ones with the same
/// properties.
///
/// # The reason this is worth a unit test rather than a glance
///
/// This host's five memory types (`docs/research/graphics-spike.md` §3) include one that is
/// `DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT` — a ReBAR window — and it is **only ~214 MiB**,
/// against a 7.77 GiB device-local heap. So a search for `HOST_VISIBLE | HOST_COHERENT` and a
/// search for `DEVICE_LOCAL` must land on *different* types here, and an implementation that
/// accidentally preferred the ReBAR type for bulk uploads would work perfectly until an upload
/// exceeded 214 MiB. A GPU with no such type at all and a GPU where every type is host-visible
/// (an integrated one) are the other two shapes, and neither exists on this machine to test
/// against.
#[must_use]
pub fn memory_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
    required: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&index| {
        let allowed = type_bits & (1 << index) != 0;
        allowed && props.memory_types[index as usize].property_flags.contains(required)
    })
}

/// How good a physical device is, higher being better.
///
/// # D8's constraint is what fixes this order
///
/// `libroblox.so` contains the string `Vulkan: Device %s is emulated, skipping`, alongside vendor
/// and driver blacklists: **Roblox refuses emulated Vulkan devices**. Omnidroid forwards to a real
/// host driver, so the `deviceType` it reports is genuine — and this ranking is what keeps it
/// that way, by preferring a discrete GPU over the software implementations
/// (`VK_PHYSICAL_DEVICE_TYPE_CPU`, and `VIRTUAL_GPU` for a paravirtualised one) that a host may
/// also enumerate. A renderer that took `devices[0]` would pick whichever the loader listed first,
/// which on a machine with lavapipe installed is a coin toss.
///
/// `OTHER` ranks above `CPU` because it means the driver declined to classify itself, which is not
/// the same claim as "this is a software rasteriser".
#[must_use]
pub fn device_rank(device_type: vk::PhysicalDeviceType) -> u32 {
    match device_type {
        vk::PhysicalDeviceType::DISCRETE_GPU => 4,
        vk::PhysicalDeviceType::INTEGRATED_GPU => 3,
        vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
        vk::PhysicalDeviceType::CPU => 0,
        _ => 1,
    }
}

/// The filter to blit the guest's image with, given the source format's optimal-tiling features.
///
/// `VK_FILTER_LINEAR` needs `SAMPLED_IMAGE_FILTER_LINEAR` on the **source** format, which
/// `R8G8B8A8_UNORM` is required by the specification to have — so on any conformant driver this
/// returns `LINEAR`. It is still a function of what was reported rather than a constant, because
/// the alternative is `vkCmdBlitImage` with an invalid filter, which is undefined behaviour and
/// which, on a machine with no validation layers, is undefined behaviour nothing would report.
///
/// `NEAREST` is always legal for a blit, so the fallback cannot itself fail.
#[must_use]
pub fn blit_filter(source_features: vk::FormatFeatureFlags) -> vk::Filter {
    if source_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR) {
        vk::Filter::LINEAR
    } else {
        vk::Filter::NEAREST
    }
}
