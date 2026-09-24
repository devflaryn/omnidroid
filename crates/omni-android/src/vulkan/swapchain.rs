//! **`VK_KHR_swapchain`: the swapchain, its images, and the two calls that tell the guest its
//! window changed size.**
//!
//! # What stage 4 is, in one sentence
//!
//! Stage 3 ended with a `VkDevice` and a `VkQueue` and nothing to do with them. This module is the
//! first half of the answer: a swapchain over the surface stage 3 created, the images that come
//! out of it, and `vkAcquireNextImageKHR` — after which [`command`](super::command) records a
//! clear into one of those images and [`queue`](super::queue) submits it and presents it.
//!
//! # Who owns the surface, which is the question this file exists to answer clearly
//!
//! There are **two** Vulkan stacks pointed at one window in this process.
//!
//! * `omni_gfx::Renderer` has an instance, a device, a surface and a swapchain of its own. It is
//!   the host-side renderer D8 built first, and `tests/ndk_host_window.rs` drives real frames
//!   through it.
//! * The **guest** has an instance, a device and a surface of its own, created through this layer
//!   over the same `HWND` — because `ndk::HostWindowSource` publishes one window and
//!   `vkCreateAndroidSurfaceKHR` resolves the guest's `ANativeWindow *` to it.
//!
//! Two `VkSurfaceKHR` objects over one window is legal and is exactly what those two stacks have.
//! Two **swapchains** over one window is not: the specification permits a native window to be
//! associated with at most one swapchain at a time, and a driver that notices reports
//! `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`. This host has no validation layers
//! (`docs/research/graphics-spike.md` §6) and an NVIDIA driver that does not always notice — the
//! spike's own swapchain misuse produced **zero** diagnostic output and crashed the driver
//! instead.
//!
//! So the rule is stated by the one participant that can see both stacks, which is `omni-gfx`:
//! `omni_gfx::claim` is a registry of window ownership that `Renderer::new` takes and that
//! `GfxVulkanHost::create_swapchain` takes, and a conflict is an
//! [`AbiError::Refused`](crate::AbiError::Refused) **naming the other owner** before any driver is
//! asked. It is not a `VkResult`, and that is deliberate: `VK_ERROR_NATIVE_WINDOW_IN_USE_KHR`
//! would send the engine looking at its own surface, and the real fault is that an embedding
//! started a host renderer on a window it then handed to the guest.
//!
//! **The answer, stated plainly: the guest owns the surface it created, and `omni-gfx`'s renderer
//! owns the surface it created, and whichever of them creates a swapchain first owns the
//! *window*.** An embedding that wants the guest to render must not also run a `Renderer` on that
//! window.
//!
//! # `oldSwapchain` is part of that question and not a separate one
//!
//! `VkSwapchainCreateInfoKHR::oldSwapchain` is how a renderer replaces a swapchain whose surface
//! has changed size, and for the moment of the call **two swapchains exist over one window** —
//! which is precisely the state the paragraph above forbids. The specification allows it because
//! the outgoing one is *retired* by the call: it can no longer present, and the driver may reuse
//! its images.
//!
//! So a non-null `oldSwapchain` does not take a fresh claim; it **transfers** the one the outgoing
//! swapchain already holds. That makes recreation work and keeps the conflict rule intact, and it
//! makes the third case — a guest passing an `oldSwapchain` that belongs to a *different* surface
//! — a refusal naming both, rather than a claim quietly taken from somewhere else.
//!
//! The one thing this layer does **not** do is destroy the old swapchain. The specification is
//! explicit that `oldSwapchain` is retired rather than destroyed and that the guest must still
//! call `vkDestroySwapchainKHR` on it; the graphics spike measured the cost of getting the
//! ordering wrong in the other direction — destroying before passing it here crashed the NVIDIA
//! driver on the first live resize, every time (`omni_gfx::vulkan`'s module header).
//!
//! # Resize: `VK_ERROR_OUT_OF_DATE_KHR` and `VK_SUBOPTIMAL_KHR` are forwarded verbatim
//!
//! `ndk::HostWindowSource` publishes the real window's size and the guest reads it through
//! `ANativeWindow_getWidth`, so a resized window is something the guest can already see. What it
//! must **also** see is the driver's own opinion, because the two arrive at different times: the
//! surface stops matching the swapchain the instant the window moves, and a frame already in
//! flight is what discovers it.
//!
//! Both codes therefore travel from the driver into the guest's `X0` without being examined:
//!
//! | code | what it means | what this layer does |
//! |---|---|---|
//! | `VK_SUCCESS` (0) | an image was acquired, or the frame was presented | forwards it |
//! | [`VK_SUBOPTIMAL_KHR`] (1000001003) | it worked, **and** the swapchain no longer matches the surface | at **present**: forwards it. At **acquire**: answers `VK_SUCCESS` and writes `pImageIndex` (below) |
//! | [`VK_ERROR_OUT_OF_DATE_KHR`] (-1000001004) | the swapchain cannot be used at all | forwards it, and writes nothing |
//! | [`VK_TIMEOUT`] (2), [`VK_NOT_READY`] (1) | no image yet, and this is not an error | forwards it, and writes nothing |
//!
//! **Except `VK_SUBOPTIMAL_KHR` from acquire, which the guest is told is `VK_SUCCESS`**, because that
//! is what the guest's own platform answers: Android's swapchain never returns it from
//! `vkAcquireNextImageKHR` -- "Android will only return VK_SUBOPTIMAL_KHR for vkQueuePresentKHR, and
//! only when the window's transform/rotation changes. Extent changes will not cause
//! VK_SUBOPTIMAL_KHR" (AOSP `frameworks/native/vulkan/libvulkan/swapchain.cpp`, `QueuePresentKHR`,
//! main, read 2026-09-24); the window scales the buffers (`NATIVE_WINDOW_SCALING_MODE_SCALE_TO_WINDOW`)
//! and the app learns of the new size from `APP_CMD_WINDOW_RESIZED`. MEASURED on MoltenVK: the first
//! acquire after a window resize answers `VK_SUBOPTIMAL_KHR` (NVIDIA on Windows answers `VK_SUCCESS`
//! there), the engine logs it as a `VULKAN ERROR` and records its next barrier on image handle 0,
//! and the refusal of that handle killed the render thread. The image the driver acquired is valid
//! either way, and the driver's own code is still recorded in `Vulkan::driver_failures()`.
//!
//! **This layer does not recreate the swapchain on the guest's behalf**, and that is the single
//! most important sentence in this file. The engine owns its swapchain: it chose the format, the
//! image count, the present mode and the extent, and it has branches for both codes. A layer that
//! swallowed `VK_ERROR_OUT_OF_DATE_KHR` and quietly built a new swapchain would hand the guest a
//! different swapchain from the one its `VkImageView`s were built over, and the first symptom
//! would be a frame presented from a view of an image that no longer exists.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::counted;
use super::host::{DriverAnswer, SwapchainRequest};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_NOT_READY` — a **success** code. No image was available and none was waited for.
pub const VK_NOT_READY: i32 = 1;

/// `VK_TIMEOUT` — a **success** code. The timeout expired before anything became available.
///
/// Spelled out as a success rather than left to a reader's memory, because the mistake it guards
/// against is a shim that treats "not zero" as "failed": a frame loop told `VK_SUCCESS` for a
/// timed-out acquire would go on to record into an image index nothing wrote.
pub const VK_TIMEOUT: i32 = 2;

/// `VK_SUBOPTIMAL_KHR` — a **success** code from `VK_KHR_swapchain`.
///
/// `1000001000 + 3`, the extension's own base: `VK_KHR_swapchain` is extension number 2, so its
/// codes start at `1000000000 + (2 - 1) * 1000`. The frame worked; the swapchain no longer matches
/// the surface's properties exactly. An engine that ignores it presents slightly stretched frames
/// forever, which is a thing the engine gets to decide.
pub const VK_SUBOPTIMAL_KHR: i32 = 1_000_001_003;

/// `VK_ERROR_OUT_OF_DATE_KHR` — a failure code. The swapchain cannot be used for presentation.
pub const VK_ERROR_OUT_OF_DATE_KHR: i32 = -1_000_001_004;

/// `VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR`, which is that same extension base.
pub const STYPE_SWAPCHAIN_CREATE_INFO_KHR: u32 = 1_000_001_000;

/// `sizeof(VkSwapchainCreateInfoKHR)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType                  sType;                   //   0  (then 4 of padding)
/// const void                      *pNext;                   //   8
/// VkSwapchainCreateFlagsKHR        flags;                   //  16  (then 4 of padding)
/// VkSurfaceKHR                     surface;                 //  24  (a uint64_t, 8-aligned)
/// uint32_t                         minImageCount;           //  32
/// VkFormat                         imageFormat;             //  36
/// VkColorSpaceKHR                  imageColorSpace;         //  40
/// VkExtent2D                       imageExtent;             //  44  (width 44, height 48)
/// uint32_t                         imageArrayLayers;        //  52
/// VkImageUsageFlags                imageUsage;              //  56
/// VkSharingMode                    imageSharingMode;        //  60
/// uint32_t                         queueFamilyIndexCount;   //  64  (then 4 of padding)
/// const uint32_t                  *pQueueFamilyIndices;     //  72
/// VkSurfaceTransformFlagBitsKHR    preTransform;            //  80
/// VkCompositeAlphaFlagBitsKHR      compositeAlpha;          //  84
/// VkPresentModeKHR                 presentMode;             //  88
/// VkBool32                         clipped;                 //  92
/// VkSwapchainKHR                   oldSwapchain;            //  96
/// ```
///
/// **The offset a reader is most likely to get wrong is `surface` at 24, not 20.** A
/// non-dispatchable handle is a `uint64_t` on every platform — the `VK_DEFINE_NON_DISPATCHABLE_HANDLE`
/// macro is fixed at 64 bits precisely so structures like this one have the same layout on 32-bit
/// and 64-bit targets — so `flags` at 16 is followed by four bytes of padding. A layout that put
/// `surface` at 20 would read `minImageCount` and the format out of the surface handle's two
/// halves, and every field after it would be wrong by four bytes.
pub const SWAPCHAIN_CREATE_INFO_BYTES: usize = 104;

/// `sizeof(VkImage)`: a non-dispatchable handle, which is a `uint64_t` on every target.
///
/// The same eight bytes a `VkSwapchainKHR`, a `VkSemaphore` and a `VkFence` occupy, and what the
/// guest receives in each case is an address in this boundary's data area rather than the driver's
/// value. Stated as its own constant because it is what `vkGetSwapchainImagesKHR`'s two-call
/// protocol multiplies the count by.
pub const NON_DISPATCHABLE_HANDLE_BYTES: usize = 8;

/// How many entries of `pQueueFamilyIndices` one `vkCreateSwapchainKHR` may name.
///
/// An allocation bound for [`MAX_ENABLED_NAMES`](super::MAX_ENABLED_NAMES)' reason:
/// `queueFamilyIndexCount` is a guest `uint32_t` indexing an array of `uint32_t`, so honouring it
/// unbounded is a guest-controlled host allocation of up to 16 GB. Thirty-two is above the number
/// of queue families any current driver reports — this host's NVIDIA driver reports six — so
/// reaching it means something nobody has seen, which the refusal names.
pub const MAX_SWAPCHAIN_QUEUE_FAMILIES: usize = 32;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateSwapchainKHR(VkDevice device, const VkSwapchainCreateInfoKHR *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkSwapchainKHR *pSwapchain)`
///
/// The shape [`device::create_device`](super::device) has, with the window-ownership question this
/// module's header answers layered on top of it. The order of the checks is the same one every
/// forwarded call in this crate uses: the allocator is observed first, because it is a measurement
/// a later refusal would lose.
pub(super) fn create_swapchain(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateSwapchainKHR";
    refuse_allocator(vulkan, at, CALL, args[2])?;

    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let create_info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let swapchain_at = require_pointer(at, CALL, "pSwapchain", args[3])?;

    let request = decode_create_info(c, at, vulkan, create_info_at)?;
    match host.create_swapchain(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_swapchain(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(swapchain_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// Decode `VkSwapchainCreateInfoKHR` out of guest memory.
///
/// Both handles in it — `surface` and `oldSwapchain` — go through this layer's registries, so what
/// reaches the host is a pair of tokens and never a number the guest chose.
fn decode_create_info(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    create_info_at: omni_mem::GuestAddr,
) -> AbiResult<SwapchainRequest> {
    const CALL: &str = "vkCreateSwapchainKHR";
    let info = c.mem().read_bytes(create_info_at, SWAPCHAIN_CREATE_INFO_BYTES, c.blame(1))?;
    let u32_at =
        |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let u64_at =
        |offset: usize| u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight"));

    let stype = u32_at(0);
    if stype != STYPE_SWAPCHAIN_CREATE_INFO_KHR {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pCreateInfo` whose `sType` is \
             {stype}, and `VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR` is \
             {STYPE_SWAPCHAIN_CREATE_INFO_KHR}. Every field after it would be read at an offset \
             belonging to a different structure -- `surface` would be whatever sits at byte 24 of \
             something else, and this layer would then look that up in its surface registry",
            caller = at.caller
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pCreateInfo->pNext = {next:#x}`. A \
             swapchain `pNext` chain is where the things that change what the swapchain *is* \
             live -- `VkSwapchainCounterCreateInfoEXT`, \
             `VkImageFormatListCreateInfo`, `VkSurfaceFullScreenExclusiveInfoEXT`, and on Android \
             `VkSwapchainPresentBarrierCreateInfoNV` -- so dropping the chain would create a \
             different swapchain from the one that was asked for and every consequence would \
             arrive later as a frame that looks wrong. This layer does not know those layouts, so \
             it refuses and names the address for the next run to decode",
            caller = at.caller
        )));
    }

    // **Both handles through the registries.** A `VkSurfaceKHR` is non-dispatchable, so a forged
    // one would not crash: it would name some other surface, and the guest's frames would go into
    // a window nobody chose. `surface` is required; `oldSwapchain` is the one that may be NULL.
    let surface = vulkan.surface_token(at, CALL, u64_at(24))?;
    let old_swapchain = match u64_at(96) {
        0 => None,
        handle => Some(vulkan.swapchain_token(at, CALL, handle)?),
    };

    let family_count = u32_at(64);
    let queue_families = decode_queue_families(c, at, family_count, u64_at(72))?;

    Ok(SwapchainRequest {
        flags: u32_at(16),
        surface: Some(surface),
        min_image_count: u32_at(32),
        format: u32_at(36),
        colour_space: u32_at(40),
        width: u32_at(44),
        height: u32_at(48),
        array_layers: u32_at(52),
        usage: u32_at(56),
        sharing_mode: u32_at(60),
        queue_families,
        pre_transform: u32_at(80),
        composite_alpha: u32_at(84),
        present_mode: u32_at(88),
        clipped: u32_at(92),
        old_swapchain,
    })
}

/// Decode `pQueueFamilyIndices`, which is meaningful only for `VK_SHARING_MODE_CONCURRENT`.
///
/// The array is read whenever the **count** is non-zero rather than whenever the sharing mode is
/// concurrent, and that is deliberate: the specification says the array is ignored for
/// `VK_SHARING_MODE_EXCLUSIVE`, but a guest that wrote a count and a pointer meant something by
/// them, and a layer that read one field to decide whether to read another would be re-deriving a
/// rule the driver already owns. Forwarding both is what keeps this a trampoline.
fn decode_queue_families(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    count: u32,
    array: u64,
) -> AbiResult<Vec<u32>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = count as usize;
    if count > MAX_SWAPCHAIN_QUEUE_FAMILIES {
        return Err(at.refuse(format!(
            "the guest called `vkCreateSwapchainKHR` from {caller:#x} with \
             `queueFamilyIndexCount = {count}`, and this layer reads at most \
             {MAX_SWAPCHAIN_QUEUE_FAMILIES}. The count is a guest `uint32_t` indexing an array of \
             `uint32_t`, so honouring it unbounded would be a guest-controlled host allocation \
             (Global Constraint 11). It is also far more queue families than any driver reports: \
             this host's NVIDIA driver has six",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pQueueFamilyIndices", array)?;
    if array_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `vkCreateSwapchainKHR` from {caller:#x} with \
             `pQueueFamilyIndices = NULL` and `queueFamilyIndexCount = {count}`. Creating a \
             swapchain shared between no families while the engine believes it named {count} is \
             the silent divergence this layer exists to refuse -- and for a `CONCURRENT` \
             swapchain it is the difference between images the present queue may read and images \
             it may not",
            caller = at.caller
        )));
    }
    let bytes = c.mem().read_bytes(array_at, count * 4, c.blame(1))?;
    Ok((0..count)
        .map(|index| u32::from_le_bytes(bytes[index * 4..index * 4 + 4].try_into().expect("four")))
        .collect())
}

/// `VkResult vkGetSwapchainImagesKHR(VkDevice device, VkSwapchainKHR swapchain,
/// uint32_t *pSwapchainImageCount, VkImage *pSwapchainImages)`
///
/// # Every image is registered on the first call, including the count-only one
///
/// [`physical::enumerate_physical_devices`](super::physical) makes this argument in full and it is
/// the same one here with a sharper edge: the specification requires the second call to produce
/// the same `VkImage` handles as the first, *and* the index into that array is the `imageIndex`
/// `vkAcquireNextImageKHR` answers with. A registry that issued a fresh handle per call would give
/// the guest two handles for one image, it would build its `VkImageView` from one of them and its
/// barrier from the other, and the two would name the same driver image — so it would work, right
/// up until something compared them.
pub(super) fn get_swapchain_images(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkGetSwapchainImagesKHR";
    let host = vulkan.require_host(at)?;
    // `device` is validated and then deliberately not forwarded: the host finds the device from
    // the swapchain's own record, and validating the guest's handle is what makes a `VkDevice`
    // from another instance a typed refusal rather than an argument nothing looked at.
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let swapchain = vulkan.swapchain_token(at, CALL, args[1])?;

    let tokens = match host.swapchain_images(swapchain)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(tokens) => tokens,
    };

    let mut handles = Vec::with_capacity(tokens.len() * NON_DISPATCHABLE_HANDLE_BYTES);
    for token in &tokens {
        let registered = vulkan.register_image(at, *token)?;
        handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
        if registered.fresh {
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
        }
    }

    let filled = counted::enumerate(
        c,
        at,
        &counted::Array {
            call: CALL,
            element: "VkImage",
            element_bytes: NON_DISPATCHABLE_HANDLE_BYTES,
            count_pointer: args[2],
            array_pointer: args[3],
            count_argument: 2,
            array_argument: 3,
        },
        &handles,
    )?;
    c.ret().i32(filled.result());
    Ok(())
}

/// `void vkDestroySwapchainKHR(VkDevice device, VkSwapchainKHR swapchain,
/// const VkAllocationCallbacks *pAllocator)`
///
/// # `VK_NULL_HANDLE` is a no-op, and that is the specification's rule rather than a kindness
///
/// Every `vkDestroy*` in Vulkan accepts `VK_NULL_HANDLE` and does nothing, and teardown code
/// relies on it: a renderer that failed halfway through construction destroys everything it might
/// have made, and most of those handles are null. Refusing here would turn a conforming cleanup
/// path into a crash at exactly the moment something had already gone wrong.
///
/// # The images go with it
///
/// A swapchain image's lifetime is its swapchain's. After this call the driver's `VkImage` values
/// name nothing, so the guest handles for them are dropped in the same breath —
/// [`Vulkan::forget_images_of`](super::Vulkan) carries the argument. The guest never asked for
/// that and could not have: there is no `vkDestroyImage` for a swapchain image, and a guest that
/// called one would be destroying an object it does not own.
pub(super) fn destroy_swapchain(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroySwapchainKHR";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;

    if args[1] == 0 {
        // The specified no-op. See this function's documentation.
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "swapchain", args[1])?;
    let swapchain = vulkan.swapchain_token(at, CALL, args[1])?;

    // **The images' handles are taken back before the driver is asked**, so that there is no
    // instant at which another guest thread could resolve one against a swapchain that is being
    // destroyed. The host still knows which images belonged to it, because it is the one being
    // asked; what it answers here is the list, so this layer can say which handles to drop.
    let doomed = match host.swapchain_images(swapchain)? {
        DriverAnswer::Ok(images) => images,
        // A driver that will not enumerate a swapchain it is about to destroy is answering about
        // a swapchain in a state this layer cannot describe. Dropping every image handle would be
        // safe and would also be wrong about which ones: `retain` needs the list.
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result("vkGetSwapchainImagesKHR", result);
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} and the driver answered \
                 `vkGetSwapchainImagesKHR` for that swapchain with VkResult {result}, so this \
                 layer cannot tell which `VkImage` handles belong to it. Destroying it anyway \
                 would leave the guest holding `VkImage` handles whose driver objects are gone, \
                 and a non-dispatchable handle the driver still looks up is the defect Global \
                 Constraint 1 names",
                caller = at.caller
            )));
        }
    };
    host.destroy_swapchain(swapchain)?;
    vulkan.forget_swapchain(handle);
    vulkan.forget_images_of(|image| !doomed.contains(&image));

    c.ret().void();
    Ok(())
}

/// `VkResult vkAcquireNextImageKHR(VkDevice device, VkSwapchainKHR swapchain, uint64_t timeout,
/// VkSemaphore semaphore, VkFence fence, uint32_t *pImageIndex)`
///
/// # The four-way answer, and why `pImageIndex` is written for exactly two of them
///
/// This module's header has the table. The rule the code enforces is that
/// [`Acquired::image_index`](super::Acquired) is `Some` when and only when the driver wrote one,
/// and the write into guest memory happens when and only when it is `Some`. Writing a zero for a
/// `VK_TIMEOUT` would hand the guest image 0, which is a real image belonging to a frame that may
/// still be on the screen; the specification leaves the variable untouched precisely so that a
/// caller which forgot to check the result reads its own stale value rather than a plausible one
/// this layer invented.
///
/// # `semaphore` and `fence` are both optional, and at least one must be given
///
/// The specification requires at least one of them to be non-null — an acquire that signals
/// nothing gives the caller no way to know when the image is safe to write. That check is the
/// driver's and is deliberately not repeated here; what this layer owns is that a non-null handle
/// is one it issued, because a `VkSemaphore` and a `VkFence` are both bare 64-bit values with
/// nothing in them to tell one from the other.
pub(super) fn acquire_next_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkAcquireNextImageKHR";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let swapchain = vulkan.swapchain_token(at, CALL, args[1])?;
    let timeout = args[2];
    let semaphore = match args[3] {
        0 => None,
        handle => Some(vulkan.semaphore_token(at, CALL, handle)?),
    };
    let fence = match args[4] {
        0 => None,
        handle => Some(vulkan.fence_token(at, CALL, handle)?),
    };
    let index_at = require_pointer(at, CALL, "pImageIndex", args[5])?;

    let acquired = host.acquire_next_image(swapchain, timeout, semaphore, fence)?;
    if acquired.result != VK_SUCCESS {
        // Recorded whatever it is, including the two success codes: `VK_SUBOPTIMAL_KHR` and
        // `VK_TIMEOUT` are exactly the things a run that "worked but looked wrong" needs in its
        // log, and `Vulkan::driver_failures()` is where a reader finds them.
        vulkan.note_driver_result(CALL, acquired.result);
    }
    if let Some(index) = acquired.image_index {
        c.mem().write_u32(index_at, index, c.blame(5))?;
    }
    // **Verbatim, but for the one code Android never gives here** (module header): not clamped, not
    // turned into a swapchain recreation, and `VK_SUBOPTIMAL_KHR` answered as the `VK_SUCCESS` an
    // Android swapchain would have given for the same image.
    let answer = if acquired.result == VK_SUBOPTIMAL_KHR { VK_SUCCESS } else { acquired.result };
    c.ret().i32(answer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure's numbers are the ones the specification fixes**, with the offset a reader
    /// is most likely to get wrong stated as arithmetic rather than as a memory.
    #[test]
    fn the_swapchain_structure_numbers_are_the_specifications() {
        assert_eq!(SWAPCHAIN_CREATE_INFO_BYTES, 104);
        assert_eq!(STYPE_SWAPCHAIN_CREATE_INFO_KHR, 1_000_001_000);
        // Extension 2's structure-type base, which is where that number comes from.
        let swapchain_extension: u32 = 2;
        assert_eq!(STYPE_SWAPCHAIN_CREATE_INFO_KHR, 1_000_000_000 + (swapchain_extension - 1) * 1000);
        // `flags` is at 16 and is four bytes; `surface` is a `uint64_t` and is 8-aligned, so there
        // are four bytes of padding and it lands at 24 rather than 20. A layout that packed it at
        // 20 would read the two halves of the surface handle as `minImageCount` and `imageFormat`.
        assert_eq!(16 + 4 + 4, 24);
        // And the tail: `clipped` at 92 is four bytes, `oldSwapchain` is 8-aligned at 96, so the
        // structure is 104 with no trailing padding.
        assert_eq!(96 + 8, SWAPCHAIN_CREATE_INFO_BYTES);
        assert_eq!(NON_DISPATCHABLE_HANDLE_BYTES, 8);
    }

    /// **`VK_SUBOPTIMAL_KHR` and `VK_TIMEOUT` are successes and `VK_ERROR_OUT_OF_DATE_KHR` is
    /// not**, stated as the sign of the number because that is how the specification defines the
    /// distinction.
    ///
    /// The one property this file's whole resize story rests on: a shim that treated "non-zero" as
    /// "failed" would swallow the two codes the engine's resize branch is looking for, and a run
    /// in which the window was never resized would never notice.
    #[test]
    fn the_two_resize_codes_have_the_signs_the_specification_gives_them() {
        // **The sign, as `signum` rather than as `> 0`.** A comparison between two constants is
        // one the compiler folds away and clippy's `assertions_on_constants` names it; what this
        // states instead is the number the specification's own rule turns on -- a success code is
        // non-negative and a failure code is negative, and these four are the whole of the resize
        // story's arithmetic.
        assert_eq!(VK_SUBOPTIMAL_KHR.signum(), 1, "a success code");
        assert_eq!(VK_TIMEOUT.signum(), 1, "a success code");
        assert_eq!(VK_NOT_READY.signum(), 1, "a success code");
        assert_eq!(VK_ERROR_OUT_OF_DATE_KHR.signum(), -1, "a failure code");
        assert_eq!(VK_SUBOPTIMAL_KHR, 1_000_001_003);
        assert_eq!(VK_ERROR_OUT_OF_DATE_KHR, -1_000_001_004);
        assert_eq!(VK_TIMEOUT, 2);
        assert_eq!(VK_NOT_READY, 1);
        // And `VK_SUBOPTIMAL_KHR` is not `VK_SUCCESS`, which is the whole reason it must travel
        // rather than be normalised on the way past.
        assert_ne!(VK_SUBOPTIMAL_KHR, VK_SUCCESS);
    }

    /// The queue-family bound is above what any driver reports, which is what makes reaching it a
    /// finding rather than a limit. Stated as the number, for
    /// [`device`](super::super::device)'s reason: a comparison between two constants is one the
    /// compiler folds away.
    #[test]
    fn the_queue_family_bound_is_above_what_any_driver_reports() {
        // This host's NVIDIA driver reports six families (`docs/research/graphics-spike.md` §3).
        assert_eq!(MAX_SWAPCHAIN_QUEUE_FAMILIES, 32);
        // And the largest read it permits stays well under a page.
        assert_eq!(MAX_SWAPCHAIN_QUEUE_FAMILIES * 4, 128);
    }
}
