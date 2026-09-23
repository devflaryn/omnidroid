//! **The thirteen `vkCmd*` calls that turn a command buffer into a drawn frame**, and
//! `vkCmdDispatch`, the one that runs a compute pipeline.
//!
//! # These are the commands that record nothing when they go wrong
//!
//! Stage 4's two recordings — a barrier and a clear — already had this property, and this module
//! has eleven more of it: **a `vkCmd*` call returns `void`**. There is no `VkResult`, the driver
//! reports a malformed recording at `vkEndCommandBuffer` at the earliest and at submission at the
//! latest, and there are no validation layers on this machine
//! (`docs/research/graphics-spike.md` §6). A handler that quietly dropped a bind or a draw would
//! produce a command buffer that begins, ends and submits with every code zero, and a frame that
//! is empty.
//!
//! So nothing here is optional and nothing here is best-effort. Every handle goes through its
//! registry, every count is bounded and refused above the bound rather than truncated — a
//! *truncated* vertex-buffer list is a plausible vertex-buffer list — and anything this layer
//! cannot decode is a refusal naming the argument.
//!
//! # The two handles that are the same Vulkan type
//!
//! `vkCmdCopyBufferToImage`'s destination may be either a swapchain image or one the guest
//! created, and both arrive as a bare `uint64_t`.
//! [`Vulkan::image_ref_token`](super::Vulkan::image_ref_token) is the lookup that accepts either
//! and answers *which* it found, as a [`HostImageRef`](super::HostImageRef) — so a host cannot
//! treat a swapchain image as a created one by mistake, and a handle from neither registry is a
//! refusal naming both.
//!
//! # `pClearValues` is a union and stays one
//!
//! [`VulkanHost::cmd_clear_color_image`](super::VulkanHost) makes the argument and
//! `vkCmdBeginRenderPass` is the same case: which member of a `VkClearValue` is live is decided by
//! the **attachment's format**, which this layer does not know, so the sixteen bytes travel whole
//! and the driver reads whichever one the format says. Interpreting them here would be this layer
//! choosing, and a depth-stencil clear read as a colour is a value that is plausible and wrong.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::RenderPassBegin;
use super::instance::{guest_pointer, require_pointer};
use super::shader::{read_u64_array, RECT_2D_BYTES, VIEWPORT_BYTES};
use super::{Site, Vulkan};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO`.
const STYPE_RENDER_PASS_BEGIN_INFO: u32 = 43;

/// `sizeof(VkRenderPassBeginInfo)`.
///
/// ```text
/// VkStructureType      sType;            //  0  (then 4 of padding)
/// const void          *pNext;            //  8
/// VkRenderPass         renderPass;       // 16  (a uint64_t)
/// VkFramebuffer        framebuffer;      // 24
/// VkRect2D             renderArea;       // 32  (16 bytes)
/// uint32_t             clearValueCount;  // 48  (then 4 of padding)
/// const VkClearValue  *pClearValues;     // 56
/// ```
pub const RENDER_PASS_BEGIN_INFO_BYTES: usize = 64;

/// `sizeof(VkClearValue)`: a union of a four-float colour, a four-`int32_t` colour, a
/// four-`uint32_t` colour, and a `{float, uint32_t}` depth-stencil. Sixteen bytes whichever member
/// is live, which is what lets it travel as bytes.
pub const CLEAR_VALUE_BYTES: usize = 16;

/// `sizeof(VkBufferCopy)`: `srcOffset`, `dstOffset`, `size`, all `VkDeviceSize`.
pub const BUFFER_COPY_BYTES: usize = 24;

/// `sizeof(VkBufferImageCopy)`.
///
/// ```text
/// VkDeviceSize              bufferOffset;       //  0
/// uint32_t                  bufferRowLength;    //  8
/// uint32_t                  bufferImageHeight;  // 12
/// VkImageSubresourceLayers  imageSubresource;   // 16  (four uint32_t, to 32)
/// VkOffset3D                imageOffset;        // 32  (three int32_t, to 44)
/// VkExtent3D                imageExtent;        // 44  (three uint32_t, to 56)
/// ```
pub const BUFFER_IMAGE_COPY_BYTES: usize = 56;

/// `sizeof(VkImageSubresourceLayers)`: `aspectMask`, `mipLevel`, `baseArrayLayer`, `layerCount`.
pub const IMAGE_SUBRESOURCE_LAYERS_BYTES: usize = 16;

/// `sizeof(VkImageCopy)`: `srcSubresource` 0, `srcOffset` 16, `dstSubresource` 28, `dstOffset`
/// 44, `extent` 56 -- all four-byte members, so no padding anywhere.
pub const IMAGE_COPY_BYTES: usize = 68;

// ------------------------------------------------------------------------------ the bounds

/// How many `VkClearValue`s one `vkCmdBeginRenderPass` reads. One per render-pass attachment.
pub const MAX_CLEAR_VALUES: usize = super::MAX_RENDER_PASS_ATTACHMENTS;

/// How many vertex buffers one `vkCmdBindVertexBuffers` binds.
pub const MAX_VERTEX_BUFFER_BINDINGS: usize = super::MAX_VERTEX_BINDINGS;

/// How many descriptor sets one `vkCmdBindDescriptorSets` binds.
///
/// `maxBoundDescriptorSets` is 4 on a minimum-conformant device and 32 on this one; sixteen is
/// above every engine's set count and is the same bound the pipeline layout uses, because binding
/// more sets than a layout can hold is a driver error rather than a layer one.
pub const MAX_BOUND_DESCRIPTOR_SETS: usize = super::MAX_SET_LAYOUTS;

/// How many dynamic offsets one `vkCmdBindDescriptorSets` carries.
pub const MAX_DYNAMIC_OFFSETS: usize = 64;

/// How many copy regions one `vkCmdCopyBuffer` or `vkCmdCopyBufferToImage` reads.
///
/// A mip chain is one region per level and an atlas upload is one per sub-image, so this is the
/// bound most likely to be reached by a real engine — and reaching it is a refusal naming the
/// constant rather than a copy that moves some of the texture.
pub const MAX_COPY_REGIONS: usize = 128;

/// How many bytes of push constants one `vkCmdPushConstants` writes.
///
/// `maxPushConstantsSize` is 128 on a minimum-conformant device and 256 on this one, and the
/// specification caps the range at that limit — so 256 is the largest a conforming guest can send
/// and the 257th byte is a refusal rather than a host allocation the guest chose the size of.
pub const MAX_PUSH_CONSTANT_BYTES: usize = 256;

// ------------------------------------------------------------------------------- the handlers

/// `void vkCmdBeginRenderPass(VkCommandBuffer commandBuffer,
/// const VkRenderPassBeginInfo *pRenderPassBegin, VkSubpassContents contents)`
pub(super) fn cmd_begin_render_pass(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdBeginRenderPass";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pRenderPassBegin", args[1])?;

    let info = c.mem().read_bytes(info_at, RENDER_PASS_BEGIN_INFO_BYTES, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != STYPE_RENDER_PASS_BEGIN_INFO {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with a `pRenderPassBegin` whose `sType` \
             is {stype}, and `VK_STRUCTURE_TYPE_RENDER_PASS_BEGIN_INFO` is \
             {STYPE_RENDER_PASS_BEGIN_INFO}. `renderPass` and `framebuffer` would be read at \
             offsets belonging to a different structure and looked up in two registries",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with \
             `pRenderPassBegin->pNext = {next:#x}`. A render-pass-begin chain carries \
             `VkRenderPassAttachmentBeginInfo`, which is **where an imageless framebuffer's \
             attachments come from** -- dropping it would begin a render pass with no attachments \
             at all -- and `VkDeviceGroupRenderPassBeginInfo`. This layer does not walk chains; \
             see `vkAllocateMemory`'s refusal for the argument",
            caller = at.caller
        )));
    }

    let render_pass = vulkan.render_pass_token(
        at,
        CALL,
        u64::from_le_bytes(info[16..24].try_into().expect("eight")),
    )?;
    let framebuffer = vulkan.framebuffer_token(
        at,
        CALL,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
    )?;
    let render_area = info[32..32 + RECT_2D_BYTES].to_vec();
    let count = u32::from_le_bytes(info[48..52].try_into().expect("four")) as usize;
    let pointer = u64::from_le_bytes(info[56..64].try_into().expect("eight"));

    let clear_values = if count == 0 || pointer == 0 {
        Vec::new()
    } else {
        if count > MAX_CLEAR_VALUES {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with `clearValueCount = {count}`, and \
                 this layer reads at most {MAX_CLEAR_VALUES} -- one per render-pass attachment. \
                 The count is a guest `uint32_t` indexing an array of \
                 {CLEAR_VALUE_BYTES}-byte unions (Global Constraint 11)",
                caller = at.caller
            )));
        }
        let array_at = guest_pointer(at, "pClearValues", pointer)?;
        let bytes = c.mem().read_bytes(array_at, count * CLEAR_VALUE_BYTES, c.blame(1))?;
        bytes
            .chunks_exact(CLEAR_VALUE_BYTES)
            .map(|chunk| <[u8; CLEAR_VALUE_BYTES]>::try_from(chunk).expect("sixteen"))
            .collect()
    };

    host.cmd_begin_render_pass(
        buffer,
        &RenderPassBegin {
            render_pass: Some(render_pass),
            framebuffer: Some(framebuffer),
            render_area,
            clear_values,
        },
        args[2] as u32,
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdEndRenderPass(VkCommandBuffer commandBuffer)`
pub(super) fn cmd_end_render_pass(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdEndRenderPass";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    host.cmd_end_render_pass(buffer)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdBindPipeline(VkCommandBuffer commandBuffer, VkPipelineBindPoint pipelineBindPoint,
/// VkPipeline pipeline)`
pub(super) fn cmd_bind_pipeline(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdBindPipeline";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let pipeline = vulkan.pipeline_token(at, CALL, args[2])?;
    host.cmd_bind_pipeline(buffer, args[1] as u32, pipeline)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdBindVertexBuffers(VkCommandBuffer commandBuffer, uint32_t firstBinding,
/// uint32_t bindingCount, const VkBuffer *pBuffers, const VkDeviceSize *pOffsets)`
///
/// The two arrays are **zipped** on the way through, for
/// [`SubmitRequest::waits`](super::SubmitRequest)' reason: the specification requires them to have
/// the same length, and a pair cannot come apart the way two `Vec`s can.
pub(super) fn cmd_bind_vertex_buffers(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdBindVertexBuffers";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let count = args[2] as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `bindingCount = 0`, which the \
             specification forbids. A bind of no buffers records successfully and leaves the \
             previous binding in place, so the draw would read whichever vertices the last one \
             pointed at",
            caller = at.caller
        )));
    }
    let handles = read_u64_array(
        c,
        at,
        CALL,
        "pBuffers",
        count,
        args[3],
        MAX_VERTEX_BUFFER_BINDINGS,
        3,
    )?;
    if handles.len() != count {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `bindingCount = {count}` and \
             `pBuffers = NULL`. The specification requires the array",
            caller = at.caller
        )));
    }
    let offsets_at = require_pointer(at, CALL, "pOffsets", args[4])?;
    let offset_bytes = c.mem().read_bytes(offsets_at, count * 8, c.blame(4))?;

    let mut buffers = Vec::with_capacity(count);
    for (index, handle) in handles.iter().enumerate() {
        buffers.push((
            vulkan.buffer_token(at, CALL, *handle)?,
            u64::from_le_bytes(offset_bytes[index * 8..][..8].try_into().expect("eight")),
        ));
    }
    host.cmd_bind_vertex_buffers(buffer, args[1] as u32, &buffers)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdBindIndexBuffer(VkCommandBuffer commandBuffer, VkBuffer buffer,
/// VkDeviceSize offset, VkIndexType indexType)`
pub(super) fn cmd_bind_index_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdBindIndexBuffer";
    let host = vulkan.require_host(at)?;
    let command = vulkan.command_buffer_token(at, CALL, args[0])?;
    let index_buffer = vulkan.buffer_token(at, CALL, args[1])?;
    host.cmd_bind_index_buffer(command, index_buffer, args[2], args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdBindDescriptorSets(VkCommandBuffer commandBuffer,
/// VkPipelineBindPoint pipelineBindPoint, VkPipelineLayout layout, uint32_t firstSet,
/// uint32_t descriptorSetCount, const VkDescriptorSet *pDescriptorSets,
/// uint32_t dynamicOffsetCount, const uint32_t *pDynamicOffsets)`
///
/// **Exactly eight parameters**, which is exactly `X0`-`X7`: the first call in this stage to fill
/// the register file to the last one without spilling. A ninth would be on the stack, which is the
/// case [`command::cmd_pipeline_barrier`](super::command) documents.
pub(super) fn cmd_bind_descriptor_sets(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdBindDescriptorSets";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let layout = vulkan.pipeline_layout_token(at, CALL, args[2])?;
    let count = args[4] as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `descriptorSetCount = 0`, which the \
             specification forbids. Nothing would be bound and the draw would read whichever \
             descriptors were bound before",
            caller = at.caller
        )));
    }
    let handles = read_u64_array(
        c,
        at,
        CALL,
        "pDescriptorSets",
        count,
        args[5],
        MAX_BOUND_DESCRIPTOR_SETS,
        5,
    )?;
    if handles.len() != count {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `descriptorSetCount = {count}` and \
             `pDescriptorSets = NULL`",
            caller = at.caller
        )));
    }
    let mut sets = Vec::with_capacity(count);
    for handle in &handles {
        sets.push(vulkan.descriptor_set_token(at, CALL, *handle)?);
    }

    let dynamic_count = args[6] as u32 as usize;
    let dynamic_offsets = if dynamic_count == 0 || args[7] == 0 {
        Vec::new()
    } else {
        if dynamic_count > MAX_DYNAMIC_OFFSETS {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with \
                 `dynamicOffsetCount = {dynamic_count}`, and this layer reads at most \
                 {MAX_DYNAMIC_OFFSETS}",
                caller = at.caller
            )));
        }
        let array_at = guest_pointer(at, "pDynamicOffsets", args[7])?;
        let bytes = c.mem().read_bytes(array_at, dynamic_count * 4, c.blame(7))?;
        (0..dynamic_count)
            .map(|index| u32::from_le_bytes(bytes[index * 4..][..4].try_into().expect("four")))
            .collect()
    };

    host.cmd_bind_descriptor_sets(
        buffer,
        args[1] as u32,
        layout,
        args[3] as u32,
        &sets,
        &dynamic_offsets,
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdSetViewport(VkCommandBuffer commandBuffer, uint32_t firstViewport,
/// uint32_t viewportCount, const VkViewport *pViewports)`
pub(super) fn cmd_set_viewport(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdSetViewport";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let viewports =
        read_dynamic_array(c, at, CALL, "pViewports", "VkViewport", VIEWPORT_BYTES, args[2], args[3])?;
    host.cmd_set_viewport(buffer, args[1] as u32, &viewports)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdSetScissor(VkCommandBuffer commandBuffer, uint32_t firstScissor,
/// uint32_t scissorCount, const VkRect2D *pScissors)`
pub(super) fn cmd_set_scissor(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdSetScissor";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let scissors =
        read_dynamic_array(c, at, CALL, "pScissors", "VkRect2D", RECT_2D_BYTES, args[2], args[3])?;
    host.cmd_set_scissor(buffer, args[1] as u32, &scissors)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdDraw(VkCommandBuffer commandBuffer, uint32_t vertexCount,
/// uint32_t instanceCount, uint32_t firstVertex, uint32_t firstInstance)`
pub(super) fn cmd_draw(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdDraw";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    host.cmd_draw(buffer, args[1] as u32, args[2] as u32, args[3] as u32, args[4] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdDrawIndexed(VkCommandBuffer commandBuffer, uint32_t indexCount,
/// uint32_t instanceCount, uint32_t firstIndex, int32_t vertexOffset, uint32_t firstInstance)`
///
/// `vertexOffset` is **signed**, and it is the only one of the five that is. Reading it as a `u32`
/// and widening would turn `-1` into 4,294,967,295 and index the vertex buffer four gigabytes past
/// its start — so the cast is written out rather than left to inference.
pub(super) fn cmd_draw_indexed(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdDrawIndexed";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    host.cmd_draw_indexed(
        buffer,
        args[1] as u32,
        args[2] as u32,
        args[3] as u32,
        args[4] as u32 as i32,
        args[5] as u32,
    )?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdDispatch(VkCommandBuffer commandBuffer, uint32_t groupCountX,
/// uint32_t groupCountY, uint32_t groupCountZ)`
///
/// MEASURED: the renderer's first, once its compute pipelines exist, is `(buffer, 5, 3, 1)`.
pub(super) fn cmd_dispatch(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdDispatch";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    host.cmd_dispatch(buffer, args[1] as u32, args[2] as u32, args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdCopyBuffer(VkCommandBuffer commandBuffer, VkBuffer srcBuffer, VkBuffer dstBuffer,
/// uint32_t regionCount, const VkBufferCopy *pRegions)`
pub(super) fn cmd_copy_buffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdCopyBuffer";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let source = vulkan.buffer_token(at, CALL, args[1])?;
    let destination = vulkan.buffer_token(at, CALL, args[2])?;
    let regions =
        read_regions(c, at, CALL, "pRegions", "VkBufferCopy", BUFFER_COPY_BYTES, args[3], args[4], 4)?;
    host.cmd_copy_buffer(buffer, source, destination, &regions)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdCopyBufferToImage(VkCommandBuffer commandBuffer, VkBuffer srcBuffer,
/// VkImage dstImage, VkImageLayout dstImageLayout, uint32_t regionCount,
/// const VkBufferImageCopy *pRegions)`
///
/// **The call that puts a texture on the GPU**, and the one whose image argument may be either
/// family; see this module's header.
pub(super) fn cmd_copy_buffer_to_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdCopyBufferToImage";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let source = vulkan.buffer_token(at, CALL, args[1])?;
    let image = vulkan.image_ref_token(at, CALL, args[2])?;
    let regions = read_regions(
        c,
        at,
        CALL,
        "pRegions",
        "VkBufferImageCopy",
        BUFFER_IMAGE_COPY_BYTES,
        args[4],
        args[5],
        5,
    )?;
    host.cmd_copy_buffer_to_image(buffer, source, image, args[3] as u32, &regions)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdCopyImage(VkCommandBuffer commandBuffer, VkImage srcImage,
/// VkImageLayout srcImageLayout, VkImage dstImage, VkImageLayout dstImageLayout,
/// uint32_t regionCount, const VkImageCopy *pRegions)`
///
/// Seven parameters, all in registers. MEASURED: the renderer's first, after its first dispatch,
/// copies one region from an image in `TRANSFER_SRC_OPTIMAL` to one in `TRANSFER_DST_OPTIMAL`.
/// **Either image may be of either family**, as `vkCmdCopyBufferToImage`'s destination may.
pub(super) fn cmd_copy_image(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdCopyImage";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let source = vulkan.image_ref_token(at, CALL, args[1])?;
    let destination = vulkan.image_ref_token(at, CALL, args[3])?;
    let regions =
        read_regions(c, at, CALL, "pRegions", "VkImageCopy", IMAGE_COPY_BYTES, args[5], args[6], 6)?;
    host.cmd_copy_image(buffer, source, args[2] as u32, destination, args[4] as u32, &regions)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdPushConstants(VkCommandBuffer commandBuffer, VkPipelineLayout layout,
/// VkShaderStageFlags stageFlags, uint32_t offset, uint32_t size, const void *pValues)`
pub(super) fn cmd_push_constants(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdPushConstants";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let layout = vulkan.pipeline_layout_token(at, CALL, args[1])?;
    let size = args[4] as u32 as usize;
    if size == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `size = 0`, which the specification \
             forbids. Nothing would be written and the shader would read whichever constants were \
             pushed before",
            caller = at.caller
        )));
    }
    if size > MAX_PUSH_CONSTANT_BYTES {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `size = {size}`, and this layer \
             reads at most {MAX_PUSH_CONSTANT_BYTES} -- which is this device's own \
             `maxPushConstantsSize` and the largest range the specification permits. A larger \
             size is a guest `uint32_t` choosing the length of a host read",
            caller = at.caller
        )));
    }
    let values_at = require_pointer(at, CALL, "pValues", args[5])?;
    let values = c.mem().read_bytes(values_at, size, c.blame(5))?;
    host.cmd_push_constants(buffer, layout, args[2] as u32, args[3] as u32, &values)?;
    c.ret().void();
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// Read a `vkCmdSetViewport`/`vkCmdSetScissor` array, whose count is in a register.
#[allow(clippy::too_many_arguments)]
fn read_dynamic_array(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    element: &str,
    element_bytes: usize,
    count: u64,
    pointer: u64,
) -> AbiResult<Vec<u8>> {
    let count = count as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with a count of zero, which the \
             specification forbids. A pipeline with this state dynamic has none until it is set, \
             and a command buffer that never sets it draws with whatever the hardware had",
            caller = at.caller
        )));
    }
    if count > super::MAX_VIEWPORTS {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with {count} `{element}` entries, and \
             this layer reads at most {bound}. The count is a guest `uint32_t` indexing an array \
             of {element_bytes}-byte structures (Global Constraint 11)",
            caller = at.caller,
            bound = super::MAX_VIEWPORTS
        )));
    }
    let array_at = require_pointer(at, call, field, pointer)?;
    c.mem().read_bytes(array_at, count * element_bytes, c.blame(3))
}

/// Read a copy-region array, whose elements carry no handle and travel as flat bytes.
#[allow(clippy::too_many_arguments)]
fn read_regions(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    element: &str,
    element_bytes: usize,
    count: u64,
    pointer: u64,
    argument: usize,
) -> AbiResult<Vec<u8>> {
    let count = count as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `regionCount = 0`, which the \
             specification forbids. A copy of no regions records successfully and moves nothing, \
             so a texture upload would leave the image holding whatever the driver put in it",
            caller = at.caller
        )));
    }
    if count > MAX_COPY_REGIONS {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `regionCount = {count}`, and this \
             layer reads at most {MAX_COPY_REGIONS}. The count is a guest `uint32_t` indexing an \
             array of {element_bytes}-byte `{element}` structures (Global Constraint 11). A mip \
             chain or an atlas upload is the case most likely to reach this, so raise \
             `omni_android::vulkan::MAX_COPY_REGIONS` rather than splitting the copy",
            caller = at.caller
        )));
    }
    let array_at = require_pointer(at, call, field, pointer)?;
    c.mem().read_bytes(array_at, count * element_bytes, c.blame(argument))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The structure sizes are the specification's**, with the arithmetic written out.
    #[test]
    fn the_recording_structure_numbers_are_the_specifications() {
        assert_eq!(STYPE_RENDER_PASS_BEGIN_INFO, 43);
        // `renderArea` is a 16-byte `VkRect2D` at 32, so `clearValueCount` lands at 48 and the
        // pointer after it at 56.
        assert_eq!(32 + RECT_2D_BYTES, 48);
        assert_eq!(RENDER_PASS_BEGIN_INFO_BYTES, 56 + 8);
        assert_eq!(CLEAR_VALUE_BYTES, 4 * 4, "four channels, whichever member is live");
        assert_eq!(BUFFER_COPY_BYTES, 3 * 8);
        // The image copy's three sub-structures, in the order they appear.
        assert_eq!(IMAGE_SUBRESOURCE_LAYERS_BYTES, 4 * 4);
        assert_eq!(16 + IMAGE_SUBRESOURCE_LAYERS_BYTES, 32, "imageOffset starts here");
        assert_eq!(32 + 3 * 4, 44, "and imageExtent here");
        assert_eq!(BUFFER_IMAGE_COPY_BYTES, 44 + 3 * 4);
        // Two subresources, two offsets and an extent, every member four bytes.
        assert_eq!(IMAGE_COPY_BYTES, 2 * IMAGE_SUBRESOURCE_LAYERS_BYTES + 3 * 3 * 4);
    }

    /// **`vertexOffset` is signed and stays signed.** A `u32` widened to `i32` the wrong way
    /// round turns `-1` into a four-gigabyte index into a vertex buffer, which is a GPU read of
    /// memory nobody gave it — and on this machine nothing would report it.
    #[test]
    fn a_negative_vertex_offset_survives_the_register() {
        // What the guest's `mov w4, #-1` leaves in `X4`, and what the handler must make of it.
        let register: u64 = 0xFFFF_FFFF;
        assert_eq!(register as u32 as i32, -1);
        assert_ne!(u64::from(register as u32), u64::MAX, "the guest wrote 32 bits, not 64");
        // The wrong reading, written out so the right one is visibly different.
        assert_eq!(register as u32, 4_294_967_295);
    }
}
