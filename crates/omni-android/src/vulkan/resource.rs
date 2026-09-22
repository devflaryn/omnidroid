//! **`VkBuffer`, `VkImage` and `VkSampler`: the three objects memory gets bound to.**
//!
//! # Why these three and not `VkBufferView`
//!
//! D17: this project does not implement a Vulkan call because the name exists. A buffer is what a
//! vertex, an index and a staging upload all are; an image is what a texture is; a sampler is the
//! other half of the one descriptor type a textured draw needs. `vkCreateBufferView` is a
//! *texel* buffer view — the thing a `UNIFORM_TEXEL_BUFFER` descriptor points at — and nothing in
//! this stage's path reaches one, so it is not here and a guest that calls it gets a refusal
//! naming the function, which is how the next stage finds out it is needed.
//!
//! # The `VkImage` this module creates is not the `VkImage` stage 4 hands out
//!
//! They are the same Vulkan type and completely different objects, and
//! [`HostImageRef`](super::HostImageRef) is what keeps them apart. A swapchain image is owned by
//! its swapchain, already has memory, and must never be destroyed by the guest; one created here
//! is owned by the guest, has **no memory at all** until `vkBindImageMemory`, and must be. Both
//! are 64-bit non-dispatchable values with nothing in them to tell one from the other, so they
//! live in separate ranges of the boundary's data area and
//! [`Vulkan::image_ref_token`](super::Vulkan) is the one lookup that accepts either — answering
//! *which kind it found*, so that a host matching on the enum cannot treat one as the other.
//!
//! A `vkDestroyImage` of a swapchain image therefore refuses by name, which is the mistake this
//! arrangement exists to make unreachable: the specification calls it undefined behaviour, and no
//! validation layer on this machine would report it (`docs/research/graphics-spike.md` §6).
//!
//! # `VkSamplerCreateInfo` travels as bytes and `VkBufferCreateInfo` does not
//!
//! The rule is the one [`physical`](super::physical) states: a structure with no pointer and no
//! handle in it *is* its bytes on both targets, and a structure with one has to be decoded. A
//! sampler's sixteen members are all scalars, so its body crosses whole — and `addressModeU`
//! written where `addressModeV` belongs would be a texture that wraps on the wrong axis and looks
//! like an atlas bug for a day. A buffer and an image each carry `pQueueFamilyIndices`, which is a
//! guest array this layer must follow and copy.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{BufferRequest, DriverAnswer, ImageRequest};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO`.
const STYPE_BUFFER_CREATE_INFO: u32 = 12;
/// `VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO`.
const STYPE_IMAGE_CREATE_INFO: u32 = 14;
/// `VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO`.
const STYPE_SAMPLER_CREATE_INFO: u32 = 31;

/// `sizeof(VkBufferCreateInfo)`.
///
/// ```text
/// VkStructureType      sType;                   //  0  (then 4 of padding)
/// const void          *pNext;                   //  8
/// VkBufferCreateFlags  flags;                   // 16  (then 4 of padding)
/// VkDeviceSize         size;                    // 24
/// VkBufferUsageFlags   usage;                   // 32
/// VkSharingMode        sharingMode;             // 36
/// uint32_t             queueFamilyIndexCount;   // 40  (then 4 of padding)
/// const uint32_t      *pQueueFamilyIndices;     // 48
/// ```
pub const BUFFER_CREATE_INFO_BYTES: usize = 56;

/// `sizeof(VkImageCreateInfo)`.
///
/// ```text
/// VkStructureType     sType;                   //  0  (then 4 of padding)
/// const void         *pNext;                   //  8
/// VkImageCreateFlags  flags;                   // 16
/// VkImageType         imageType;               // 20
/// VkFormat            format;                  // 24
/// VkExtent3D          extent;                  // 28  (three uint32_t, to 40)
/// uint32_t            mipLevels;               // 40
/// uint32_t            arrayLayers;             // 44
/// VkSampleCountFlagBits samples;               // 48
/// VkImageTiling       tiling;                  // 52
/// VkImageUsageFlags   usage;                   // 56
/// VkSharingMode       sharingMode;             // 60
/// uint32_t            queueFamilyIndexCount;   // 64  (then 4 of padding)
/// const uint32_t     *pQueueFamilyIndices;     // 72
/// VkImageLayout       initialLayout;           // 80  (then 4 of padding, alignment 8)
/// ```
pub const IMAGE_CREATE_INFO_BYTES: usize = 88;

/// `sizeof(VkSamplerCreateInfo)`.
///
/// ```text
/// VkStructureType       sType;                     //  0  (then 4 of padding)
/// const void           *pNext;                     //  8
/// VkSamplerCreateFlags  flags;                     // 16
/// VkFilter              magFilter;                 // 20
/// VkFilter              minFilter;                 // 24
/// VkSamplerMipmapMode   mipmapMode;                // 28
/// VkSamplerAddressMode  addressModeU;              // 32
/// VkSamplerAddressMode  addressModeV;              // 36
/// VkSamplerAddressMode  addressModeW;              // 40
/// float                 mipLodBias;                // 44
/// VkBool32              anisotropyEnable;          // 48
/// float                 maxAnisotropy;             // 52
/// VkBool32              compareEnable;             // 56
/// VkCompareOp           compareOp;                 // 60
/// float                 minLod;                    // 64
/// float                 maxLod;                    // 68
/// VkBorderColor         borderColor;               // 72
/// VkBool32              unnormalizedCoordinates;   // 76
/// ```
pub const SAMPLER_CREATE_INFO_BYTES: usize = 80;

/// The part of a `VkSamplerCreateInfo` that crosses the host seam: everything after `pNext`.
///
/// Sixteen four-byte scalars. See this module's header for why they travel whole.
pub const SAMPLER_CREATE_INFO_BODY_BYTES: usize = SAMPLER_CREATE_INFO_BYTES - 16;

/// How many `pQueueFamilyIndices` one buffer or image will be created with.
///
/// [`MAX_SWAPCHAIN_QUEUE_FAMILIES`](super::MAX_SWAPCHAIN_QUEUE_FAMILIES)' argument and its number:
/// the count is a guest `uint32_t`, no device has thirty-two queue families, and honouring it
/// unbounded would be a guest-controlled host allocation.
pub const MAX_RESOURCE_QUEUE_FAMILIES: usize = 32;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateBuffer(VkDevice device, const VkBufferCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkBuffer *pBuffer)`
pub(super) fn create_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateBuffer";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pBuffer", args[3])?;

    let info = c.mem().read_bytes(info_at, BUFFER_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_BUFFER_CREATE_INFO,
        "VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO",
        "`size` and `usage` would be read at offsets belonging to a different structure, and a \
         buffer created without the usage bit a later bind needs fails at the bind rather than \
         here",
        "a buffer `pNext` chain carries `VkExternalMemoryBufferCreateInfo`, \
         `VkBufferOpaqueCaptureAddressCreateInfo` and the dedicated-allocation structures -- each \
         of which changes what the buffer is rather than decorating it",
    )?;

    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let count = u32_at(40) as usize;
    let families = decode_families(c, at, CALL, count, &info[48..56], 1)?;

    let request = BufferRequest {
        flags: u32_at(16),
        size: u64::from_le_bytes(info[24..32].try_into().expect("eight")),
        usage: u32_at(32),
        sharing_mode: u32_at(36),
        queue_families: families,
    };
    if request.size == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `size = 0`, which the specification \
             forbids. A zero-length buffer has no memory requirements to answer and nothing that \
             could be bound to it, so every call after this one would be about an object with no \
             extent",
            caller = at.caller
        )));
    }

    match host.create_buffer(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_buffer(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyBuffer(VkDevice device, VkBuffer buffer,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyBuffer";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "buffer", args[1])?;
    let token = vulkan.buffer_token(at, CALL, args[1])?;
    host.destroy_buffer(token)?;
    vulkan.forget_buffer(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateImage(VkDevice device, const VkImageCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkImage *pImage)`
pub(super) fn create_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateImage";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pImage", args[3])?;

    let info = c.mem().read_bytes(info_at, IMAGE_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_IMAGE_CREATE_INFO,
        "VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO",
        "`extent`, `format` and `usage` would be read at offsets belonging to a different \
         structure, and an image of the wrong extent is a texture upload that writes past what \
         was allocated for it",
        "an image `pNext` chain carries `VkExternalMemoryImageCreateInfo`, \
         `VkImageDrmFormatModifierListCreateInfoEXT` and the Android hardware-buffer structures -- \
         and the last of those is the one this layer would most like to know the engine sends, \
         which is why it is named rather than dropped",
    )?;

    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let count = u32_at(64) as usize;
    let families = decode_families(c, at, CALL, count, &info[72..80], 1)?;

    let request = ImageRequest {
        flags: u32_at(16),
        image_type: u32_at(20),
        format: u32_at(24),
        extent: [u32_at(28), u32_at(32), u32_at(36)],
        mip_levels: u32_at(40),
        array_layers: u32_at(44),
        samples: u32_at(48),
        tiling: u32_at(52),
        usage: u32_at(56),
        sharing_mode: u32_at(60),
        queue_families: families,
        initial_layout: u32_at(80),
    };
    if request.extent.contains(&0) {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `extent = {extent:?}`, and the \
             specification requires every component to be at least one -- a `VK_IMAGE_TYPE_2D` \
             image still has `depth = 1`. A zero component here is most often a guest that wrote \
             `extent` at the wrong offset, which this refusal names rather than turning into an \
             image nothing can be copied into",
            caller = at.caller,
            extent = request.extent
        )));
    }

    match host.create_image(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_created_image(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyImage(VkDevice device, VkImage image, const VkAllocationCallbacks *pAllocator)`
///
/// **A swapchain image reaches the refusal rather than the driver.** The two families have
/// separate ranges of the data area, so a handle from `vkGetSwapchainImagesKHR` is simply not in
/// the created-image registry and [`Vulkan::created_image_token`](super::Vulkan) says so by name.
/// That is the whole value of the split: destroying a swapchain image is undefined behaviour, it
/// would take the presentation engine's own resource with it, and nothing on this machine would
/// report it.
pub(super) fn destroy_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyImage";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "image", args[1])?;
    let token = vulkan.created_image_token(at, CALL, args[1])?;
    host.destroy_image(token)?;
    vulkan.forget_created_image(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateSampler(VkDevice device, const VkSamplerCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkSampler *pSampler)`
pub(super) fn create_sampler(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateSampler";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pSampler", args[3])?;

    let info = c.mem().read_bytes(info_at, SAMPLER_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_SAMPLER_CREATE_INFO,
        "VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO",
        "the sixteen scalars after `pNext` would be taken from a different structure, and a \
         sampler built out of them would filter and wrap in ways nothing asked for",
        "a sampler `pNext` chain carries `VkSamplerYcbcrConversionInfo`, which changes how texels \
         are *interpreted*, and `VkSamplerReductionModeCreateInfo`, which changes what filtering \
         means. Dropping either makes a sampler that is not the one that was asked for",
    )?;

    match host.create_sampler(device, &info[16..SAMPLER_CREATE_INFO_BYTES])? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_sampler(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroySampler(VkDevice device, VkSampler sampler,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_sampler(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroySampler";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "sampler", args[1])?;
    let token = vulkan.sampler_token(at, CALL, args[1])?;
    host.destroy_sampler(token)?;
    vulkan.forget_sampler(handle);
    c.ret().void();
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// The `sType` and `pNext` check every `vkCreate*` in this stage makes, written once.
///
/// Both refusals are about the same failure from two directions: a structure read at the wrong
/// offsets, and a structure whose *extra* half this layer cannot see. The caller supplies the
/// consequence, because the consequence differs per call and a generic "this would be wrong" is
/// the kind of message that gets skimmed.
pub(super) fn check_header(
    at: &Site,
    call: &str,
    info: &[u8],
    expected: u32,
    name: &str,
    consequence: &str,
    chain: &str,
) -> AbiResult<()> {
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != expected {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with a `pCreateInfo` whose `sType` is \
             {stype}, and `{name}` is {expected}. {consequence}",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `pCreateInfo->pNext = {next:#x}`. \
             **This layer refuses every `pNext` chain the guest sends rather than walking one**, \
             and the reason is that walking means knowing: a structure this layer did not \
             recognise would have to be either dropped, which produces an object that is not the \
             one that was asked for, or forwarded blind, which hands a driver a guest pointer \
             nothing validated. {chain}. The address is named so that a run reports which \
             structure the engine actually sends, which is what would decide whether to \
             implement it",
            caller = at.caller
        )));
    }
    Ok(())
}

/// Decode a `pQueueFamilyIndices` array, which both `VkBufferCreateInfo` and `VkImageCreateInfo`
/// carry at a different offset and with the same rules.
///
/// **The count is honoured even for `VK_SHARING_MODE_EXCLUSIVE`**, where the specification says
/// the members are ignored: an engine that left a stale count and a stale pointer there is
/// conforming, and refusing it would refuse a correct program. What is *not* done is reading the
/// array when the pointer is NULL, which an exclusive-sharing caller is entitled to leave.
fn decode_families(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    count: usize,
    pointer_bytes: &[u8],
    argument: usize,
) -> AbiResult<Vec<u32>> {
    let pointer = u64::from_le_bytes(pointer_bytes[0..8].try_into().expect("eight"));
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_RESOURCE_QUEUE_FAMILIES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `queueFamilyIndexCount = {count}`, \
             and this layer reads at most {MAX_RESOURCE_QUEUE_FAMILIES}. No device has that many \
             queue families, and honouring a guest `uint32_t` unbounded is a guest-controlled \
             host allocation (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pQueueFamilyIndices", pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * 4, c.blame(argument))?;
    Ok((0..count)
        .map(|index| u32::from_le_bytes(bytes[index * 4..][..4].try_into().expect("four")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure sizes are the specification's**, with the arithmetic written out.
    #[test]
    fn the_resource_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_BUFFER_CREATE_INFO, 12);
        assert_eq!(STYPE_IMAGE_CREATE_INFO, 14);
        assert_eq!(STYPE_SAMPLER_CREATE_INFO, 31);

        // `size` is a `VkDeviceSize` at 24, so `flags` at 16 is followed by four bytes of
        // padding; `pQueueFamilyIndices` is a pointer at 48 for the same reason after the count
        // at 40.
        assert_eq!(BUFFER_CREATE_INFO_BYTES, 48 + 8);
        // The image's three-component extent starts at 28 and ends at 40.
        assert_eq!(28 + 3 * 4, 40);
        assert_eq!(IMAGE_CREATE_INFO_BYTES, 80 + 4 + 4);
        // The sampler is sixteen four-byte members after a sixteen-byte header.
        assert_eq!(SAMPLER_CREATE_INFO_BYTES, 16 + 16 * 4);
        assert_eq!(SAMPLER_CREATE_INFO_BODY_BYTES, 64);
    }
}
