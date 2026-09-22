//! **`vkCreateImageView` and `vkDestroyImageView`: the one object stage 4 makes that stage 5 will
//! need.**
//!
//! # Why an image view is in this stage at all, when nothing here samples one
//!
//! Stage 4's present path — a barrier and `vkCmdClearColorImage` — does not use a `VkImageView`.
//! It could have been left out, and it is here for a reason that is about what the engine does
//! rather than about what this stage draws: **a renderer creates one view per swapchain image
//! immediately after `vkGetSwapchainImagesKHR`**, before it has a render pass or a pipeline,
//! because the views are what a framebuffer will later be built from. A guest that got a refusal
//! there would stop before it ever reached an acquire, and the census would record a swapchain
//! that was created and never used.
//!
//! So this is not pre-implementing Vulkan because the names exist (D17). It is the call that sits
//! between `vkGetSwapchainImagesKHR` and `vkAcquireNextImageKHR` in every renderer's bring-up, and
//! leaving it out would have made the rest of stage 4 unreachable from a real engine.
//!
//! # `components` and `subresourceRange` travel as bytes
//!
//! [`physical`](super::physical) makes the argument for output structures and it is the same one
//! in the input direction: `VkComponentMapping` is four `VkComponentSwizzle` enums and
//! `VkImageSubresourceRange` is five `uint32_t`s. Neither has a pointer, a `size_t` or a hole, so
//! the guest's aarch64 LP64 bytes **are** the bytes the driver reads, and decoding nine integers
//! into nine named fields and building them back up again is eighteen chances to transpose two of
//! them. A `VkComponentMapping` with `r` and `b` exchanged is a frame that renders in the wrong
//! colour and looks like a format-selection bug for a day.
//!
//! What is *not* passed through is the `image` member, because that one is a handle: a
//! non-dispatchable `uint64_t` the driver looks up, and a value the guest computed would name some
//! other image. It goes through the registry like every other handle in this crate, and the type
//! the host receives is [`HostImageRef`](super::HostImageRef) rather than a bare token — so that
//! stage 5's `vkCreateImage`, whose images are **not** a swapchain's, cannot be silently mistaken
//! for one of these.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, HostImageRef, ImageViewRequest};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO`.
const STYPE_IMAGE_VIEW_CREATE_INFO: u32 = 15;

/// `sizeof(VkComponentMapping)`: four `VkComponentSwizzle`, which are `uint32_t`s.
pub const COMPONENT_MAPPING_BYTES: usize = 16;

/// `sizeof(VkImageSubresourceRange)`.
///
/// ```text
/// VkImageAspectFlags   aspectMask;       //  0
/// uint32_t             baseMipLevel;     //  4
/// uint32_t             levelCount;       //  8
/// uint32_t             baseArrayLayer;   // 12
/// uint32_t             layerCount;       // 16
/// ```
///
/// Twenty bytes with alignment 4, which is why the structures that embed it — `VkImageViewCreateInfo`
/// and `VkImageMemoryBarrier` — both end on a four-byte boundary and are then padded to eight.
pub const IMAGE_SUBRESOURCE_RANGE_BYTES: usize = 20;

/// `sizeof(VkImageViewCreateInfo)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType            sType;              //  0  (then 4 of padding)
/// const void                *pNext;              //  8
/// VkImageViewCreateFlags     flags;              // 16  (then 4 of padding)
/// VkImage                    image;              // 24  (a uint64_t, 8-aligned)
/// VkImageViewType            viewType;           // 32
/// VkFormat                   format;             // 36
/// VkComponentMapping         components;         // 40  (16 bytes)
/// VkImageSubresourceRange    subresourceRange;   // 56  (20 bytes, ending at 76)
/// ```
///
/// 76 rounded up to the structure's own alignment of 8. `image` at 24 rather than 20 is the same
/// padding [`SWAPCHAIN_CREATE_INFO_BYTES`](super::SWAPCHAIN_CREATE_INFO_BYTES) documents, for the
/// same reason: a non-dispatchable handle is a `uint64_t` on every target.
pub const IMAGE_VIEW_CREATE_INFO_BYTES: usize = 80;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateImageView(VkDevice device, const VkImageViewCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkImageView *pView)`
pub(super) fn create_image_view(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateImageView";
    refuse_allocator(vulkan, at, CALL, args[2])?;

    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let create_info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let view_at = require_pointer(at, CALL, "pView", args[3])?;

    let info = c.mem().read_bytes(create_info_at, IMAGE_VIEW_CREATE_INFO_BYTES, c.blame(1))?;
    let u32_at =
        |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let u64_at =
        |offset: usize| u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight"));

    let stype = u32_at(0);
    if stype != STYPE_IMAGE_VIEW_CREATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pCreateInfo` whose `sType` is \
             {stype}, and `VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO` is \
             {STYPE_IMAGE_VIEW_CREATE_INFO}. The `image` member would be read at an offset that \
             belongs to a different structure, and this layer would then look that value up in \
             its image registry",
            caller = at.caller
        )));
    }
    let next = u64_at(8);
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pCreateInfo->pNext = {next:#x}`. An \
             image-view `pNext` chain carries `VkImageViewUsageCreateInfo`, which *narrows* the \
             usage the view is valid for, and `VkSamplerYcbcrConversionInfo`, which changes how \
             the texels are interpreted. Dropping either would make a view that is not the one \
             that was asked for, and the first symptom would be a validation error on a draw. \
             This layer does not know those layouts, so it refuses and names the address",
            caller = at.caller
        )));
    }

    // The one member that is a handle. Non-dispatchable, so a forged value would not fault -- it
    // would make a view of some other image, and everything afterwards would look right.
    let image = vulkan.image_token(at, CALL, u64_at(24))?;

    let request = ImageViewRequest {
        flags: u32_at(16),
        image: HostImageRef::Swapchain(image),
        view_type: u32_at(32),
        format: u32_at(36),
        components: info[40..40 + COMPONENT_MAPPING_BYTES].to_vec(),
        subresource_range: info[56..56 + IMAGE_SUBRESOURCE_RANGE_BYTES].to_vec(),
    };

    match host.create_image_view(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_image_view(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(view_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyImageView(VkDevice device, VkImageView imageView,
/// const VkAllocationCallbacks *pAllocator)`
///
/// `VK_NULL_HANDLE` is the specified no-op;
/// [`swapchain::destroy_swapchain`](super::swapchain) carries the argument for why that is the
/// specification's rule rather than a kindness.
pub(super) fn destroy_image_view(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyImageView";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;

    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "imageView", args[1])?;
    let view = vulkan.image_view_token(at, CALL, args[1])?;
    host.destroy_image_view(view)?;
    vulkan.forget_image_view(handle);
    c.ret().void();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure's numbers are the ones the specification fixes**, with the arithmetic that
    /// produces the size written out so a reader can check it rather than remember it.
    #[test]
    fn the_image_view_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_IMAGE_VIEW_CREATE_INFO, 15);
        assert_eq!(COMPONENT_MAPPING_BYTES, 16, "four VkComponentSwizzle");
        assert_eq!(COMPONENT_MAPPING_BYTES / 4, 4);
        assert_eq!(IMAGE_SUBRESOURCE_RANGE_BYTES, 20, "five uint32_t");
        assert_eq!(IMAGE_SUBRESOURCE_RANGE_BYTES / 4, 5);
        // `image` is 8-aligned, so `flags` at 16 is followed by four bytes of padding.
        assert_eq!(16 + 4 + 4, 24);
        // `components` at 40, `subresourceRange` at 56, ending at 76 and padded to the
        // structure's own alignment of 8.
        assert_eq!(40 + COMPONENT_MAPPING_BYTES, 56);
        assert_eq!(56 + IMAGE_SUBRESOURCE_RANGE_BYTES, 76);
        assert_eq!(IMAGE_VIEW_CREATE_INFO_BYTES, 80);
        assert_eq!(IMAGE_VIEW_CREATE_INFO_BYTES % 8, 0);
    }
}
