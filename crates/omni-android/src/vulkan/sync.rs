//! **Semaphores and fences: the six calls that make a frame loop possible.**
//!
//! # Two primitives that look alike and are not
//!
//! A `VkSemaphore` and a `VkFence` are both non-dispatchable 64-bit values, both created from a
//! three-field structure whose only interesting member is a `flags`, and both destroyed by a call
//! of the same shape. **Nothing in either value says which it is.** That is why they have separate
//! registries over separate ranges of the boundary's data area: a guest that passed its fence
//! where the semaphore belongs would otherwise get a lookup that lands, and the driver would be
//! told to wait on an object of the wrong kind.
//!
//! What they *do* differs completely, and it is worth stating because it is why both are here:
//!
//! * A **semaphore** orders work on the GPU. Nothing on the host can observe it; there is no
//!   "is it signalled?" call for a binary one. A frame waits on the acquire semaphore so that it
//!   does not write an image the presentation engine is still reading, and present waits on the
//!   render-finished semaphore so that the screen does not show a half-drawn frame.
//! * A **fence** is the one synchronisation object the *host* can observe.
//!   `vkWaitForFences` is what tells a frame loop that the command buffer it recorded two frames
//!   ago has finished executing and is safe to re-record.
//!
//! # `VK_TIMEOUT` is a success, and this is where that matters most
//!
//! `vkWaitForFences` returns `VK_SUCCESS`, `VK_TIMEOUT` or a failure. `VK_TIMEOUT` is a **success
//! code**: the call did what it was asked and the fence had not signalled yet. A shim that
//! normalised it to `VK_SUCCESS` — or that treated "non-zero" as "failed" and refused — would
//! break a frame loop in the two ways that are hardest to see. Told `VK_SUCCESS`, the guest
//! re-records a command buffer the GPU is still executing, and on this machine there are no
//! validation layers to say so (`docs/research/graphics-spike.md` §6). Told an error, it tears
//! down a renderer that was merely busy.
//!
//! So the driver's code travels into the guest's `X0` unchanged, exactly as
//! [`swapchain`](super::swapchain)'s two resize codes do, and for the same reason: the caller has
//! a branch for it and this layer does not know which one.
//!
//! # The timeout is not clamped
//!
//! `timeout` is a `uint64_t` of nanoseconds and `UINT64_MAX` means "wait forever". It is passed
//! through unchanged, including that value. A layer that clamped it — to a second, say, on the
//! theory that a frame should never take that long — would convert a slow frame into a
//! `VK_TIMEOUT` the engine would read as a dropped frame, and the clamp would be invisible in
//! every run where the GPU was fast enough.

use std::sync::Arc;

use crate::abi::ARG_REGISTERS;
use crate::boundary::ImportCall;
use crate::error::AbiResult;

use super::host::{DriverAnswer, HostFence};
use super::instance::{guest_pointer, refuse_allocator, require_pointer};
use super::{Site, Vulkan, VK_SUCCESS};

// ----------------------------------------------------------------- the specification's numbers

/// `VK_STRUCTURE_TYPE_FENCE_CREATE_INFO`.
const STYPE_FENCE_CREATE_INFO: u32 = 8;
/// `VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO`.
const STYPE_SEMAPHORE_CREATE_INFO: u32 = 9;

/// `sizeof(VkSemaphoreCreateInfo)` on any LP64 or LLP64 target.
///
/// ```text
/// VkStructureType             sType;   //  0  (then 4 of padding)
/// const void                 *pNext;   //  8
/// VkSemaphoreCreateFlags      flags;   // 16  (then 4 of trailing padding, alignment 8)
/// ```
pub const SEMAPHORE_CREATE_INFO_BYTES: usize = 24;

/// `sizeof(VkFenceCreateInfo)`. The same three members as [`SEMAPHORE_CREATE_INFO_BYTES`], and
/// the same twenty-four bytes — which is exactly why the two structures cannot be told apart by
/// their size and `sType` is the only thing that distinguishes them.
pub const FENCE_CREATE_INFO_BYTES: usize = 24;

/// How many fences one `vkWaitForFences` or `vkResetFences` may name.
///
/// An allocation bound for [`MAX_ENABLED_NAMES`](super::MAX_ENABLED_NAMES)' reason: `fenceCount`
/// is a guest `uint32_t` indexing an array of 64-bit handles, so honouring it unbounded is a
/// guest-controlled host allocation of up to 32 GB. Thirty-two is above
/// [`MAX_FENCES`](super::MAX_FENCES), so a conforming guest cannot reach it — every fence it could
/// name is one this layer issued, and there are at most half this many.
pub const MAX_FENCES_PER_CALL: usize = 32;

// ------------------------------------------------------------------------------- the handlers

/// `VkResult vkCreateSemaphore(VkDevice device, const VkSemaphoreCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkSemaphore *pSemaphore)`
pub(super) fn create_semaphore(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateSemaphore";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let create_info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pSemaphore", args[3])?;

    let flags = decode_flags_only(
        c,
        at,
        &FlagsOnly {
            call: CALL,
            stype_name: "VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO",
            expected_stype: STYPE_SEMAPHORE_CREATE_INFO,
            why_the_chain_matters:
                "a semaphore `pNext` chain is where `VkSemaphoreTypeCreateInfo` goes, and that \
                 is what turns a binary semaphore into a **timeline** semaphore -- an object \
                 with completely different wait semantics that `vkQueueSubmit` addresses \
                 through a different structure. Dropping the chain would create a binary \
                 semaphore where the engine asked for a timeline one, and every later wait on \
                 it would be a wait on the wrong kind of object",
            create_info_at,
            bytes: SEMAPHORE_CREATE_INFO_BYTES,
        },
    )?;

    match host.create_semaphore(device, flags)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_semaphore(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroySemaphore(VkDevice device, VkSemaphore semaphore,
/// const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_semaphore(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroySemaphore";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "semaphore", args[1])?;
    let semaphore = vulkan.semaphore_token(at, CALL, args[1])?;
    host.destroy_semaphore(semaphore)?;
    vulkan.forget_semaphore(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkCreateFence(VkDevice device, const VkFenceCreateInfo *pCreateInfo,
/// const VkAllocationCallbacks *pAllocator, VkFence *pFence)`
///
/// `flags` carries `VK_FENCE_CREATE_SIGNALED_BIT`, which is the bit every frame loop sets on its
/// per-frame fences so that the first `vkWaitForFences` of the first frame returns immediately
/// instead of waiting forever for work that was never submitted. It is forwarded rather than
/// interpreted; a layer that dropped it would hang the guest on its first frame.
pub(super) fn create_fence(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkCreateFence";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let create_info_at = require_pointer(at, CALL, "pCreateInfo", args[1])?;
    let out_at = require_pointer(at, CALL, "pFence", args[3])?;

    let flags = decode_flags_only(
        c,
        at,
        &FlagsOnly {
            call: CALL,
            stype_name: "VK_STRUCTURE_TYPE_FENCE_CREATE_INFO",
            expected_stype: STYPE_FENCE_CREATE_INFO,
            why_the_chain_matters:
                "a fence `pNext` chain is where `VkExportFenceCreateInfo` goes, which makes the \
                 fence's payload shareable with another process or API. Dropping it would \
                 create an ordinary fence where the engine asked for an exportable one, and the \
                 failure would arrive at the `vkGetFenceFdKHR` that came next",
            create_info_at,
            bytes: FENCE_CREATE_INFO_BYTES,
        },
    )?;

    match host.create_fence(device, flags)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
            Ok(())
        }
        DriverAnswer::Ok(token) => {
            let registered = vulkan.register_fence(at, token)?;
            c.mem().write_bytes(registered.at, &registered.image, c.blame(3))?;
            c.mem().write_u64(out_at, registered.at as u64, c.blame(3))?;
            c.ret().i32(VK_SUCCESS);
            Ok(())
        }
    }
}

/// `void vkDestroyFence(VkDevice device, VkFence fence, const VkAllocationCallbacks *pAllocator)`
pub(super) fn destroy_fence(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkDestroyFence";
    refuse_allocator(vulkan, at, CALL, args[2])?;
    let host = vulkan.require_host(at)?;
    let _device = vulkan.device_token(at, CALL, args[0])?;
    if args[1] == 0 {
        c.ret().void();
        return Ok(());
    }
    let handle = guest_pointer(at, "fence", args[1])?;
    let fence = vulkan.fence_token(at, CALL, args[1])?;
    host.destroy_fence(fence)?;
    vulkan.forget_fence(handle);
    c.ret().void();
    Ok(())
}

/// `VkResult vkWaitForFences(VkDevice device, uint32_t fenceCount, const VkFence *pFences,
/// VkBool32 waitAll, uint64_t timeout)`
///
/// This module's header carries the argument for why `VK_TIMEOUT` travels verbatim and why the
/// timeout is not clamped.
///
/// `waitAll` is a `VkBool32`, so **any** non-zero value is true; the comparison here is `!= 0`
/// rather than `== 1`, because a guest that wrote `VK_TRUE` as `-1` — which a C `!x` idiom
/// produces — would otherwise be told to wait for the first fence instead of all of them, and the
/// difference is a frame loop that re-records a buffer the GPU is still reading.
pub(super) fn wait_for_fences(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkWaitForFences";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let fences = decode_fences(c, at, vulkan, CALL, args[1] as u32, args[2])?;
    if fences.is_empty() {
        // **Refused here rather than forwarded**, and it is the one place in this module where
        // this layer has an opinion the driver would also have had. `fenceCount` must be greater
        // than zero, a wait on no fences returns `VK_SUCCESS` immediately, and what that tells a
        // frame loop is that work it never submitted has finished -- after which it re-records a
        // command buffer the GPU may be reading. The driver would refuse too, and on this machine
        // nothing would report it: there are no validation layers
        // (`docs/research/graphics-spike.md` §6), so the driver's own `VK_SUCCESS` for a
        // zero-length wait would be indistinguishable from a wait that happened.
        return Err(at.refuse(format!(
            "the guest called `{CALL}` from {caller:#x} with no fences -- `fenceCount` was zero. \
             The specification requires it to be greater than zero, and `VK_SUCCESS` for a wait \
             on nothing is the plausible answer Global Constraint 1 exists for: it says work has \
             finished that was never submitted",
            caller = at.caller
        )));
    }
    let wait_all = (args[3] as u32) != 0;
    let timeout = args[4];

    let result = host.wait_for_fences(device, &fences, wait_all, timeout)?;
    if result != VK_SUCCESS {
        // `VK_TIMEOUT` included: a run whose frames are timing out is the thing a reader wants to
        // find in the log, and it is not a failure.
        vulkan.note_driver_result(CALL, result);
    }
    c.ret().i32(result);
    Ok(())
}

/// `VkResult vkResetFences(VkDevice device, uint32_t fenceCount, const VkFence *pFences)`
///
/// # Why a count of zero is answered and not refused
///
/// The specification requires `fenceCount` to be greater than zero, and this layer forwards a
/// zero-length list anyway rather than refusing it — because the driver owns that rule and a
/// second copy of it here would be a second place for it to be wrong. What this layer does own is
/// that every handle in the list is one it issued.
pub(super) fn reset_fences(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    args: [u64; ARG_REGISTERS as usize],
) -> AbiResult<()> {
    const CALL: &str = "vkResetFences";
    let host = vulkan.require_host(at)?;
    let device = vulkan.device_token(at, CALL, args[0])?;
    let fences = decode_fences(c, at, vulkan, CALL, args[1] as u32, args[2])?;
    match host.reset_fences(device, &fences)? {
        DriverAnswer::Failed(result) => {
            vulkan.note_driver_result(CALL, result);
            c.ret().i32(result);
        }
        DriverAnswer::Ok(()) => c.ret().i32(VK_SUCCESS),
    }
    Ok(())
}

// ------------------------------------------------------------------------------ small helpers

/// Decode a `const VkFence *` of `count` handles, each through the fence registry.
fn decode_fences(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    vulkan: &Arc<Vulkan>,
    call: &str,
    count: u32,
    array: u64,
) -> AbiResult<Vec<HostFence>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let count = count as usize;
    if count > MAX_FENCES_PER_CALL {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `fenceCount = {count}`, and this \
             layer reads at most {MAX_FENCES_PER_CALL}. The count is a guest `uint32_t` indexing \
             an array of 64-bit handles, so honouring it unbounded would be a guest-controlled \
             host allocation (Global Constraint 11). It is also more fences than can exist: this \
             layer issues at most {max} of them",
            caller = at.caller,
            max = super::MAX_FENCES
        )));
    }
    let array_at = require_pointer(at, call, "pFences", array)?;
    let bytes = c.mem().read_bytes(array_at, count * 8, c.blame(2))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let handle =
            u64::from_le_bytes(bytes[index * 8..index * 8 + 8].try_into().expect("eight"));
        // **Each one, individually.** A list in which one handle is wild is a list this layer
        // must refuse whole: forwarding the good ones and dropping the bad one would make
        // `vkWaitForFences` wait for fewer fences than the guest named, which with `waitAll` is
        // the difference between "every frame has finished" and "some of them have".
        out.push(vulkan.fence_token(at, call, handle)?);
    }
    Ok(out)
}

/// One of the two create-infos whose only member is a `flags`, as the facts it takes to read it.
///
/// **A struct rather than six parameters**, for [`counted::Array`](super::counted::Array)'s
/// reason and not only because clippy counts: `call`, `stype_name` and `why_the_chain_matters`
/// are all `&str`, and a positional call that transposed any two of them would compile — and
/// would then print a refusal naming the wrong structure, which is the worst possible outcome for
/// a message whose whole job is to say which structure was wrong.
struct FlagsOnly<'a> {
    /// The Vulkan function, for every refusal this produces.
    call: &'a str,
    /// The `VkStructureType` constant's own name, for the refusal that says the `sType` is wrong.
    stype_name: &'a str,
    /// Its value.
    expected_stype: u32,
    /// What a dropped `pNext` chain would cost, for the refusal that names one.
    why_the_chain_matters: &'a str,
    /// The guest's `pCreateInfo`, already checked non-NULL.
    create_info_at: omni_mem::GuestAddr,
    /// `sizeof` that structure.
    bytes: usize,
}

/// Decode one of the two create-infos whose only member is a `flags`.
///
/// **Written once because the two structures are byte-for-byte identical.** `VkFenceCreateInfo`
/// and `VkSemaphoreCreateInfo` have the same three members at the same three offsets and the same
/// size, and nothing but `sType` distinguishes them — so one function that takes the expected
/// `sType` and the name to print is strictly safer than two copies, of which one could drift.
fn decode_flags_only(
    c: &mut ImportCall<'_, '_>,
    at: &Site,
    what: &FlagsOnly<'_>,
) -> AbiResult<u32> {
    let FlagsOnly { call, stype_name, expected_stype, why_the_chain_matters, .. } = *what;
    let info = c.mem().read_bytes(what.create_info_at, what.bytes, c.blame(1))?;
    let stype = u32::from_le_bytes(info[0..4].try_into().expect("four"));
    if stype != expected_stype {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with a `pCreateInfo` whose `sType` is \
             {stype}, and `{stype_name}` is {expected_stype}. `VkFenceCreateInfo` and \
             `VkSemaphoreCreateInfo` are the same twenty-four bytes with the same three members, \
             so `sType` is the only thing that says which structure this is -- a mismatch here is \
             not a detail, it is the whole identification",
            caller = at.caller
        )));
    }
    let next = u64::from_le_bytes(info[8..16].try_into().expect("eight"));
    if next != 0 {
        return Err(at.refuse(format!(
            "the guest called `{call}` from {caller:#x} with `pCreateInfo->pNext = {next:#x}`. \
             {why_the_chain_matters}. This layer does not know those layouts, so it refuses and \
             names the address for the next run to decode",
            caller = at.caller
        )));
    }
    Ok(u32::from_le_bytes(info[16..20].try_into().expect("four")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The two structures are the same size and are told apart by `sType` alone**, which is the
    /// fact [`decode_flags_only`] exists because of.
    #[test]
    fn the_fence_and_semaphore_create_infos_are_indistinguishable_but_for_their_stype() {
        assert_eq!(SEMAPHORE_CREATE_INFO_BYTES, FENCE_CREATE_INFO_BYTES);
        assert_eq!(SEMAPHORE_CREATE_INFO_BYTES, 24);
        // `sType` at 0 (four bytes), four of padding, `pNext` at 8, `flags` at 16, four of
        // trailing padding because the structure's alignment is the pointer's.
        assert_eq!(16 + 4 + 4, SEMAPHORE_CREATE_INFO_BYTES);
        assert_ne!(STYPE_FENCE_CREATE_INFO, STYPE_SEMAPHORE_CREATE_INFO);
        assert_eq!(STYPE_FENCE_CREATE_INFO, 8);
        assert_eq!(STYPE_SEMAPHORE_CREATE_INFO, 9);
    }

    /// The per-call bound is above the number of fences that can exist, so a conforming guest
    /// cannot reach it.
    #[test]
    fn no_conforming_guest_can_reach_the_per_call_fence_bound() {
        assert_eq!(MAX_FENCES_PER_CALL, 32);
        assert_eq!(super::super::MAX_FENCES, 16);
        // Twice the registry's bound, stated as the arithmetic: a `>` between two constants is
        // folded away, and the relation is the thing worth saying.
        assert_eq!(MAX_FENCES_PER_CALL, super::super::MAX_FENCES * 2);
        // The largest read the bound permits is a quarter of a page.
        assert_eq!(MAX_FENCES_PER_CALL * 8, 256);
    }
}
