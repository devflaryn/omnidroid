//! **Command pools, command buffers, and the two `vkCmd*` calls that make an image a visible
//! colour.**
//!
//! # Why a clear and a barrier are enough, and what they are enough *of*
//!
//! `vkCmdClearColorImage` writes a solid colour into an image. It needs no `VkRenderPass`, no
//! `VkFramebuffer`, no `VkPipeline`, no `VkShaderModule` and therefore no SPIR-V — which is
//! precisely why `omni_gfx::vulkan`'s own present path is built on it, and that file's header
//! records the cost that decision avoided: a shader compiler in the build (`shaderc` needs a
//! working CMake + MSVC + Ninja chain) for a triangle that would prove nothing.
//!
//! The barrier is what makes the clear legal. A swapchain image arrives from
//! `vkAcquireNextImageKHR` in `VK_IMAGE_LAYOUT_UNDEFINED` or
//! `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`, `vkCmdClearColorImage` requires
//! `VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL`, and `vkQueuePresentKHR` requires
//! `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` again. Two `vkCmdPipelineBarrier` calls move it between
//! them. Getting that wrong is undefined behaviour with **no diagnostic on this machine** — there
//! are no validation layers here (`docs/research/graphics-spike.md` §6) — so the barrier is not
//! ceremony, it is the thing that decides whether the frame is the colour that was asked for.
//!
//! # `vkCmd*` returns `void`, and that changes what a mistake costs
//!
//! Every command in this file that begins `vkCmd` records into a command buffer rather than
//! executing, and returns nothing. There is no `VkResult` for a bad recording: the driver reports
//! it at `vkEndCommandBuffer`, or at submission, or not at all. So a shim that quietly dropped a
//! barrier would produce a run in which every call succeeded and the frame was wrong — which is
//! the exact shape Global Constraint 1 names, one layer deeper than usual. Everything here that
//! cannot be forwarded is a refusal.
//!
//! # `vkCmdPipelineBarrier` has ten parameters, and two of them are on the stack
//!
//! AAPCS64 passes the first eight integer arguments in `X0`-`X7` and the rest in the caller's
//! overflow area. `vkCmdPipelineBarrier`'s ninth and tenth — `imageMemoryBarrierCount` and
//! `pImageMemoryBarriers`, which are the two this stage actually uses — are therefore **not** in
//! the `args` array [`proc_slot`](super::proc_slot) captures, which is `X0`-`X7` and deliberately
//! stops there. They are read through [`ImportCall::args`], whose cursor walks the overflow area
//! for exactly this case. A handler that read `args[7]` and stopped would silently record a
//! barrier with no image barriers in it.
//!
//! # Buffer memory barriers are refused, and that is not the same as being dropped
//!
//! A `VkBufferMemoryBarrier` names a `VkBuffer`. Stage 4 has no `VkBuffer` registry, because
//! stage 4 creates no buffers — device memory and buffers are stage 5. Forwarding a barrier with a
//! guest-chosen buffer handle would be handing the driver a non-dispatchable value nothing
//! validated, and dropping the barrier would remove a synchronisation the engine asked for. So a
//! non-zero `bufferMemoryBarrierCount` is a refusal naming the count, and the guest's own
//! `vkCreateBuffer` would have been refused before it could get here.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, HostImageRef, ImageBarrier, PipelineBarrier};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::view::IMAGE_SUBRESOURCE_RANGE_BYTES;
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO`.
const STYPE_COMMAND_POOL_CREATE_INFO: u32 = 39;
/// `VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO`.
const STYPE_COMMAND_BUFFER_ALLOCATE_INFO: u32 = 40;
/// `VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO`.
const STYPE_COMMAND_BUFFER_BEGIN_INFO: u32 = 42;
/// `VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER`.
const STYPE_BUFFER_MEMORY_BARRIER: u32 = 44;
/// `VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER`.
const STYPE_IMAGE_MEMORY_BARRIER: u32 = 45;
/// `VK_STRUCTURE_TYPE_MEMORY_BARRIER`.
const STYPE_MEMORY_BARRIER: u32 = 46;

/// `sizeof(VkCommandPoolCreateInfo)`.
///
/// ```text
/// VkStructureType            sType;              //  0  (then 4 of padding)
/// const void                *pNext;              //  8
/// VkCommandPoolCreateFlags   flags;              // 16
/// uint32_t                   queueFamilyIndex;   // 20
/// ```
///
/// The one structure in this file with **no** padding between `flags` and the member after it,
/// because that member is a `uint32_t` rather than a handle or a pointer. A reader who has just
/// read `VkSwapchainCreateInfoKHR`'s layout will expect four bytes of padding at 20 and there are
/// none.
pub const COMMAND_POOL_CREATE_INFO_BYTES: usize = 24;

/// `sizeof(VkCommandBufferAllocateInfo)`.
///
/// ```text
/// VkStructureType         sType;                 //  0  (then 4 of padding)
/// const void             *pNext;                 //  8
/// VkCommandPool           commandPool;           // 16  (a uint64_t)
/// VkCommandBufferLevel    level;                 // 24
/// uint32_t                commandBufferCount;    // 28
/// ```
pub const COMMAND_BUFFER_ALLOCATE_INFO_BYTES: usize = 32;

/// `sizeof(VkCommandBufferBeginInfo)`.
///
/// ```text
/// VkStructureType                        sType;              //  0  (then 4 of padding)
/// const void                            *pNext;              //  8
/// VkCommandBufferUsageFlags              flags;              // 16  (then 4 of padding)
/// const VkCommandBufferInheritanceInfo  *pInheritanceInfo;   // 24
/// ```
pub const COMMAND_BUFFER_BEGIN_INFO_BYTES: usize = 32;

/// `sizeof(VkMemoryBarrier)`.
///
/// ```text
/// VkStructureType   sType;           //  0  (then 4 of padding)
/// const void       *pNext;           //  8
/// VkAccessFlags     srcAccessMask;   // 16
/// VkAccessFlags     dstAccessMask;   // 20
/// ```
pub const MEMORY_BARRIER_BYTES: usize = 24;

/// `sizeof(VkImageMemoryBarrier)`.
///
/// ```text
/// VkStructureType           sType;                 //  0  (then 4 of padding)
/// const void               *pNext;                 //  8
/// VkAccessFlags             srcAccessMask;         // 16
/// VkAccessFlags             dstAccessMask;         // 20
/// VkImageLayout             oldLayout;             // 24
/// VkImageLayout             newLayout;             // 28
/// uint32_t                  srcQueueFamilyIndex;   // 32
/// uint32_t                  dstQueueFamilyIndex;   // 36
/// VkImage                   image;                 // 40  (a uint64_t)
/// VkImageSubresourceRange   subresourceRange;      // 48  (20 bytes, ending at 68)
/// ```
///
/// 68 rounded up to the structure's alignment of 8. `image` lands at 40 with **no** padding
/// before it, because the six `uint32_t`-sized members before it are an even number and fill
/// 16..40 exactly — which is the arithmetic a reader should check rather than assume, since the
/// neighbouring structures in this stage all *do* have padding there.
pub const IMAGE_MEMORY_BARRIER_BYTES: usize = 72;

/// `sizeof(VkBufferMemoryBarrier)`. Stated so the refusal for one can say how many bytes it
/// declined to read, and for no other reason: stage 4 forwards none.
pub const BUFFER_MEMORY_BARRIER_BYTES: usize = 56;

/// How many command buffers one `vkAllocateCommandBuffers` or `vkFreeCommandBuffers` may name.
///
/// An allocation bound. `commandBufferCount` is a guest `uint32_t`; thirty-two is far above the
/// one-per-frame-in-flight a renderer allocates in a batch, and it is **half**
/// [`MAX_COMMAND_BUFFERS`](super::MAX_COMMAND_BUFFERS) so that a single call cannot fill the
/// registry on its own — a guest that allocated its whole budget in one call would leave the next
/// `vkAllocateCommandBuffers` refusing for a reason that looks like this bound rather than like
/// the registry's.
pub const MAX_COMMAND_BUFFERS_PER_CALL: usize = 32;

/// How many barriers of one kind a single `vkCmdPipelineBarrier` may name.
///
/// An allocation bound. Thirty-two image barriers in one call is already an unusual renderer —
/// a deferred pass transitioning every G-buffer attachment at once is a handful — and each is 72
/// bytes, so the largest read this permits is 2,304 bytes.
pub const MAX_BARRIERS: usize = 32;

/// How many `VkImageSubresourceRange`s one `vkCmdClearColorImage` may name.
///
/// An allocation bound. A clear of a whole image is one range; eight covers a clear of several mip
/// or array ranges separately, which nothing has been seen to do.
pub const MAX_CLEAR_RANGES: usize = 8;

/// `sizeof(VkClearColorValue)`: a union of `float[4]`, `int32_t[4]` and `uint32_t[4]`.
///
/// Sixteen bytes whichever member is live, which is what makes carrying it as bytes exact rather
/// than approximate — see [`VulkanHost::cmd_clear_color_image`](super::VulkanHost) for why this
/// layer must not decide which member that is.
const CLEAR_COLOR_VALUE_BYTES: usize = 16;

// --------------------------------------------------------------------------- pools and buffers

/// `VkResult vkCreateCommandPool(VkDevice device, const VkCommandPoolCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkCommandPool *pCommandPool)`
pub(super) fn create_command_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateCommandPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let create_info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pCommandPool", args[3])?;

    let info = c.mem().read_bytes(create_info_at, COMMAND_POOL_CREATE_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_COMMAND_POOL_CREATE_INFO {
        return Err(wrong_stype(
            at,
            CALL,
            "VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO",
            STYPE_COMMAND_POOL_CREATE_INFO,
            stype,
            "`queueFamilyIndex` would be read at an offset belonging to a different structure, \
             and a pool created for the wrong family produces command buffers that cannot be \
             submitted to the queue the engine has",
        ));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(chain_refused(
            at,
            CALL,
            next,
            "a command-pool `pNext` chain is where `VkCommandPoolCreateInfo`'s extensions go, \
             including the one that binds the pool to a specific device memory allocator. \
             Dropping it would create a pool with different properties from the one asked for",
        ));
    }
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let family = u32::from_le_bytes(info[20..24].try_into().expect("four"));

    match host.create_command_pool(device, flags, family)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_command_pool(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyCommandPool(VkDevice device, VkCommandPool commandPool,
/// const VkAllocationCallbacks *pAllocator)`
///
/// **Every command buffer allocated from the pool is freed by this call**, and their guest handles
/// go with it. A `VkCommandBuffer` is *dispatchable*, so a handle left in the registry would
/// resolve to a token whose host object is freed memory the driver would then dereference —
/// Global Constraint 11 calls that Critical, and it is reachable with a handle this layer itself
/// issued. [`Vulkan::forget_command_buffers_of`](super::Vulkan) is what prevents it.
pub(super) fn destroy_command_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyCommandPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "commandPool", args[1])?;
    let pool = vulkan.command_pool_token(at, CALL, args[1])?;

    // Which buffers the pool owns has to be asked **before** it is destroyed, because afterwards
    // there is nothing to ask. The host answers the list; this layer drops the handles.
    let doomed = host.command_buffers_of(pool)?;
    host.destroy_command_pool(pool)?;
    vulkan.forget_command_pool(handle);
    vulkan.forget_command_buffers_of(|buffer| !doomed.contains(&buffer));
    c.ret().void();
    Ok(())
}

/// `VkResult vkResetCommandPool(VkDevice device, VkCommandPool commandPool,
/// VkCommandPoolResetFlags flags)`
///
/// **The handles survive.** Resetting a pool returns every command buffer allocated from it to the
/// initial state; it does not free them, so the guest's `VkCommandBuffer` handles stay valid and
/// stay in the registry. That is the difference between this and
/// [`destroy_command_pool`], and it is the reason they are separate functions rather than one with
/// a flag.
pub(super) fn reset_command_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkResetCommandPool";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let pool = vulkan.command_pool_token(at, CALL, args[1])?;
    match host.reset_command_pool(pool, args[2] as u32)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

/// `VkResult vkAllocateCommandBuffers(VkDevice device,
/// const VkCommandBufferAllocateInfo *pAllocateInfo, VkCommandBuffer *pCommandBuffers)`
///
/// # The array is written whole or not at all
///
/// `pCommandBuffers` is `commandBufferCount` entries and the guest sized it from the same number
/// it put in the structure, so there is no two-call protocol here and no `VK_INCOMPLETE`. What
/// there is instead is the registry: every buffer the driver produced is registered *before* any
/// of them is written into the guest's array, so a registry that filled up halfway leaves the
/// guest's array untouched rather than half-filled with handles beside a `VK_SUCCESS`.
pub(super) fn allocate_command_buffers(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkAllocateCommandBuffers";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let allocate_info_at = require_pointer(at, CALL, "pAllocateInfo", args[1])?;
    let array_at = require_pointer(at, CALL, "pCommandBuffers", args[2])?;

    let info =
        c.mem().read_bytes(allocate_info_at, COMMAND_BUFFER_ALLOCATE_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_COMMAND_BUFFER_ALLOCATE_INFO {
        return Err(wrong_stype(
            at,
            CALL,
            "VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO",
            STYPE_COMMAND_BUFFER_ALLOCATE_INFO,
            stype,
            "`commandPool` would be read at byte 16 of a different structure and looked up in \
             this layer's pool registry, and `commandBufferCount` would be whatever sits at 28",
        ));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(chain_refused(
            at,
            CALL,
            next,
            "an allocate `pNext` chain carries `VkCommandBufferInheritanceInfo`-adjacent \
             extensions and device-group allocation. Dropping it would allocate buffers with \
             different properties from the ones asked for",
        ));
    }
    let pool_handle = u64::from_le_bytes(info[16..24].try_into().expect("eight"));
    let pool = vulkan.command_pool_token(at, CALL, pool_handle)?;
    let level = u32::from_le_bytes(info[24..28].try_into().expect("four"));
    let count = u32::from_le_bytes(info[28..32].try_into().expect("four"));

    if count as usize > MAX_COMMAND_BUFFERS_PER_CALL {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `commandBufferCount = {count}`, and \
             this layer allocates at most {MAX_COMMAND_BUFFERS_PER_CALL} in one call. The count \
             is a guest `uint32_t` indexing an array of dispatchable handles, so honouring it \
             unbounded would be a guest-controlled host allocation (Global Constraint 11). This \
             is a refusal rather than a truncation because a caller that asked for {count} and \
             received fewer would read past the end of what was written",
            caller = at.caller
        )));
    }

    let tokens = match host.allocate_command_buffers(pool, level, count)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            return Ok(());
        }
        DriverAnswer::Ok(tokens) => tokens,
    };
    if tokens.len() != count as usize {
        return Err(at.refuse(format!(
            "the guest asked `{CALL}` for {count} command buffer(s) and the host answered with \
             {got}. Writing the shorter list would leave the tail of the guest's array as \
             whatever was in it -- which the guest would then submit as dispatchable handles",
            got = tokens.len()
        )));
    }

    // Registered first, written second. See this function's documentation.
    let mut handles = Vec::with_capacity(tokens.len() * 8);
    for token in &tokens {
        let registered = vulkan.register_command_buffer(at, *token)?;
        c.mem().write_bytes(registered.at, &registered.image, c.blame(2))?;
        handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
    }
    if !handles.is_empty() {
        c.mem().write_bytes(array_at, &handles, c.blame(2))?;
    }
    c.ret().i32(VK_SUCCESS);
    Ok(())
}

/// `void vkFreeCommandBuffers(VkDevice device, VkCommandPool commandPool,
/// uint32_t commandBufferCount, const VkCommandBuffer *pCommandBuffers)`
///
/// A `VK_NULL_HANDLE` **entry** is a no-op, unlike the other destroys where it is the whole call:
/// the specification says elements of `pCommandBuffers` may be `VK_NULL_HANDLE` and are then
/// ignored, which is what lets a caller free a partly-allocated array.
pub(super) fn free_command_buffers(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkFreeCommandBuffers";
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    let pool = vulkan.command_pool_token(at, CALL, args[1])?;
    let count = args[2] as u32 as usize;
    if count == 0 {
        c.ret().void();
        return Ok(());
    }
    if count > MAX_COMMAND_BUFFERS_PER_CALL {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `commandBufferCount = {count}`, and \
             this layer reads at most {MAX_COMMAND_BUFFERS_PER_CALL} in one call (Global \
             Constraint 11: the count is a guest `uint32_t`)",
            caller = at.caller
        )));
    }
    let array_at = require_pointer(at, CALL, "pCommandBuffers", args[3])?;
    let bytes = c.mem().read_bytes(array_at, count * 8, c.blame(3))?;

    let mut tokens = Vec::with_capacity(count);
    let mut handles = Vec::with_capacity(count);
    for index in 0..count {
        let handle =
            u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().expect("eight"));
        if handle == 0 {
            continue; // The specified per-entry no-op.
        }
        tokens.push(vulkan.command_buffer_token(at, CALL, handle)?);
        handles.push(guest_pointer(at, "pCommandBuffers", handle)?);
    }
    host.free_command_buffers(pool, &tokens)?;
    for handle in handles {
        vulkan.forget_command_buffer(handle);
    }
    c.ret().void();
    Ok(())
}

/// `VkResult vkBeginCommandBuffer(VkCommandBuffer commandBuffer,
/// const VkCommandBufferBeginInfo *pBeginInfo)`
///
/// # `pInheritanceInfo` is refused rather than ignored
///
/// It is meaningful only for a **secondary** command buffer executing inside a render pass, and
/// stage 4 has no render pass. The specification says it is ignored for a primary buffer — so
/// ignoring it here would be *correct* for every call a stage 4 guest can make, and would silently
/// become wrong the moment the engine allocates a secondary buffer. A refusal naming the pointer
/// is what makes that transition visible rather than a frame that records into nothing.
pub(super) fn begin_command_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkBeginCommandBuffer";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let begin_info_at = require_pointer(at, CALL, "pBeginInfo", args[1])?;

    let info = c.mem().read_bytes(begin_info_at, COMMAND_BUFFER_BEGIN_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_COMMAND_BUFFER_BEGIN_INFO {
        return Err(wrong_stype(
            at,
            CALL,
            "VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO",
            STYPE_COMMAND_BUFFER_BEGIN_INFO,
            stype,
            "`flags` would be read at byte 16 of a different structure, and \
             `VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT` being lost or gained changes whether \
             the driver may discard the recording after one submission",
        ));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(chain_refused(
            at,
            CALL,
            next,
            "a begin `pNext` chain carries `VkDeviceGroupCommandBufferBeginInfo`, which selects \
             which physical devices of a group the recording applies to",
        ));
    }
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let inheritance = u64::from_le_bytes(info[24..32].try_into().expect("eight"));
    if inheritance != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with \
             `pBeginInfo->pInheritanceInfo = {inheritance:#x}`. That structure names a \
             `VkRenderPass`, a `VkFramebuffer` and a `VkQueryPool`, and **stage 4 has none of \
             those** -- there is no render pass in this layer at all, so there is nothing to \
             resolve those handles against and forwarding numbers the guest chose would be \
             handing the driver three unvalidated handles. It is ignored by the specification for \
             a *primary* command buffer, which is why dropping it would work today and would stop \
             working silently the first time the engine records a secondary one",
            caller = at.caller
        )));
    }

    match host.begin_command_buffer(buffer, flags)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

/// `VkResult vkEndCommandBuffer(VkCommandBuffer commandBuffer)`
pub(super) fn end_command_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkEndCommandBuffer";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    match host.end_command_buffer(buffer)? {
        DriverAnswer::Failed(result) => {
            // **The one place a recording error surfaces.** Every `vkCmd*` returns `void`, so a
            // malformed recording has nowhere to be reported until here.
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

/// `VkResult vkResetCommandBuffer(VkCommandBuffer commandBuffer,
/// VkCommandBufferResetFlags flags)`
pub(super) fn reset_command_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkResetCommandBuffer";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    match host.reset_command_buffer(buffer, args[1] as u32)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

// ------------------------------------------------------------------------------ the recordings

/// `void vkCmdPipelineBarrier(VkCommandBuffer commandBuffer, VkPipelineStageFlags srcStageMask,
/// VkPipelineStageFlags dstStageMask, VkDependencyFlags dependencyFlags,
/// uint32_t memoryBarrierCount, const VkMemoryBarrier *pMemoryBarriers,
/// uint32_t bufferMemoryBarrierCount, const VkBufferMemoryBarrier *pBufferMemoryBarriers,
/// uint32_t imageMemoryBarrierCount, const VkImageMemoryBarrier *pImageMemoryBarriers)`
///
/// **Ten parameters, of which the last two are on the stack.** This module's header explains why
/// they are read through [`ImportCall::args`] rather than out of the `args` array, and what a
/// handler that stopped at `X7` would silently record.
pub(super) fn cmd_pipeline_barrier(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdPipelineBarrier";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;

    // The ninth and tenth arguments, from the AAPCS64 overflow area. A fresh cursor walks the
    // whole list; the first eight it re-reads are the ones already in `args` and are discarded,
    // which costs eight register reads and buys a single source of truth for the split point.
    let (image_count, image_array) = {
        let mut cursor = c.args();
        for _ in 0..ARG_REGISTERS {
            cursor.next_u64()?;
        }
        (cursor.next_u64()? as u32, cursor.next_u64()?)
    };

    let buffer_count = args[6] as u32;
    if buffer_count != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with \
             `bufferMemoryBarrierCount = {buffer_count}` and \
             `pBufferMemoryBarriers = {array:#x}`. Each of those \
             {BUFFER_MEMORY_BARRIER_BYTES}-byte structures names a `VkBuffer`, and **stage 4 has \
             no `VkBuffer` registry** because it creates no buffers -- device memory and buffers \
             are stage 5. Forwarding one would hand the driver a non-dispatchable handle nothing \
             validated, which is the defect Global Constraint 1 names; dropping the barrier would \
             remove a synchronisation the engine asked for, and a missing barrier is a data race \
             on the GPU that no validation layer on this machine would report. The guest's own \
             `vkCreateBuffer` is refused by name, so nothing it could legitimately hold can reach \
             this",
            caller = at.caller,
            array = args[7]
        )));
    }

    let memory_barriers = decode_memory_barriers(c, at, args[4] as u32, args[5])?;
    let image_barriers = decode_image_barriers(c, at, vulkan, image_count, image_array)?;

    host.cmd_pipeline_barrier(
        buffer,
        &PipelineBarrier {
            src_stage: args[1] as u32,
            dst_stage: args[2] as u32,
            dependency_flags: args[3] as u32,
            memory_barriers,
            image_barriers,
        },
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdClearColorImage(VkCommandBuffer commandBuffer, VkImage image,
/// VkImageLayout imageLayout, const VkClearColorValue *pColor, uint32_t rangeCount,
/// const VkImageSubresourceRange *pRanges)`
///
/// Six parameters, all in registers. The colour travels as its sixteen **bytes**;
/// [`VulkanHost::cmd_clear_color_image`](super::VulkanHost) says why this layer must not decide
/// which member of the union is live.
pub(super) fn cmd_clear_color_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdClearColorImage";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let image = vulkan.image_token(at, CALL, args[1])?;
    let layout = args[2] as u32;

    let colour_at = require_pointer(at, CALL, "pColor", args[3])?;
    let colour_bytes = c.mem().read_bytes(colour_at, CLEAR_COLOR_VALUE_BYTES, c.blame(3))?;
    let colour: [u8; CLEAR_COLOR_VALUE_BYTES] =
        colour_bytes.as_slice().try_into().expect("read_bytes answered the length it was asked");

    let count = args[4] as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `rangeCount = 0`. The specification \
             requires at least one range, and a clear of no subresources is a command that \
             records successfully and changes nothing -- so the frame presented would be whatever \
             the presentation engine last had in that image, with every call along the way \
             answering `VK_SUCCESS`",
            caller = at.caller
        )));
    }
    if count > MAX_CLEAR_RANGES {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `rangeCount = {count}`, and this \
             layer reads at most {MAX_CLEAR_RANGES}. The count is a guest `uint32_t` indexing an \
             array of {IMAGE_SUBRESOURCE_RANGE_BYTES}-byte structures, so honouring it unbounded \
             would be a guest-controlled host allocation (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let ranges_at = require_pointer(at, CALL, "pRanges", args[5])?;
    let bytes =
        c.mem().read_bytes(ranges_at, count * IMAGE_SUBRESOURCE_RANGE_BYTES, c.blame(5))?;
    let ranges: Vec<Vec<u8>> = (0..count)
        .map(|index| {
            bytes[index * IMAGE_SUBRESOURCE_RANGE_BYTES..][..IMAGE_SUBRESOURCE_RANGE_BYTES].to_vec()
        })
        .collect();

    host.cmd_clear_color_image(
        buffer,
        HostImageRef::Swapchain(image),
        layout,
        colour,
        &ranges,
    )?;
    c.ret().void();
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// Decode `pMemoryBarriers`, which contains no handle at all.
fn decode_memory_barriers(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    count: u32,
    array: u64,
) -> AbiResult<Vec<(u32, u32)>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = bounded(at, "vkCmdPipelineBarrier", "memoryBarrierCount", count)?;
    let array_at = require_pointer(at, "vkCmdPipelineBarrier", "pMemoryBarriers", array)?;
    let bytes = c.mem().read_bytes(array_at, count * MEMORY_BARRIER_BYTES, c.blame(5))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * MEMORY_BARRIER_BYTES..][..MEMORY_BARRIER_BYTES];
        let stype = u32::from_le_bytes(entry[0..4].try_into().expect("four"));
        if stype != STYPE_MEMORY_BARRIER {
            return Err(at.refuse(format!(
                "the guest's `pMemoryBarriers[{index}]` has `sType` {stype}, and \
                 `VK_STRUCTURE_TYPE_MEMORY_BARRIER` is {STYPE_MEMORY_BARRIER}. The two access \
                 masks would be read at offsets belonging to a different structure"
            )));
        }
        let next = u64::from_le_bytes(entry[8..16].try_into().expect("eight"));
        if next != 0 {
            return Err(at.refuse(format!(
                "the guest's `pMemoryBarriers[{index}]->pNext` is {next:#x}. A memory-barrier \
                 chain carries `VkSampleLocationsInfoEXT` and the external-memory barriers, and \
                 dropping it would record a different barrier from the one asked for"
            )));
        }
        out.push((
            u32::from_le_bytes(entry[16..20].try_into().expect("four")),
            u32::from_le_bytes(entry[20..24].try_into().expect("four")),
        ));
    }
    Ok(out)
}

/// Decode `pImageMemoryBarriers`, resolving each `image` through the registry.
fn decode_image_barriers(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    count: u32,
    array: u64,
) -> AbiResult<Vec<ImageBarrier>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = bounded(at, "vkCmdPipelineBarrier", "imageMemoryBarrierCount", count)?;
    // Blamed on argument 9, which is where AAPCS64 put it: the overflow area rather than a
    // register, and a refusal that said `x9` would send a reader looking at the wrong place.
    let array_at = require_pointer(at, "vkCmdPipelineBarrier", "pImageMemoryBarriers", array)?;
    let bytes = c.mem().read_bytes(array_at, count * IMAGE_MEMORY_BARRIER_BYTES, c.blame(9))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * IMAGE_MEMORY_BARRIER_BYTES..][..IMAGE_MEMORY_BARRIER_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
        let stype = u32_at(0);
        if stype != STYPE_IMAGE_MEMORY_BARRIER {
            return Err(at.refuse(format!(
                "the guest's `pImageMemoryBarriers[{index}]` has `sType` {stype}, and \
                 `VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER` is {STYPE_IMAGE_MEMORY_BARRIER} -- \
                 `VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER` is {STYPE_BUFFER_MEMORY_BARRIER} and \
                 is the one a caller is most likely to have put here by mistake. `image` would \
                 be read at byte 40, which in a `VkBufferMemoryBarrier` is the buffer's offset"
            )));
        }
        let next = u64::from_le_bytes(entry[8..16].try_into().expect("eight"));
        if next != 0 {
            return Err(at.refuse(format!(
                "the guest's `pImageMemoryBarriers[{index}]->pNext` is {next:#x}. An image \
                 barrier's chain carries `VkSampleLocationsInfoEXT` and the external-memory and \
                 queue-family-foreign barriers. Dropping it would record a transition with \
                 different semantics from the one asked for, and a wrong layout transition on \
                 this machine is undefined behaviour with no validation layer to report it"
            )));
        }
        let image = vulkan.image_token(at, "vkCmdPipelineBarrier", u64::from_le_bytes(
            entry[40..48].try_into().expect("eight"),
        ))?;
        out.push(ImageBarrier {
            src_access: u32_at(16),
            dst_access: u32_at(20),
            old_layout: u32_at(24),
            new_layout: u32_at(28),
            src_queue_family: u32_at(32),
            dst_queue_family: u32_at(36),
            image: HostImageRef::Swapchain(image),
            subresource_range: entry[48..48 + IMAGE_SUBRESOURCE_RANGE_BYTES].to_vec(),
        });
    }
    Ok(out)
}

/// A barrier count within [`MAX_BARRIERS`], or a refusal naming the field.
fn bounded(at: &Site, call: &str, field: &str, count: u32) -> AbiResult<usize> {
    let count = count as usize;
    if count > MAX_BARRIERS {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field} = {count}`, and this layer \
             reads at most {MAX_BARRIERS}. The count is a guest `uint32_t` indexing an array of \
             structures, so honouring it unbounded would be a guest-controlled host allocation \
             (Global Constraint 11). Thirty-two barriers of one kind in one call is already more \
             than any renderer this project has seen",
            caller = at.caller
        )));
    }
    Ok(count)
}

/// The refusal a `pCreateInfo` with the wrong `sType` produces.
fn wrong_stype(
    at: &Site,
    call: &str,
    name: &str,
    expected: u32,
    found: u32,
    consequence: &str,
) -> crate::error::AbiError {
    at.refuse(format!(
        "the guest called `{call}` from {caller:#x} with a structure whose `sType` is {found}, \
         and `{name}` is {expected}. {consequence}",
        caller = at.caller
    ))
}

/// The refusal a non-null `pNext` produces.
fn chain_refused(at: &Site, call: &str, next: u64, what: &str) -> crate::error::AbiError {
    at.refuse(format!(
        "the guest called `{call}` from {caller:#x} with `pNext = {next:#x}`. {what}. This layer \
         does not know those layouts, so it refuses and names the address for the next run to \
         decode",
        caller = at.caller
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure numbers are the ones the specification fixes**, with the padding a reader
    /// is likely to get wrong written out as arithmetic.
    #[test]
    fn the_command_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_COMMAND_POOL_CREATE_INFO, 39);
        assert_eq!(STYPE_COMMAND_BUFFER_ALLOCATE_INFO, 40);
        assert_eq!(STYPE_COMMAND_BUFFER_BEGIN_INFO, 42);
        assert_eq!(STYPE_BUFFER_MEMORY_BARRIER, 44);
        assert_eq!(STYPE_IMAGE_MEMORY_BARRIER, 45);
        assert_eq!(STYPE_MEMORY_BARRIER, 46);

        // `VkCommandPoolCreateInfo` is the one with NO padding after `flags`, because the member
        // after it is a `uint32_t` rather than a handle.
        assert_eq!(COMMAND_POOL_CREATE_INFO_BYTES, 24);
        assert_eq!(16 + 4 + 4, COMMAND_POOL_CREATE_INFO_BYTES, "flags at 16, family at 20");

        // `VkCommandBufferAllocateInfo`: the pool is a `uint64_t` at 16, so `level` is at 24.
        assert_eq!(COMMAND_BUFFER_ALLOCATE_INFO_BYTES, 32);
        assert_eq!(16 + 8, 24);

        // `VkCommandBufferBeginInfo`: `flags` at 16 then four of padding, pointer at 24.
        assert_eq!(COMMAND_BUFFER_BEGIN_INFO_BYTES, 32);

        assert_eq!(MEMORY_BARRIER_BYTES, 24);
        // `VkImageMemoryBarrier`: six `uint32_t`-sized members fill 16..40 exactly, so `image`
        // lands at 40 with no padding -- unlike every other structure in this stage.
        assert_eq!(16 + 6 * 4, 40);
        assert_eq!(40 + 8 + IMAGE_SUBRESOURCE_RANGE_BYTES, 68);
        assert_eq!(IMAGE_MEMORY_BARRIER_BYTES, 72, "68 padded to the alignment of 8");
        assert_eq!(BUFFER_MEMORY_BARRIER_BYTES, 56);
        assert_eq!(CLEAR_COLOR_VALUE_BYTES, 16, "four 32-bit components, whichever union member");
    }

    /// The allocation bounds are far above what any renderer asks for, and the largest read each
    /// permits stays small. Stated as the numbers, for [`device`](super::super::device)'s reason.
    #[test]
    fn the_command_bounds_are_above_what_any_renderer_asks_for() {
        assert_eq!(MAX_COMMAND_BUFFERS_PER_CALL, 32);
        assert_eq!(super::super::MAX_COMMAND_BUFFERS, 64);
        assert!(MAX_COMMAND_BUFFERS_PER_CALL < super::super::MAX_COMMAND_BUFFERS,
            "one call must not be able to fill the registry on its own");
        assert_eq!(MAX_BARRIERS, 32);
        assert_eq!(MAX_CLEAR_RANGES, 8);
        // The largest read each bound permits, in bytes -- all well under a page.
        assert_eq!(MAX_COMMAND_BUFFERS_PER_CALL * 8, 256);
        assert_eq!(MAX_BARRIERS * IMAGE_MEMORY_BARRIER_BYTES, 2304);
        assert_eq!(MAX_CLEAR_RANGES * IMAGE_SUBRESOURCE_RANGE_BYTES, 160);
    }
}
