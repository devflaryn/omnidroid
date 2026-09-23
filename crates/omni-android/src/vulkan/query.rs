//! **Query pools: the engine's GPU timer.**
//!
//! MEASURED: once its swapchain, image views, semaphores, fences and command pools exist, the
//! engine creates `gpuTimeQueryPool` (`0x2592d68`..`0x2592da0` in `libroblox.so`, the string is
//! its own assertion text) -- a pool of timestamp queries it reads back to time the GPU's frame.
//! A `VkQueryPool` is a non-dispatchable handle of its own family ([`MAX_QUERY_POOLS`]); its
//! create info has no pointer beyond `pNext`, which is refused by name as every create info's is
//! here ([`check_header`]).
//!
//! [`MAX_QUERY_POOLS`]: super::MAX_QUERY_POOLS

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, QueryPoolRequest};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::resource::check_header;
use super::{Site, Vulkan, VK_SUCCESS};

/// `VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO`.
pub const STYPE_QUERY_POOL_CREATE_INFO: u32 = 11;

/// `sizeof(VkQueryPoolCreateInfo)`.
///
/// ```text
/// VkStructureType                  sType;               //  0  (then 4 of padding)
/// const void                      *pNext;               //  8
/// VkQueryPoolCreateFlags           flags;               // 16
/// VkQueryType                      queryType;           // 20
/// uint32_t                         queryCount;          // 24
/// VkQueryPipelineStatisticFlags    pipelineStatistics;  // 28
/// ```
pub const QUERY_POOL_CREATE_INFO_BYTES: usize = 32;

/// `VkResult vkCreateQueryPool(VkDevice device, const VkQueryPoolCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkQueryPool *pQueryPool)`
pub(super) fn create_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateQueryPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pQueryPool", args[3])?;

    let info = c.mem().read_bytes(info_at, QUERY_POOL_CREATE_INFO_BYTES, c.blame(1))?;
    check_header(
        at,
        CALL,
        &info,
        STYPE_QUERY_POOL_CREATE_INFO,
        "VK_STRUCTURE_TYPE_QUERY_POOL_CREATE_INFO",
        "`queryType` and `queryCount` would be read at offsets belonging to a different structure",
        "a query-pool `pNext` chain carries `VkQueryPoolPerformanceCreateInfoKHR`, which selects \
         performance counters",
    )?;
    let word = |offset: usize| u32::from_le_bytes(info[offset..offset + 4].try_into().expect("four"));
    let request = QueryPoolRequest {
        flags: word(16),
        query_type: word(20),
        query_count: word(24),
        pipeline_statistics: word(28),
    };

    match host.create_query_pool(device, &request)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_query_pool(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkCmdResetQueryPool(VkCommandBuffer commandBuffer, VkQueryPool queryPool,
/// uint32_t firstQuery, uint32_t queryCount)`
///
/// MEASURED: the first command the engine records, `(pool, 0, 2)` -- the GPU timer's two
/// timestamps, reset before a frame writes them.
pub(super) fn cmd_reset_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdResetQueryPool";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let pool = vulkan.query_pool_token(at, CALL, args[1])?;
    host.cmd_reset_query_pool(buffer, pool, args[2] as u32, args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkCmdWriteTimestamp(VkCommandBuffer commandBuffer, VkPipelineStageFlagBits
/// pipelineStage, VkQueryPool queryPool, uint32_t query)`
///
/// The other half of a timestamp pool's use, and the only way one is written: the pool the engine
/// created is `VK_QUERY_TYPE_TIMESTAMP`, which nothing but this command fills.
pub(super) fn cmd_write_timestamp(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCmdWriteTimestamp";
    let host = vulkan.require_host(at)?;
    let buffer = vulkan.command_buffer_token(at, CALL, args[0])?;
    let pool = vulkan.query_pool_token(at, CALL, args[2])?;
    host.cmd_write_timestamp(buffer, args[1] as u32, pool, args[3] as u32)?;
    c.ret().void();
    Ok(())
}

/// `void vkDestroyQueryPool(VkDevice device, VkQueryPool queryPool,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_query_pool(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyQueryPool";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "queryPool", args[1])?;
    let token = vulkan.query_pool_token(at, CALL, args[1])?;
    host.destroy_query_pool(token)?;
    vulkan.forget_query_pool(handle);
    c.ret().void();
    Ok(())
}
