//! **Shaders, render passes, framebuffers and the graphics pipeline.**
//!
//! # SPIR-V needs no translation, and that is a fact rather than a convenience
//!
//! Every other thing that crosses this boundary is a structure whose layout depends on a target.
//! SPIR-V does not: the specification defines a module as a stream of 32-bit words in the
//! *module's own* endianness, which a consumer detects from the magic number, and Vulkan requires
//! `pCode` to be a `const uint32_t *` with the host's endianness. Both sides here are
//! little-endian, both are LP64/LLP64 with a four-byte `uint32_t`, and the bytes the guest
//! assembled **are** the bytes the driver compiles.
//!
//! So [`create_shader_module`] does exactly two things that matter: it validates the guest pointer
//! through [`GuestMem`](crate::GuestMem), because `pCode` is a guest address and `codeSize` is a
//! guest `size_t`; and it refuses a `codeSize` that is not a multiple of four, because the
//! specification requires it and a driver handed three and a half words reads one word past the
//! buffer. There is no transformation in between, and a reader looking for one should not find
//! one.
//!
//! # `vkCreateGraphicsPipelines` is the call rule 1 was written about
//!
//! A `vkCreateGraphicsPipelines` that answers `VK_SUCCESS` with no pipeline behind it is believed
//! — the guest binds the handle, records a draw with it and presents a frame that is empty, and
//! every `VkResult` along the way is zero. There is no shortcut version of this function in this
//! file. `VkGraphicsPipelineCreateInfo` is nine sub-state pointers, two handles, an array of
//! stages each carrying a handle and two more pointers, and a `pDynamicState` whose presence
//! changes what several of the others mean; all of it is decoded, and anything that cannot be is
//! a refusal naming the member.
//!
//! **It can also partly succeed**, which is unique among Vulkan's creation calls: the
//! specification requires `pPipelines` to hold `VK_NULL_HANDLE` for each pipeline that failed and
//! a valid handle for each that did not, *and* an error code to be returned, with the successful
//! ones still the application's to destroy. [`PipelinesCreated`](super::host::PipelinesCreated) is
//! the shape that lets that be said; a `DriverAnswer<Vec<_>>` would have forced the failure to be
//! total and leaked every pipeline the driver did create.
//!
//! # Which sub-states travel as bytes, and the rule that decides it
//!
//! [`physical`](super::physical)'s rule, applied member by member rather than to whole structures:
//! a run of scalars with no pointer and no handle in it crosses as its bytes, and anything holding
//! a pointer is decoded. `pRasterizationState` and `pDepthStencilState` are entirely scalars, so
//! their bodies cross whole — 44 and 88 bytes of `VkBool32`s, enums and floats, where decoding
//! would mean naming 33 fields in this crate and laying them out with this host's compiler while
//! making a claim about the guest's. The others each hold at least one array, so each is decoded
//! down to the arrays and those cross as bytes.
//!
//! # `None` is not a default
//!
//! Five members of a `VkGraphicsPipelineCreateInfo` are legitimately NULL. A pipeline with
//! `rasterizerDiscardEnable` set needs no viewport, multisample, depth-stencil or colour-blend
//! state at all, and one with no tessellation stages needs no tessellation state. Substituting a
//! zeroed structure for a NULL would create a pipeline that rasterizes where the engine asked for
//! one that does not — so each is an `Option` all the way to the driver, and the host passes NULL
//! back through.

use std::sync::Arc;

use omni_mem::GuestAddr;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{
    ColorBlendState, ComputePipelineRequest, DriverAnswer, FramebufferRequest,
    GraphicsPipelineRequest, HostPipelineCache,
    MultisampleState, PipelineLayoutRequest, RenderPassRequest, ShaderStage, Specialization,
    SubpassRequest, VertexInputState, ViewportState,
};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::resource::check_header;
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO`.
const STYPE_SHADER_MODULE_CREATE_INFO: u32 = 16;
/// `VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO`.
const STYPE_PIPELINE_CACHE_CREATE_INFO: u32 = 17;
/// `VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO`.
const STYPE_PIPELINE_SHADER_STAGE_CREATE_INFO: u32 = 18;
/// `VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO`.
const STYPE_VERTEX_INPUT_STATE: u32 = 19;
/// `VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO`.
const STYPE_INPUT_ASSEMBLY_STATE: u32 = 20;
/// `VK_STRUCTURE_TYPE_PIPELINE_TESSELLATION_STATE_CREATE_INFO`.
const STYPE_TESSELLATION_STATE: u32 = 21;
/// `VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO`.
const STYPE_VIEWPORT_STATE: u32 = 22;
/// `VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO`.
const STYPE_RASTERIZATION_STATE: u32 = 23;
/// `VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO`.
const STYPE_MULTISAMPLE_STATE: u32 = 24;
/// `VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO`.
const STYPE_DEPTH_STENCIL_STATE: u32 = 25;
/// `VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO`.
const STYPE_COLOR_BLEND_STATE: u32 = 26;
/// `VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO`.
const STYPE_DYNAMIC_STATE: u32 = 27;
/// `VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO`.
const STYPE_GRAPHICS_PIPELINE_CREATE_INFO: u32 = 28;
/// `VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO`.
const STYPE_COMPUTE_PIPELINE_CREATE_INFO: u32 = 29;
/// `VK_SHADER_STAGE_COMPUTE_BIT`, the one stage a compute pipeline has.
const SHADER_STAGE_COMPUTE: u32 = 0x20;
/// `VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO`.
const STYPE_PIPELINE_LAYOUT_CREATE_INFO: u32 = 30;
/// `VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO`.
const STYPE_FRAMEBUFFER_CREATE_INFO: u32 = 37;
/// `VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO`.
const STYPE_RENDER_PASS_CREATE_INFO: u32 = 38;

/// `sizeof(VkShaderModuleCreateInfo)`: header, `flags` (padded), `codeSize` (a `size_t`), `pCode`.
pub const SHADER_MODULE_CREATE_INFO_BYTES: usize = 40;

/// `sizeof(VkPipelineCacheCreateInfo)`: header, `flags` (padded), `initialDataSize`, `pInitialData`.
pub const PIPELINE_CACHE_CREATE_INFO_BYTES: usize = 40;

/// `sizeof(VkPipelineLayoutCreateInfo)`.
///
/// ```text
/// flags 16, setLayoutCount 20, pSetLayouts 24,
/// pushConstantRangeCount 32 (then 4 of padding), pPushConstantRanges 40
/// ```
pub const PIPELINE_LAYOUT_CREATE_INFO_BYTES: usize = 48;

/// `sizeof(VkPushConstantRange)`: `stageFlags`, `offset`, `size`.
pub const PUSH_CONSTANT_RANGE_BYTES: usize = 12;

/// `sizeof(VkRenderPassCreateInfo)`.
///
/// ```text
/// flags 16, attachmentCount 20, pAttachments 24,
/// subpassCount 32, pSubpasses 40, dependencyCount 48, pDependencies 56
/// ```
pub const RENDER_PASS_CREATE_INFO_BYTES: usize = 64;

/// `sizeof(VkAttachmentDescription)`: nine `uint32_t`-sized members, no padding.
pub const ATTACHMENT_DESCRIPTION_BYTES: usize = 36;

/// `sizeof(VkAttachmentReference)`: `attachment` and `layout`.
pub const ATTACHMENT_REFERENCE_BYTES: usize = 8;

/// `sizeof(VkSubpassDescription)`.
///
/// ```text
/// flags 0, pipelineBindPoint 4, inputAttachmentCount 8, pInputAttachments 16,
/// colorAttachmentCount 24, pColorAttachments 32, pResolveAttachments 40,
/// pDepthStencilAttachment 48, preserveAttachmentCount 56, pPreserveAttachments 64
/// ```
pub const SUBPASS_DESCRIPTION_BYTES: usize = 72;

/// `sizeof(VkSubpassDependency)`: seven `uint32_t`s.
pub const SUBPASS_DEPENDENCY_BYTES: usize = 28;

/// `sizeof(VkFramebufferCreateInfo)`.
///
/// ```text
/// flags 16 (then 4 of padding), renderPass 24, attachmentCount 32 (then 4),
/// pAttachments 40, width 48, height 52, layers 56 (then 4, alignment 8)
/// ```
pub const FRAMEBUFFER_CREATE_INFO_BYTES: usize = 64;

/// `sizeof(VkGraphicsPipelineCreateInfo)`.
///
/// ```text
/// flags 16, stageCount 20, pStages 24, pVertexInputState 32, pInputAssemblyState 40,
/// pTessellationState 48, pViewportState 56, pRasterizationState 64, pMultisampleState 72,
/// pDepthStencilState 80, pColorBlendState 88, pDynamicState 96, layout 104, renderPass 112,
/// subpass 120 (then 4 of padding), basePipelineHandle 128, basePipelineIndex 136 (then 4)
/// ```
pub const GRAPHICS_PIPELINE_CREATE_INFO_BYTES: usize = 144;

/// `sizeof(VkComputePipelineCreateInfo)`: `flags` 16 (then 4 of padding), the whole
/// `VkPipelineShaderStageCreateInfo` `stage` **embedded** at 24, `layout` 72,
/// `basePipelineHandle` 80, `basePipelineIndex` 88 (then 4).
pub const COMPUTE_PIPELINE_CREATE_INFO_BYTES: usize = 96;

/// Where a `VkComputePipelineCreateInfo`'s embedded `stage` starts.
const COMPUTE_STAGE_OFFSET: usize = 24;

/// `sizeof(VkPipelineShaderStageCreateInfo)`: `flags` 16, `stage` 20, `module` 24, `pName` 32,
/// `pSpecializationInfo` 40.
pub const PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES: usize = 48;

/// `sizeof(VkSpecializationInfo)`: `mapEntryCount` 0, `pMapEntries` 8, `dataSize` 16, `pData` 24.
pub const SPECIALIZATION_INFO_BYTES: usize = 32;

/// `sizeof(VkSpecializationMapEntry)`: `constantID` 0, `offset` 4, `size` 8 (a `size_t`).
pub const SPECIALIZATION_MAP_ENTRY_BYTES: usize = 16;

/// `sizeof(VkPipelineVertexInputStateCreateInfo)`.
pub const VERTEX_INPUT_STATE_BYTES: usize = 48;

/// `sizeof(VkVertexInputBindingDescription)`: `binding`, `stride`, `inputRate`.
pub const VERTEX_INPUT_BINDING_BYTES: usize = 12;

/// `sizeof(VkVertexInputAttributeDescription)`: `location`, `binding`, `format`, `offset`.
pub const VERTEX_INPUT_ATTRIBUTE_BYTES: usize = 16;

/// `sizeof(VkPipelineInputAssemblyStateCreateInfo)`: `flags` 16, `topology` 20,
/// `primitiveRestartEnable` 24, padded to 32.
pub const INPUT_ASSEMBLY_STATE_BYTES: usize = 32;

/// `sizeof(VkPipelineTessellationStateCreateInfo)`: `flags` 16, `patchControlPoints` 20.
pub const TESSELLATION_STATE_BYTES: usize = 24;

/// `sizeof(VkPipelineViewportStateCreateInfo)`.
pub const VIEWPORT_STATE_BYTES: usize = 48;

/// `sizeof(VkViewport)`: `x`, `y`, `width`, `height`, `minDepth`, `maxDepth`.
pub const VIEWPORT_BYTES: usize = 24;

/// `sizeof(VkRect2D)`: a `VkOffset2D` of two `int32_t` and a `VkExtent2D` of two `uint32_t`.
pub const RECT_2D_BYTES: usize = 16;

/// `sizeof(VkPipelineRasterizationStateCreateInfo)`, whose last member ends at 60 and which is
/// padded to its own alignment of 8.
pub const RASTERIZATION_STATE_BYTES: usize = 64;

/// The part of it that crosses the seam: the eleven scalars after `pNext`, **without** the tail
/// padding. Carrying the padding would make a host that reconstructs field by field disagree with
/// one that memcpys, for bytes neither of them means.
pub const RASTERIZATION_STATE_BODY_BYTES: usize = 44;

/// `sizeof(VkPipelineMultisampleStateCreateInfo)`.
pub const MULTISAMPLE_STATE_BYTES: usize = 48;

/// `sizeof(VkPipelineDepthStencilStateCreateInfo)`: five `VkBool32`/enum, two 28-byte
/// `VkStencilOpState`s, and two floats.
pub const DEPTH_STENCIL_STATE_BYTES: usize = 104;

/// Its body after `pNext`: 22 four-byte members, with no padding anywhere in them.
pub const DEPTH_STENCIL_STATE_BODY_BYTES: usize = 88;

/// `sizeof(VkPipelineColorBlendStateCreateInfo)`.
pub const COLOR_BLEND_STATE_BYTES: usize = 56;

/// `sizeof(VkPipelineColorBlendAttachmentState)`: eight `uint32_t`-sized members.
pub const COLOR_BLEND_ATTACHMENT_BYTES: usize = 32;

/// `sizeof(VkPipelineDynamicStateCreateInfo)`: `flags` 16, `dynamicStateCount` 20,
/// `pDynamicStates` 24.
pub const DYNAMIC_STATE_CREATE_INFO_BYTES: usize = 32;

// ------------------------------------------------------------------------------ the bounds
//
// Every one of these bounds a guest `uint32_t` that indexes an array. They are allocation bounds
// and not claims about Vulkan: reaching one is a refusal naming the constant, never a truncated
// list, because a truncated pipeline description is a *plausible* pipeline description.

/// How many bytes of SPIR-V one `vkCreateShaderModule` will read.
///
/// **Measured rather than chosen.** D8 records that Roblox ships 1,364 SPIR-V modules; the largest
/// shader module anything in this project has seen is far under a megabyte, and four is room with
/// a factor in hand. `codeSize` is a guest `size_t`, so an unbounded one is a guest-controlled
/// host allocation of any size it likes.
pub const MAX_SHADER_CODE_BYTES: usize = 4 * 1024 * 1024;

/// How many bytes of pipeline-cache blob one `vkCreatePipelineCache` will read.
pub const MAX_PIPELINE_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// How many `VkDescriptorSetLayout`s one pipeline layout will be built from.
///
/// `maxBoundDescriptorSets` is 4 on the minimum-conformant device and 32 on this one; eight covers
/// every engine's set layout and the ninth is a refusal naming this constant.
pub const MAX_SET_LAYOUTS: usize = 16;

/// How many `VkPushConstantRange`s one pipeline layout will be built from.
pub const MAX_PUSH_CONSTANT_RANGES: usize = 16;

/// How many attachments one render pass describes.
pub const MAX_RENDER_PASS_ATTACHMENTS: usize = 16;

/// How many subpasses one render pass describes.
pub const MAX_SUBPASSES: usize = 16;

/// How many dependencies one render pass describes.
pub const MAX_SUBPASS_DEPENDENCIES: usize = 32;

/// How many attachment references one subpass names in any one of its four lists.
pub const MAX_SUBPASS_REFERENCES: usize = 16;

/// How many attachments one framebuffer is built from. One per render-pass attachment.
pub const MAX_FRAMEBUFFER_ATTACHMENTS: usize = MAX_RENDER_PASS_ATTACHMENTS;

/// How many pipelines one `vkCreateGraphicsPipelines` will create.
///
/// An engine batches its pipelines, and a batch is what the call exists for — but each one is a
/// 144-byte structure with nine more behind it, so the batch is bounded. Sixteen is above what any
/// single bring-up batch in this project has seen.
pub const MAX_PIPELINES_PER_CALL: usize = 16;

/// How many shader stages one pipeline has. Vulkan 1.0 has five graphics stages; eight is room.
pub const MAX_PIPELINE_STAGES: usize = 8;

/// How many vertex bindings one pipeline declares.
pub const MAX_VERTEX_BINDINGS: usize = 32;

/// How many vertex attributes one pipeline declares.
pub const MAX_VERTEX_ATTRIBUTES: usize = 64;

/// How many viewports or scissors one pipeline or one `vkCmdSetViewport` names.
pub const MAX_VIEWPORTS: usize = 16;

/// How many colour-blend attachments one pipeline declares. One per colour attachment.
pub const MAX_BLEND_ATTACHMENTS: usize = MAX_SUBPASS_REFERENCES;

/// How many dynamic states one pipeline declares. Vulkan 1.0 has nine; 64 leaves room for the
/// extension states an engine may name on a driver that has them.
pub const MAX_DYNAMIC_STATES: usize = 64;

/// How many specialization constants one shader stage declares.
pub const MAX_SPECIALIZATION_ENTRIES: usize = 64;

/// How many bytes of specialization data one shader stage carries.
pub const MAX_SPECIALIZATION_BYTES: usize = 4096;

/// How many words of `pSampleMask` one pipeline carries: `ceil(rasterizationSamples / 32)`, and
/// `VK_SAMPLE_COUNT_64_BIT` is the largest there is.
pub const MAX_SAMPLE_MASK_WORDS: usize = 2;

/// How long a shader entry-point name may be.
pub const MAX_ENTRY_POINT_BYTES: usize = 256;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateShaderModule(VkDevice device, const VkShaderModuleCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkShaderModule *pShaderModule)`
pub(super) fn create_shader_module(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateShaderModule";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pShaderModule", args[3])?;

    let info = c.mem().read_bytes(info_at, SHADER_MODULE_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_SHADER_MODULE_CREATE_INFO,
        "VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO",
        "`codeSize` and `pCode` would be read at offsets belonging to a different structure, and \
         this layer would then read that many bytes of guest memory through that pointer",
        "a shader-module `pNext` chain carries `VkShaderModuleValidationCacheCreateInfoEXT`",
    )?;
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let code_size = u64::from_le_bytes(info[24..32].try_into().expect("eight"));
    let code_pointer = u64::from_le_bytes(info[32..40].try_into().expect("eight"));

    if code_size == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `codeSize = 0`. A module with no \
             words has no entry point, and a pipeline built from it would have a stage that \
             compiles nothing",
            caller = at.caller
        )));
    }
    if code_size % 4 != 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `codeSize = {code_size}`, which is \
             not a multiple of four. `pCode` is a `const uint32_t *` and the specification \
             requires the size to be a whole number of words -- a driver handed this would read \
             {over} byte(s) past the end of the guest's buffer while assembling its last word",
            caller = at.caller,
            over = 4 - (code_size % 4)
        )));
    }
    let code_size = usize::try_from(code_size).map_err(|_| {
        at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `codeSize = {code_size}`, which does \
             not fit this host's address space",
            caller = at.caller
        ))
    })?;
    if code_size > MAX_SHADER_CODE_BYTES {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `codeSize = {code_size}`, and this \
             layer reads at most {MAX_SHADER_CODE_BYTES}. `codeSize` is a guest `size_t`, so \
             honouring it unbounded would be a guest-controlled host allocation of any size the \
             guest chose (Global Constraint 11). Raise \
             `omni_android::vulkan::MAX_SHADER_CODE_BYTES` if a real module is larger",
            caller = at.caller
        )));
    }
    let code_at = guest_pointer(at, "pCode", code_pointer)?;
    if code_at == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `pCode = NULL` and \
             `codeSize = {code_size}`. There is no SPIR-V to compile",
            caller = at.caller
        )));
    }
    // **The one read that matters, and it goes through `admit` like every other.** The bytes are
    // not transformed; see this module's header for why there is nothing here to translate.
    let code = c.mem().read_bytes(code_at, code_size, c.blame(1))?;

    match host.create_shader_module(device, flags, &code)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_shader_module(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyShaderModule(VkDevice device, VkShaderModule shaderModule,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_shader_module(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyShaderModule";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "shaderModule", args[1])?;
    let token = vulkan.shader_module_token(at, CALL, args[1])?;
    host.destroy_shader_module(token)?;
    vulkan.forget_shader_module(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreatePipelineCache(VkDevice device, const VkPipelineCacheCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkPipelineCache *pPipelineCache)`
///
/// **The blob is opaque and stays opaque.** A pipeline cache's contents are a driver's own format
/// with a header the driver validates; this layer copies the guest's bytes through `admit` and
/// hands them over, and a blob that came from another driver is rejected *by that driver*, which
/// is exactly what the header is for.
pub(super) fn create_pipeline_cache(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreatePipelineCache";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pPipelineCache", args[3])?;

    let info = c.mem().read_bytes(info_at, PIPELINE_CACHE_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_PIPELINE_CACHE_CREATE_INFO,
        "VK_STRUCTURE_TYPE_PIPELINE_CACHE_CREATE_INFO",
        "`initialDataSize` and `pInitialData` would be read at offsets belonging to a different \
         structure",
        "a pipeline-cache `pNext` chain is where `VK_EXT_pipeline_creation_cache_control`'s \
         structures go",
    )?;
    let flags = u32::from_le_bytes(info[16..20].try_into().expect("four"));
    let size = u64::from_le_bytes(info[24..32].try_into().expect("eight"));
    let pointer = u64::from_le_bytes(info[32..40].try_into().expect("eight"));

    let initial = if size == 0 || pointer == 0 {
        Vec::new()
    } else {
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        if size > MAX_PIPELINE_CACHE_BYTES {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with \
                 `initialDataSize = {size}`, and this layer reads at most \
                 {MAX_PIPELINE_CACHE_BYTES}. The size is a guest `size_t`",
                caller = at.caller
            )));
        }
        let data_at = guest_pointer(at, "pInitialData", pointer)?;
        c.mem().read_bytes(data_at, size, c.blame(1))?
    };

    match host.create_pipeline_cache(device, flags, &initial)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_pipeline_cache(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyPipelineCache(VkDevice device, VkPipelineCache pipelineCache,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_pipeline_cache(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyPipelineCache";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "pipelineCache", args[1])?;
    let token = vulkan.pipeline_cache_token(at, CALL, args[1])?;
    host.destroy_pipeline_cache(token)?;
    vulkan.forget_pipeline_cache(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreatePipelineLayout(VkDevice device,
/// const VkPipelineLayoutCreateInfo *pCreateInfo, const VkAllocationCallbacks *pAllocator,
/// VkPipelineLayout *pPipelineLayout)`
pub(super) fn create_pipeline_layout(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreatePipelineLayout";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pPipelineLayout", args[3])?;

    let info = c.mem().read_bytes(info_at, PIPELINE_LAYOUT_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_PIPELINE_LAYOUT_CREATE_INFO,
        "VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO",
        "`pSetLayouts` would be read at an offset belonging to a different structure, and this \
         layer would then look those values up in its descriptor-set-layout registry",
        "a pipeline-layout `pNext` chain is where `VK_EXT_graphics_pipeline_library`'s structures \
         go",
    )?;
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));

    let layout_handles = read_u64_array(
        c,
        at,
        CALL,
        "pSetLayouts",
        u32_at(20) as usize,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
        MAX_SET_LAYOUTS,
        1,
    )?;
    let mut set_layouts = Vec::with_capacity(layout_handles.len());
    for handle in &layout_handles {
        set_layouts.push(vulkan.descriptor_set_layout_token(at, CALL, *handle)?);
    }

    let ranges = read_blob_array(
        c,
        at,
        CALL,
        "pPushConstantRanges",
        "VkPushConstantRange",
        PUSH_CONSTANT_RANGE_BYTES,
        u32_at(32) as usize,
        u64::from_le_bytes(info[40..48].try_into().expect("eight")),
        MAX_PUSH_CONSTANT_RANGES,
        1,
    )?;

    let request =
        PipelineLayoutRequest { flags: u32_at(16), set_layouts, push_constant_ranges: ranges };
    match host.create_pipeline_layout(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_pipeline_layout(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyPipelineLayout(VkDevice device, VkPipelineLayout pipelineLayout,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_pipeline_layout(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyPipelineLayout";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "pipelineLayout", args[1])?;
    let token = vulkan.pipeline_layout_token(at, CALL, args[1])?;
    host.destroy_pipeline_layout(token)?;
    vulkan.forget_pipeline_layout(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateRenderPass(VkDevice device, const VkRenderPassCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkRenderPass *pRenderPass)`
pub(super) fn create_render_pass(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateRenderPass";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pRenderPass", args[3])?;

    let info = c.mem().read_bytes(info_at, RENDER_PASS_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_RENDER_PASS_CREATE_INFO,
        "VK_STRUCTURE_TYPE_RENDER_PASS_CREATE_INFO",
        "`pAttachments` and `pSubpasses` would be read at offsets belonging to a different \
         structure, and a render pass whose attachment list came from elsewhere decides what \
         happens to the swapchain image before every draw",
        "a render-pass `pNext` chain carries `VkRenderPassMultiviewCreateInfo` and \
         `VkRenderPassInputAttachmentAspectCreateInfo`, both of which change what the subpasses \
         mean rather than adding to them",
    )?;
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));

    let attachments = read_blob_array(
        c,
        at,
        CALL,
        "pAttachments",
        "VkAttachmentDescription",
        ATTACHMENT_DESCRIPTION_BYTES,
        u32_at(20) as usize,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
        MAX_RENDER_PASS_ATTACHMENTS,
        1,
    )?;
    let dependencies = read_blob_array(
        c,
        at,
        CALL,
        "pDependencies",
        "VkSubpassDependency",
        SUBPASS_DEPENDENCY_BYTES,
        u32_at(48) as usize,
        u64::from_le_bytes(info[56..64].try_into().expect("eight")),
        MAX_SUBPASS_DEPENDENCIES,
        1,
    )?;
    let subpasses = decode_subpasses(
        c,
        at,
        CALL,
        u32_at(32) as usize,
        u64::from_le_bytes(info[40..48].try_into().expect("eight")),
    )?;
    if subpasses.is_empty() {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `subpassCount = 0`, which the \
             specification forbids. A render pass with no subpass can be begun and ended and \
             draws nothing, which is a frame that renders nothing with every `VkResult` zero",
            caller = at.caller
        )));
    }

    let request = RenderPassRequest { flags: u32_at(16), attachments, subpasses, dependencies };
    match host.create_render_pass(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_render_pass(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyRenderPass(VkDevice device, VkRenderPass renderPass,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_render_pass(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyRenderPass";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "renderPass", args[1])?;
    let token = vulkan.render_pass_token(at, CALL, args[1])?;
    host.destroy_render_pass(token)?;
    vulkan.forget_render_pass(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateFramebuffer(VkDevice device, const VkFramebufferCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkFramebuffer *pFramebuffer)`
pub(super) fn create_framebuffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateFramebuffer";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pFramebuffer", args[3])?;

    let info = c.mem().read_bytes(info_at, FRAMEBUFFER_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_FRAMEBUFFER_CREATE_INFO,
        "VK_STRUCTURE_TYPE_FRAMEBUFFER_CREATE_INFO",
        "`renderPass` and `pAttachments` would be read at offsets belonging to a different \
         structure, and this layer would then look those values up in two registries",
        "a framebuffer `pNext` chain carries `VkFramebufferAttachmentsCreateInfo`, which is what \
         an imageless framebuffer is -- a framebuffer with *no* attachments, whose `pAttachments` \
         is supplied at `vkCmdBeginRenderPass` instead. Dropping it would create an ordinary \
         framebuffer with no attachments at all",
    )?;
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));

    let render_pass = vulkan.render_pass_token(
        at,
        CALL,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
    )?;
    let handles = read_u64_array(
        c,
        at,
        CALL,
        "pAttachments",
        u32_at(32) as usize,
        u64::from_le_bytes(info[40..48].try_into().expect("eight")),
        MAX_FRAMEBUFFER_ATTACHMENTS,
        1,
    )?;
    let mut attachments = Vec::with_capacity(handles.len());
    for handle in &handles {
        attachments.push(vulkan.image_view_token(at, CALL, *handle)?);
    }

    let request = FramebufferRequest {
        flags: u32_at(16),
        render_pass: Some(render_pass),
        attachments,
        width: u32_at(48),
        height: u32_at(52),
        layers: u32_at(56),
    };
    match host.create_framebuffer(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_framebuffer(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyFramebuffer(VkDevice device, VkFramebuffer framebuffer,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_framebuffer(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyFramebuffer";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "framebuffer", args[1])?;
    let token = vulkan.framebuffer_token(at, CALL, args[1])?;
    host.destroy_framebuffer(token)?;
    vulkan.forget_framebuffer(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateGraphicsPipelines(VkDevice device, VkPipelineCache pipelineCache,
/// uint32_t createInfoCount, const VkGraphicsPipelineCreateInfo *pCreateInfos,
/// const VkAllocationCallbacks *pAllocator, VkPipeline *pPipelines)`
///
/// Six parameters, all in registers. See this module's header for the partial-success rule and why
/// the answer is a [`PipelinesCreated`](super::host::PipelinesCreated) rather than a
/// `DriverAnswer`.
pub(super) fn create_graphics_pipelines(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateGraphicsPipelines";
    refuse_allocator(vulkan, at, CALL, args[4])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let cache: Option<HostPipelineCache> = if args[1] == 0 {
        None
    } else {
        Some(vulkan.pipeline_cache_token(at, CALL, args[1])?)
    };
    let count = args[2] as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `createInfoCount = 0`. Nothing would \
             be created and nothing would be written into `pPipelines`, so the guest would read \
             whatever was already in its own array as a `VkPipeline` and bind it",
            caller = at.caller
        )));
    }
    if count > MAX_PIPELINES_PER_CALL {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `createInfoCount = {count}`, and \
             this layer creates at most {MAX_PIPELINES_PER_CALL} in one call. Each is a \
             {GRAPHICS_PIPELINE_CREATE_INFO_BYTES}-byte structure with nine more behind it, so an \
             unbounded count is a guest-controlled host allocation (Global Constraint 11). Raise \
             `omni_android::vulkan::MAX_PIPELINES_PER_CALL`",
            caller = at.caller
        )));
    }
    let infos_at = require_pointer(at, CALL, "pCreateInfos", args[3])?;
    let out_at = require_pointer(at, CALL, "pPipelines", args[5])?;

    let infos =
        c.mem().read_bytes(infos_at, count * GRAPHICS_PIPELINE_CREATE_INFO_BYTES, c.blame(3))?;
    let mut requests = Vec::with_capacity(count);
    for index in 0..count {
        let info = &infos[index * GRAPHICS_PIPELINE_CREATE_INFO_BYTES..]
            [..GRAPHICS_PIPELINE_CREATE_INFO_BYTES];
        requests.push(decode_pipeline(c, at, vulkan, CALL, index, info)?);
    }

    let created = host.create_graphics_pipelines(device, cache, &requests)?;
    if created.pipelines.len() != count {
        return Err(at.refuse(format!(
            "the host answered `{CALL}` with {answered} pipeline slot(s) for {count} create info \
             structure(s). The guest's `pPipelines` array has exactly {count} entries and the \
             specification requires one written per create info, so neither writing fewer -- \
             which leaves the guest reading its own uninitialised memory as a handle -- nor \
             writing more, which is a host write past the end of a guest buffer, is available",
            answered = created.pipelines.len()
        )));
    }

    // **Written even on failure**, because that is what the specification requires and what the
    // guest's own clean-up loop reads: `VK_NULL_HANDLE` for each pipeline that was not created,
    // and a real handle for each that was.
    let mut handles = Vec::with_capacity(count * 8);
    for token in &created.pipelines {
        match token {
            None => handles.extend_from_slice(&0u64.to_le_bytes()),
            Some(token) => {
                let registered = vulkan.register_pipeline(at, *token)?;
                c.mem().write_bytes(registered.at, &registered.image, c.blame(5))?;
                handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
            }
        }
    }
    c.mem().write_bytes(out_at, &handles, c.blame(5))?;
    if created.result != VK_SUCCESS {
        vulkan.note_driver_result(CALL, created.result);
    }
    c.ret().i32(created.result);
    Ok(())
}

/// `VkResult vkCreateComputePipelines(VkDevice device, VkPipelineCache pipelineCache,
/// uint32_t createInfoCount, const VkComputePipelineCreateInfo *pCreateInfos,
/// const VkAllocationCallbacks *pAllocator, VkPipeline *pPipelines)`
///
/// MEASURED: the engine's renderer, once its descriptor update templates exist, creates its compute
/// pipelines one per call -- `createInfoCount = 1`, a create info on its own stack with no `pNext`
/// anywhere, one `VK_SHADER_STAGE_COMPUTE_BIT` stage with no specialization, a module and a layout
/// it made, and no base pipeline. The partial-success rule and its answer are the graphics call's.
///
/// The embedded stage is decoded by the graphics call's own stage decoder, as a one-element
/// `pStages` at the stage's address, so every check a graphics stage gets it gets too -- and a
/// refusal about it names it `pStages[0]`.
pub(super) fn create_compute_pipelines(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateComputePipelines";
    refuse_allocator(vulkan, at, CALL, args[4])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let cache: Option<HostPipelineCache> = if args[1] == 0 {
        None
    } else {
        Some(vulkan.pipeline_cache_token(at, CALL, args[1])?)
    };
    let count = args[2] as u32 as usize;
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `createInfoCount = 0`. Nothing would \
             be created and nothing would be written into `pPipelines`, so the guest would read \
             whatever was already in its own array as a `VkPipeline` and bind it",
            caller = at.caller
        )));
    }
    if count > MAX_PIPELINES_PER_CALL {
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with `createInfoCount = {count}`, and \
             this layer creates at most {MAX_PIPELINES_PER_CALL} in one call, which bounds the \
             host allocation a guest count controls (Global Constraint 11). Raise \
             `omni_android::vulkan::MAX_PIPELINES_PER_CALL`",
            caller = at.caller
        )));
    }
    let infos_at = require_pointer(at, CALL, "pCreateInfos", args[3])?;
    let out_at = require_pointer(at, CALL, "pPipelines", args[5])?;

    let infos =
        c.mem().read_bytes(infos_at, count * COMPUTE_PIPELINE_CREATE_INFO_BYTES, c.blame(3))?;
    let mut requests = Vec::with_capacity(count);
    for index in 0..count {
        let info = &infos[index * COMPUTE_PIPELINE_CREATE_INFO_BYTES..]
            [..COMPUTE_PIPELINE_CREATE_INFO_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
        let u64_at =
            |offset: usize| u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight"));
        let stype = u32_at(0);
        if stype != STYPE_COMPUTE_PIPELINE_CREATE_INFO {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} and `pCreateInfos[{index}].sType` is \
                 {stype}, where `VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO` is \
                 {STYPE_COMPUTE_PIPELINE_CREATE_INFO}. The stage, the layout and the base \
                 pipeline would be read at offsets belonging to a different structure",
                caller = at.caller
            )));
        }
        if u64_at(8) != 0 {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with \
                 `pCreateInfos[{index}].pNext = {next:#x}`. A compute-pipeline chain carries \
                 creation feedback, robustness and subgroup controls, each of which changes what \
                 is compiled or what the guest reads back; none has been measured here, and this \
                 layer does not drop a chain it has not read",
                caller = at.caller,
                next = u64_at(8)
            )));
        }
        let stage_at = infos_at as u64 + (index * COMPUTE_PIPELINE_CREATE_INFO_BYTES + COMPUTE_STAGE_OFFSET) as u64;
        let stage = decode_stages(c, at, vulkan, CALL, 1, stage_at)?
            .pop()
            .expect("decode_stages answers one stage for a count of one");
        if stage.stage != SHADER_STAGE_COMPUTE {
            return Err(at.refuse(format!(
                "the guest called `{CALL}` from {caller:#x} with \
                 `pCreateInfos[{index}].stage.stage = {bits:#x}`. The specification requires \
                 `VK_SHADER_STAGE_COMPUTE_BIT` ({SHADER_STAGE_COMPUTE:#x}) there, and a driver \
                 handed another stage's bit for a compute pipeline is undefined behaviour with no \
                 validation layer to say so",
                caller = at.caller,
                bits = stage.stage
            )));
        }
        let layout = vulkan.pipeline_layout_token(at, CALL, u64_at(72))?;
        let base_pipeline =
            if u64_at(80) == 0 { None } else { Some(vulkan.pipeline_token(at, CALL, u64_at(80))?) };
        requests.push(ComputePipelineRequest {
            flags: u32_at(16),
            stage,
            layout: Some(layout),
            base_pipeline,
            base_pipeline_index: u32_at(88) as i32,
        });
    }

    let created = host.create_compute_pipelines(device, cache, &requests)?;
    if created.pipelines.len() != count {
        return Err(at.refuse(format!(
            "the host answered `{CALL}` with {answered} pipeline slot(s) for {count} create info \
             structure(s). The guest's `pPipelines` array has exactly {count} entries and the \
             specification requires one written per create info",
            answered = created.pipelines.len()
        )));
    }
    // **Written even on failure**, as the graphics call's are: `VK_NULL_HANDLE` for each pipeline
    // that was not created and a real handle for each that was.
    let mut handles = Vec::with_capacity(count * 8);
    for token in &created.pipelines {
        match token {
            None => handles.extend_from_slice(&0u64.to_le_bytes()),
            Some(token) => {
                let registered = vulkan.register_pipeline(at, *token)?;
                c.mem().write_bytes(registered.at, &registered.image, c.blame(5))?;
                handles.extend_from_slice(&(registered.at as u64).to_le_bytes());
            }
        }
    }
    c.mem().write_bytes(out_at, &handles, c.blame(5))?;
    if created.result != VK_SUCCESS {
        vulkan.note_driver_result(CALL, created.result);
    }
    c.ret().i32(created.result);
    Ok(())
}

/// `void vkDestroyPipeline(VkDevice device, VkPipeline pipeline,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_pipeline(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyPipeline";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "pipeline", args[1])?;
    let token = vulkan.pipeline_token(at, CALL, args[1])?;
    host.destroy_pipeline(token)?;
    vulkan.forget_pipeline(handle);
    c.ret().void();
    Ok(())
}

// ---------------------------------------------------------------- decoding one whole pipeline

/// Decode one `VkGraphicsPipelineCreateInfo`.
fn decode_pipeline(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    index: usize,
    info: &[u8],
) -> AbiResult<GraphicsPipelineRequest> {
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let u64_at = |offset: usize| u64::from_le_bytes(info[offset..offset + 8].try_into().expect("eight"));

    let stype = u32_at(0);
    if stype != STYPE_GRAPHICS_PIPELINE_CREATE_INFO {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} and `pCreateInfos[{index}].sType` is \
             {stype}, where `VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO` is \
             {STYPE_GRAPHICS_PIPELINE_CREATE_INFO}. Every one of the nine sub-state pointers \
             would be read at an offset belonging to a different structure, and this layer would \
             follow them into guest memory",
            caller = at.caller
        )));
    }
    if u64_at(8) != 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with \
             `pCreateInfos[{index}].pNext = {next:#x}`. A graphics-pipeline `pNext` chain carries \
             `VkPipelineRenderingCreateInfo` -- which is how a pipeline is created with **no** \
             render pass at all, for dynamic rendering -- and the pipeline-library and \
             creation-feedback structures. Dropping the first would produce a pipeline that is \
             incompatible with every framebuffer, and this layer does not walk chains; see \
             `vkAllocateMemory`'s refusal for the whole argument",
            caller = at.caller,
            next = u64_at(8)
        )));
    }

    let stages = decode_stages(c, at, vulkan, call, u32_at(20) as usize, u64_at(24))?;
    let vertex_input = decode_vertex_input(c, at, call, u64_at(32))?;
    let input_assembly = decode_input_assembly(c, at, call, u64_at(40))?;
    let tessellation = decode_tessellation(c, at, call, u64_at(48))?;
    let viewport = decode_viewport_state(c, at, call, u64_at(56))?;
    let rasterization = decode_body(
        c,
        at,
        call,
        "pRasterizationState",
        u64_at(64),
        STYPE_RASTERIZATION_STATE,
        RASTERIZATION_STATE_BYTES,
        RASTERIZATION_STATE_BODY_BYTES,
    )?;
    let multisample = decode_multisample(c, at, call, u64_at(72))?;
    let depth_stencil = decode_body(
        c,
        at,
        call,
        "pDepthStencilState",
        u64_at(80),
        STYPE_DEPTH_STENCIL_STATE,
        DEPTH_STENCIL_STATE_BYTES,
        DEPTH_STENCIL_STATE_BODY_BYTES,
    )?;
    let color_blend = decode_color_blend(c, at, call, u64_at(88))?;
    let dynamic_states = decode_dynamic_state(c, at, call, u64_at(96))?;

    // **`pRasterizationState` is the one sub-state that is never optional.** The specification
    // requires it for every graphics pipeline, and a NULL here would be a pipeline whose cull
    // mode, polygon mode and `rasterizerDiscardEnable` are all whatever the driver happened to
    // find -- which is a triangle that is silently culled rather than a call that fails.
    if rasterization.is_none() {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with \
             `pCreateInfos[{index}].pRasterizationState = NULL`, which the specification does not \
             permit for a graphics pipeline. Everything that decides whether a triangle is drawn \
             at all -- `rasterizerDiscardEnable`, `cullMode`, `frontFace`, `polygonMode` -- lives \
             in that structure, and a pipeline built without it would be one whose first symptom \
             is an empty frame",
            caller = at.caller
        )));
    }

    let layout = vulkan.pipeline_layout_token(at, call, u64_at(104))?;
    let render_pass = vulkan.render_pass_token(at, call, u64_at(112))?;
    let base_pipeline =
        if u64_at(128) == 0 { None } else { Some(vulkan.pipeline_token(at, call, u64_at(128))?) };

    Ok(GraphicsPipelineRequest {
        flags: u32_at(16),
        stages,
        vertex_input,
        input_assembly,
        tessellation,
        viewport,
        rasterization,
        multisample,
        depth_stencil,
        color_blend,
        dynamic_states,
        layout: Some(layout),
        render_pass: Some(render_pass),
        subpass: u32_at(120),
        base_pipeline,
        base_pipeline_index: u32_at(136) as i32,
    })
}

/// Decode `pStages`, each of which holds a `VkShaderModule`, a `const char *` and a
/// `VkSpecializationInfo *`.
fn decode_stages(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: usize,
    array: u64,
) -> AbiResult<Vec<ShaderStage>> {
    if count == 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `stageCount = 0`. A graphics \
             pipeline with no stages has no vertex shader, and the specification requires one",
            caller = at.caller
        )));
    }
    if count > MAX_PIPELINE_STAGES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `stageCount = {count}`, and this \
             layer reads at most {MAX_PIPELINE_STAGES}. Vulkan 1.0 has five graphics stages",
            caller = at.caller
        )));
    }
    let array_at = require_pointer(at, call, "pStages", array)?;
    let bytes =
        c.mem().read_bytes(array_at, count * PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES, c.blame(3))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let stage = &bytes[index * PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES..]
            [..PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES];
        let stype = u32::from_le_bytes(stage[0..4].try_into().expect("four"));
        if stype != STYPE_PIPELINE_SHADER_STAGE_CREATE_INFO {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} and `pStages[{index}].sType` is \
                 {stype}, where `VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO` is \
                 {STYPE_PIPELINE_SHADER_STAGE_CREATE_INFO}",
                caller = at.caller
            )));
        }
        let next = u64::from_le_bytes(stage[8..16].try_into().expect("eight"));
        if next != 0 {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with \
                 `pStages[{index}].pNext = {next:#x}`. A shader-stage chain carries \
                 `VkPipelineShaderStageRequiredSubgroupSizeCreateInfo`, which changes how the \
                 shader is compiled. This layer does not walk chains",
                caller = at.caller
            )));
        }
        let module_handle = u64::from_le_bytes(stage[24..32].try_into().expect("eight"));
        let module = vulkan.shader_module_token(at, call, module_handle)?;
        let name_pointer = u64::from_le_bytes(stage[32..40].try_into().expect("eight"));
        let name_at = require_pointer(at, call, "pStages[..].pName", name_pointer)?;
        let name_bytes = c.mem().cstr(name_at, c.blame(3))?;
        if name_bytes.len() > MAX_ENTRY_POINT_BYTES {
            return Err(at.refuse(format!(
                "the guest called `{call}` from {caller:#x} with a `pStages[{index}].pName` of \
                 {len} byte(s), and this layer reads at most {MAX_ENTRY_POINT_BYTES}",
                caller = at.caller,
                len = name_bytes.len()
            )));
        }
        let name = String::from_utf8(name_bytes).map_err(|error| {
            at.refuse(format!(
                "the guest called `{call}` from {caller:#x} and `pStages[{index}].pName` is not \
                 UTF-8: {error}. A SPIR-V entry-point name is matched byte for byte against the \
                 `OpEntryPoint` string in the module, so replacing the bad byte would name an \
                 entry point that exists in no module",
                caller = at.caller
            ))
        })?;
        let specialization = decode_specialization(
            c,
            at,
            call,
            index,
            u64::from_le_bytes(stage[40..48].try_into().expect("eight")),
        )?;
        out.push(ShaderStage {
            flags: u32::from_le_bytes(stage[16..20].try_into().expect("four")),
            stage: u32::from_le_bytes(stage[20..24].try_into().expect("four")),
            module: Some(module),
            name,
            specialization,
        });
    }
    Ok(out)
}

/// Decode one `VkSpecializationInfo`, or `None` when the guest passed NULL.
fn decode_specialization(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    stage: usize,
    pointer: u64,
) -> AbiResult<Option<Specialization>> {
    if pointer == 0 {
        return Ok(None);
    }
    let info_at = guest_pointer(at, "pSpecializationInfo", pointer)?;
    let info = c.mem().read_bytes(info_at, SPECIALIZATION_INFO_BYTES, c.blame(3))?;
    let count = u32::from_le_bytes(info[0..4].try_into().expect("four")) as usize;
    let entries_pointer = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    let data_size = u64::from_le_bytes(info[16..24].try_into().expect("eight"));
    let data_pointer = u64::from_le_bytes(info[24..32].try_into().expect("eight"));

    if count > MAX_SPECIALIZATION_ENTRIES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with \
             `pStages[{stage}].pSpecializationInfo->mapEntryCount = {count}`, and this layer \
             reads at most {MAX_SPECIALIZATION_ENTRIES}",
            caller = at.caller
        )));
    }
    let data_size = usize::try_from(data_size).unwrap_or(usize::MAX);
    if data_size > MAX_SPECIALIZATION_BYTES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with \
             `pStages[{stage}].pSpecializationInfo->dataSize = {data_size}`, and this layer reads \
             at most {MAX_SPECIALIZATION_BYTES}",
            caller = at.caller
        )));
    }

    let mut entries = Vec::with_capacity(count);
    if count > 0 && entries_pointer != 0 {
        let entries_at = guest_pointer(at, "pMapEntries", entries_pointer)?;
        let bytes =
            c.mem().read_bytes(entries_at, count * SPECIALIZATION_MAP_ENTRY_BYTES, c.blame(3))?;
        for index in 0..count {
            let entry = &bytes[index * SPECIALIZATION_MAP_ENTRY_BYTES..]
                [..SPECIALIZATION_MAP_ENTRY_BYTES];
            entries.push((
                u32::from_le_bytes(entry[0..4].try_into().expect("four")),
                u32::from_le_bytes(entry[4..8].try_into().expect("four")),
                u64::from_le_bytes(entry[8..16].try_into().expect("eight")),
            ));
        }
    }
    let data = if data_size == 0 || data_pointer == 0 {
        Vec::new()
    } else {
        let data_at = guest_pointer(at, "pData", data_pointer)?;
        c.mem().read_bytes(data_at, data_size, c.blame(3))?
    };
    Ok(Some(Specialization { entries, data }))
}

/// Decode `pVertexInputState`.
fn decode_vertex_input(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<VertexInputState>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pVertexInputState",
        pointer,
        STYPE_VERTEX_INPUT_STATE,
        VERTEX_INPUT_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let bindings = read_blob_array(
        c,
        at,
        call,
        "pVertexBindingDescriptions",
        "VkVertexInputBindingDescription",
        VERTEX_INPUT_BINDING_BYTES,
        u32_at(20) as usize,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
        MAX_VERTEX_BINDINGS,
        3,
    )?;
    let attributes = read_blob_array(
        c,
        at,
        call,
        "pVertexAttributeDescriptions",
        "VkVertexInputAttributeDescription",
        VERTEX_INPUT_ATTRIBUTE_BYTES,
        u32_at(32) as usize,
        u64::from_le_bytes(info[40..48].try_into().expect("eight")),
        MAX_VERTEX_ATTRIBUTES,
        3,
    )?;
    Ok(Some(VertexInputState { flags: u32_at(16), bindings, attributes }))
}

/// Decode `pInputAssemblyState` as `(flags, topology, primitiveRestartEnable)`.
fn decode_input_assembly(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<(u32, u32, u32)>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pInputAssemblyState",
        pointer,
        STYPE_INPUT_ASSEMBLY_STATE,
        INPUT_ASSEMBLY_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    Ok(Some((u32_at(16), u32_at(20), u32_at(24))))
}

/// Decode `pTessellationState` as `(flags, patchControlPoints)`.
fn decode_tessellation(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<(u32, u32)>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pTessellationState",
        pointer,
        STYPE_TESSELLATION_STATE,
        TESSELLATION_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    Ok(Some((u32_at(16), u32_at(20))))
}

/// Decode `pViewportState`, keeping the counts separate from the arrays. See
/// [`ViewportState`](super::host::ViewportState) for why that separation is load-bearing.
fn decode_viewport_state(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<ViewportState>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pViewportState",
        pointer,
        STYPE_VIEWPORT_STATE,
        VIEWPORT_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let viewport_count = u32_at(20);
    let scissor_count = u32_at(32);
    let viewports = read_flat_array(
        c,
        at,
        call,
        "pViewports",
        "VkViewport",
        VIEWPORT_BYTES,
        viewport_count as usize,
        u64::from_le_bytes(info[24..32].try_into().expect("eight")),
        MAX_VIEWPORTS,
        3,
    )?;
    let scissors = read_flat_array(
        c,
        at,
        call,
        "pScissors",
        "VkRect2D",
        RECT_2D_BYTES,
        scissor_count as usize,
        u64::from_le_bytes(info[40..48].try_into().expect("eight")),
        MAX_VIEWPORTS,
        3,
    )?;
    Ok(Some(ViewportState {
        flags: u32_at(16),
        viewport_count,
        viewports,
        scissor_count,
        scissors,
    }))
}

/// Decode `pMultisampleState`, whose one pointer is `pSampleMask`.
fn decode_multisample(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<MultisampleState>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pMultisampleState",
        pointer,
        STYPE_MULTISAMPLE_STATE,
        MULTISAMPLE_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let samples = u32_at(20);
    let mask_pointer = u64::from_le_bytes(info[32..40].try_into().expect("eight"));
    // `pSampleMask` is `ceil(rasterizationSamples / 32)` words, and `rasterizationSamples` is a
    // single bit of `VkSampleCountFlagBits` — so the length comes from the *value*, which is why
    // it is computed rather than read from a count the guest also supplies. A `samples` of zero
    // is invalid and the driver will say so; here it simply means no words.
    let words = (samples as usize).div_ceil(32).min(MAX_SAMPLE_MASK_WORDS);
    let sample_mask = if mask_pointer == 0 || words == 0 {
        Vec::new()
    } else {
        let mask_at = guest_pointer(at, "pSampleMask", mask_pointer)?;
        let bytes = c.mem().read_bytes(mask_at, words * 4, c.blame(3))?;
        (0..words)
            .map(|index| u32::from_le_bytes(bytes[index * 4..][..4].try_into().expect("four")))
            .collect()
    };
    Ok(Some(MultisampleState {
        flags: u32_at(16),
        samples,
        sample_shading: u32_at(24),
        min_sample_shading: info[28..32].try_into().expect("four"),
        sample_mask,
        alpha_to_coverage: u32_at(40),
        alpha_to_one: u32_at(44),
    }))
}

/// Decode `pColorBlendState`.
fn decode_color_blend(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<ColorBlendState>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pColorBlendState",
        pointer,
        STYPE_COLOR_BLEND_STATE,
        COLOR_BLEND_STATE_BYTES,
    )?
    else {
        return Ok(None);
    };
    let u32_at = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let attachments = read_blob_array(
        c,
        at,
        call,
        "pAttachments",
        "VkPipelineColorBlendAttachmentState",
        COLOR_BLEND_ATTACHMENT_BYTES,
        u32_at(28) as usize,
        u64::from_le_bytes(info[32..40].try_into().expect("eight")),
        MAX_BLEND_ATTACHMENTS,
        3,
    )?;
    Ok(Some(ColorBlendState {
        flags: u32_at(16),
        logic_op_enable: u32_at(20),
        logic_op: u32_at(24),
        attachments,
        blend_constants: info[40..56].try_into().expect("sixteen"),
    }))
}

/// Decode `pDynamicState`'s `pDynamicStates`, distinguishing "no `pDynamicState`" from "an empty
/// list".
fn decode_dynamic_state(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    pointer: u64,
) -> AbiResult<Option<Vec<u32>>> {
    let Some(info) = read_substate(
        c,
        at,
        call,
        "pDynamicState",
        pointer,
        STYPE_DYNAMIC_STATE,
        DYNAMIC_STATE_CREATE_INFO_BYTES,
    )?
    else {
        return Ok(None);
    };
    let count = u32::from_le_bytes(info[20..24].try_into().expect("four")) as usize;
    let array = u64::from_le_bytes(info[24..32].try_into().expect("eight"));
    if count == 0 || array == 0 {
        return Ok(Some(Vec::new()));
    }
    if count > MAX_DYNAMIC_STATES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `dynamicStateCount = {count}`, and \
             this layer reads at most {MAX_DYNAMIC_STATES}",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pDynamicStates", array)?;
    let bytes = c.mem().read_bytes(array_at, count * 4, c.blame(3))?;
    Ok(Some(
        (0..count)
            .map(|index| u32::from_le_bytes(bytes[index * 4..][..4].try_into().expect("four")))
            .collect(),
    ))
}

/// Decode `pSubpasses`, the one array in a render pass that holds pointers of its own.
fn decode_subpasses(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    count: usize,
    array: u64,
) -> AbiResult<Vec<SubpassRequest>> {
    if count == 0 || array == 0 {
        return Ok(Vec::new());
    }
    if count > MAX_SUBPASSES {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `subpassCount = {count}`, and this \
             layer reads at most {MAX_SUBPASSES}",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, "pSubpasses", array)?;
    let bytes = c.mem().read_bytes(array_at, count * SUBPASS_DESCRIPTION_BYTES, c.blame(1))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &bytes[index * SUBPASS_DESCRIPTION_BYTES..][..SUBPASS_DESCRIPTION_BYTES];
        let u32_at =
            |offset: usize| u32::from_le_bytes(entry[offset..offset + 4].try_into().expect("four"));
        let u64_at = |offset: usize| {
            u64::from_le_bytes(entry[offset..offset + 8].try_into().expect("eight"))
        };
        let colour_count = u32_at(24) as usize;
        let input_attachments = read_flat_array(
            c,
            at,
            call,
            "pInputAttachments",
            "VkAttachmentReference",
            ATTACHMENT_REFERENCE_BYTES,
            u32_at(8) as usize,
            u64_at(16),
            MAX_SUBPASS_REFERENCES,
            1,
        )?;
        let color_attachments = read_flat_array(
            c,
            at,
            call,
            "pColorAttachments",
            "VkAttachmentReference",
            ATTACHMENT_REFERENCE_BYTES,
            colour_count,
            u64_at(32),
            MAX_SUBPASS_REFERENCES,
            1,
        )?;
        // **`pResolveAttachments` has no count of its own**: the specification says it is either
        // NULL or an array of exactly `colorAttachmentCount` entries. A layer that read a count
        // from somewhere would be inventing one.
        let resolve_attachments = read_flat_array(
            c,
            at,
            call,
            "pResolveAttachments",
            "VkAttachmentReference",
            ATTACHMENT_REFERENCE_BYTES,
            colour_count,
            u64_at(40),
            MAX_SUBPASS_REFERENCES,
            1,
        )?;
        let depth_pointer = u64_at(48);
        let depth_stencil_attachment = if depth_pointer == 0 {
            None
        } else {
            let depth_at = guest_pointer(at, "pDepthStencilAttachment", depth_pointer)?;
            Some(c.mem().read_bytes(depth_at, ATTACHMENT_REFERENCE_BYTES, c.blame(1))?)
        };
        let preserve_count = u32_at(56) as usize;
        let preserve_pointer = u64_at(64);
        let preserve_attachments = if preserve_count == 0 || preserve_pointer == 0 {
            Vec::new()
        } else {
            if preserve_count > MAX_SUBPASS_REFERENCES {
                return Err(at.refuse(format!(
                    "the guest called `{call}` from {caller:#x} with \
                     `pSubpasses[{index}].preserveAttachmentCount = {preserve_count}`, and this \
                     layer reads at most {MAX_SUBPASS_REFERENCES}",
                    caller = at.caller
                )));
            }
            let preserve_at = guest_pointer(at, "pPreserveAttachments", preserve_pointer)?;
            let raw = c.mem().read_bytes(preserve_at, preserve_count * 4, c.blame(1))?;
            (0..preserve_count)
                .map(|i| u32::from_le_bytes(raw[i * 4..][..4].try_into().expect("four")))
                .collect()
        };
        out.push(SubpassRequest {
            flags: u32_at(0),
            bind_point: u32_at(4),
            input_attachments,
            color_attachments,
            resolve_attachments,
            depth_stencil_attachment,
            preserve_attachments,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------------------ small helpers

/// Read one optional pipeline sub-state, checking its `sType` and refusing its `pNext`.
///
/// `None` for a NULL pointer, which is the whole reason this returns an `Option`: this module's
/// header says why a zeroed structure substituted for a NULL is a different pipeline.
fn read_substate(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    pointer: u64,
    stype: u32,
    len: usize,
) -> AbiResult<Option<Vec<u8>>> {
    if pointer == 0 {
        return Ok(None);
    }
    let info_at = guest_pointer(at, field, pointer)?;
    let bytes = c.mem().read_bytes(info_at, len, c.blame(3))?;
    let found = u32::from_le_bytes(bytes[0..4].try_into().expect("four"));
    if found != stype {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with a `{field}` at {info_at:#x} whose \
             `sType` is {found}, where this sub-state's is {stype}. Every member after it would \
             be read at an offset belonging to a different structure, and a pipeline built from \
             them would be a plausible pipeline that draws something else",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(bytes[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `{field}->pNext = {next:#x}`. This \
             layer does not walk `pNext` chains; see `vkAllocateMemory`'s refusal for the whole \
             argument, and the address is named so a run says which structure the engine sends",
            caller = at.caller
        )));
    }
    Ok(Some(bytes))
}

/// Read one optional sub-state and answer the **body** after its `pNext`, for the two whose
/// bodies are entirely scalars.
#[allow(clippy::too_many_arguments)]
fn decode_body(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    pointer: u64,
    stype: u32,
    len: usize,
    body: usize,
) -> AbiResult<Option<Vec<u8>>> {
    Ok(read_substate(c, at, call, field, pointer, stype, len)?.map(|bytes| bytes[16..16 + body].to_vec()))
}

/// Read a guest array of fixed-size structures as one **flat** blob.
#[allow(clippy::too_many_arguments)]
fn read_flat_array(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    element: &str,
    element_bytes: usize,
    count: usize,
    pointer: u64,
    bound: usize,
    argument: usize,
) -> AbiResult<Vec<u8>> {
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > bound {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with {count} `{element}` entries in \
             `{field}`, and this layer reads at most {bound}. The count is a guest `uint32_t` \
             indexing an array of {element_bytes}-byte structures, so honouring it unbounded \
             would be a guest-controlled host allocation (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let array_at = guest_pointer(at, field, pointer)?;
    c.mem().read_bytes(array_at, count * element_bytes, c.blame(argument))
}

/// The same, as one `Vec<u8>` per element — for the arrays a host iterates rather than memcpys.
#[allow(clippy::too_many_arguments)]
fn read_blob_array(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    element: &str,
    element_bytes: usize,
    count: usize,
    pointer: u64,
    bound: usize,
    argument: usize,
) -> AbiResult<Vec<Vec<u8>>> {
    let flat = read_flat_array(
        c,
        at,
        call,
        field,
        element,
        element_bytes,
        count,
        pointer,
        bound,
        argument,
    )?;
    Ok(flat.chunks_exact(element_bytes).map(<[u8]>::to_vec).collect())
}

/// Read a guest array of non-dispatchable handles, unresolved.
#[allow(clippy::too_many_arguments)]
pub(super) fn read_u64_array(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    call: &str,
    field: &str,
    count: usize,
    pointer: u64,
    bound: usize,
    argument: usize,
) -> AbiResult<Vec<u64>> {
    if count == 0 || pointer == 0 {
        return Ok(Vec::new());
    }
    if count > bound {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with {count} entries in `{field}`, and \
             this layer reads at most {bound}. The count is a guest `uint32_t` indexing an array \
             of handles, so honouring it unbounded would be a guest-controlled host allocation \
             (Global Constraint 11)",
            caller = at.caller
        )));
    }
    let array_at: GuestAddr = guest_pointer(at, field, pointer)?;
    let bytes = c.mem().read_bytes(array_at, count * 8, c.blame(argument))?;
    Ok((0..count)
        .map(|index| u64::from_le_bytes(bytes[index * 8..][..8].try_into().expect("eight")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every structure size is the specification's**, with the arithmetic written out so a
    /// reader can check it against `vulkan_core.h` by eye rather than remember it.
    ///
    /// These are the numbers a transposition would hide in: a `pStages` read at offset 32 rather
    /// than 24 would produce a pipeline with the vertex-input state as its stage array, and the
    /// first symptom would be a driver crash on a machine with no validation layers.
    #[test]
    fn the_pipeline_structure_numbers_are_the_specifications() {
        // The header of every one of them: `sType` at 0, four bytes of padding, `pNext` at 8.
        assert_eq!(SHADER_MODULE_CREATE_INFO_BYTES, 32 + 8, "codeSize at 24, pCode at 32");
        assert_eq!(PIPELINE_CACHE_CREATE_INFO_BYTES, SHADER_MODULE_CREATE_INFO_BYTES);
        assert_eq!(PIPELINE_LAYOUT_CREATE_INFO_BYTES, 40 + 8);
        assert_eq!(PUSH_CONSTANT_RANGE_BYTES, 3 * 4);
        assert_eq!(RENDER_PASS_CREATE_INFO_BYTES, 56 + 8);
        assert_eq!(ATTACHMENT_DESCRIPTION_BYTES, 9 * 4);
        assert_eq!(ATTACHMENT_REFERENCE_BYTES, 2 * 4);
        assert_eq!(SUBPASS_DESCRIPTION_BYTES, 64 + 8);
        assert_eq!(SUBPASS_DEPENDENCY_BYTES, 7 * 4);
        assert_eq!(FRAMEBUFFER_CREATE_INFO_BYTES, 56 + 4 + 4);
        assert_eq!(GRAPHICS_PIPELINE_CREATE_INFO_BYTES, 136 + 4 + 4);
        assert_eq!(PIPELINE_SHADER_STAGE_CREATE_INFO_BYTES, 40 + 8);
        assert_eq!(SPECIALIZATION_INFO_BYTES, 24 + 8);
        assert_eq!(SPECIALIZATION_MAP_ENTRY_BYTES, 4 + 4 + 8);
        assert_eq!(VERTEX_INPUT_STATE_BYTES, 40 + 8);
        assert_eq!(VERTEX_INPUT_BINDING_BYTES, 3 * 4);
        assert_eq!(VERTEX_INPUT_ATTRIBUTE_BYTES, 4 * 4);
        assert_eq!(INPUT_ASSEMBLY_STATE_BYTES, 24 + 4 + 4);
        assert_eq!(TESSELLATION_STATE_BYTES, 20 + 4);
        assert_eq!(VIEWPORT_STATE_BYTES, 40 + 8);
        assert_eq!(VIEWPORT_BYTES, 6 * 4);
        assert_eq!(RECT_2D_BYTES, 4 * 4);
        assert_eq!(MULTISAMPLE_STATE_BYTES, 44 + 4);
        assert_eq!(COLOR_BLEND_STATE_BYTES, 40 + 16);
        assert_eq!(COLOR_BLEND_ATTACHMENT_BYTES, 8 * 4);
        assert_eq!(DYNAMIC_STATE_CREATE_INFO_BYTES, 24 + 8);
    }

    /// **The two bodies that travel as bytes are the members and not the padding.**
    ///
    /// `VkPipelineRasterizationStateCreateInfo`'s last member ends at 60 and the structure is
    /// padded to 64; carrying the tail would make a host that reconstructs field by field and one
    /// that memcpys disagree about four bytes neither of them means.
    #[test]
    fn the_two_byte_bodies_are_the_members_without_the_padding() {
        assert_eq!(RASTERIZATION_STATE_BYTES, 64);
        assert_eq!(RASTERIZATION_STATE_BODY_BYTES, 11 * 4, "eleven scalars after pNext");
        assert_eq!(16 + RASTERIZATION_STATE_BODY_BYTES, 60, "and the structure ends at 60");
        assert_eq!(
            RASTERIZATION_STATE_BYTES - 16 - RASTERIZATION_STATE_BODY_BYTES,
            4,
            "the four tail bytes that are padding and are deliberately not carried"
        );

        assert_eq!(DEPTH_STENCIL_STATE_BYTES, 104);
        // `flags` and five more scalars, two 28-byte `VkStencilOpState`s, two floats.
        assert_eq!(DEPTH_STENCIL_STATE_BODY_BYTES, 6 * 4 + 28 + 28 + 2 * 4);
        assert_eq!(DEPTH_STENCIL_STATE_BODY_BYTES, 88);
        assert_eq!(16 + DEPTH_STENCIL_STATE_BODY_BYTES, DEPTH_STENCIL_STATE_BYTES);
    }

    /// The `sType` values are the specification's, in the block Vulkan 1.0 fixed them in.
    #[test]
    fn the_structure_types_are_the_specifications() {
        assert_eq!(STYPE_SHADER_MODULE_CREATE_INFO, 16);
        assert_eq!(STYPE_PIPELINE_CACHE_CREATE_INFO, 17);
        assert_eq!(STYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, 18);
        // 19 through 27 are the nine pipeline sub-states, in the order they appear in the
        // structure -- which is what makes an off-by-one here so easy to write and so quiet.
        assert_eq!(
            [
                STYPE_VERTEX_INPUT_STATE,
                STYPE_INPUT_ASSEMBLY_STATE,
                STYPE_TESSELLATION_STATE,
                STYPE_VIEWPORT_STATE,
                STYPE_RASTERIZATION_STATE,
                STYPE_MULTISAMPLE_STATE,
                STYPE_DEPTH_STENCIL_STATE,
                STYPE_COLOR_BLEND_STATE,
                STYPE_DYNAMIC_STATE,
            ],
            [19, 20, 21, 22, 23, 24, 25, 26, 27]
        );
        assert_eq!(STYPE_GRAPHICS_PIPELINE_CREATE_INFO, 28);
        assert_eq!(STYPE_PIPELINE_LAYOUT_CREATE_INFO, 30);
        assert_eq!(STYPE_FRAMEBUFFER_CREATE_INFO, 37);
        assert_eq!(STYPE_RENDER_PASS_CREATE_INFO, 38);
    }
}
