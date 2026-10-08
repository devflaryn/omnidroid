//! The Vulkan commands forwarding cannot pass straight through (`tools/vk/special.txt`): the ones
//! that make or consume dispatchable handles, filter what the guest is offered, or implement the
//! Android extensions no host driver has. The guest's half is `device/src/vk/special.c`.
//!
//! Struct offsets below are the Vulkan structs' (one layout on the guest and the host).
use std::ffi::{c_char, c_void, CStr, CString};

use super::generated::{self as g, EXTENSIONS};
use super::{CallError, Gpu, Kind, Table};
use crate::process::Process;

/// VK_ANDROID_native_buffer's commands, which vk.xml does not number (the guest's `driver.h`).
pub(crate) const ID_GRALLOC_USAGE: u32 = 0x1000;
const ID_GRALLOC_USAGE2: u32 = 0x1001;
const ID_GRALLOC_USAGE3: u32 = 0x1002;
const ID_GRALLOC_USAGE4: u32 = 0x1003;
const ID_ACQUIRE_IMAGE: u32 = 0x1004;
const ID_QUEUE_SIGNAL_RELEASE_IMAGE: u32 = 0x1005;
/// A command buffer's batch of recorded commands (`super::BATCH_COMMAND`).
pub(crate) const ID_BATCH: u32 = 0x1006;
/// What the host asks of the guest's driver (`super::CONFIG_COMMAND`).
pub(crate) const ID_CONFIG: u32 = 0x1007;

/// The gralloc usage of a swapchain buffer: the GPU renders it and samples it
/// (`GRALLOC_USAGE_HW_TEXTURE | GRALLOC_USAGE_HW_RENDER`, gralloc1's consumer `GPU_TEXTURE` and
/// producer `GPU_RENDER_TARGET`).
const GRALLOC_USAGE_SWAPCHAIN: u64 = 0x100 | 0x200;

const VK_SUCCESS: i32 = 0;
const VK_INCOMPLETE: i32 = 5;
const VK_ERROR_LAYER_NOT_PRESENT: i32 = -6;
const VK_ERROR_FEATURE_NOT_PRESENT: i32 = -8;

const STYPE_NATIVE_BUFFER_ANDROID: u32 = 1_000_010_000;
const STYPE_SWAPCHAIN_IMAGE_CREATE_INFO_ANDROID: u32 = 1_000_010_001;
/// `VkImageSwapchainCreateInfoKHR` and `VkBindImageMemorySwapchainInfoKHR`: they name the
/// loader's swapchain, which no host driver knows.
const STYPE_IMAGE_SWAPCHAIN_CREATE_INFO: u32 = 1_000_060_008;
const STYPE_BIND_IMAGE_MEMORY_SWAPCHAIN_INFO: u32 = 1_000_060_009;
const STYPE_PRESENTATION_PROPERTIES_ANDROID: u32 = 1_000_010_002;
const STYPE_AHB_USAGE_ANDROID: u32 = 1_000_129_000;
const STYPE_IMPORT_AHB_INFO_ANDROID: u32 = 1_000_129_003;
const STYPE_EXTERNAL_FORMAT_ANDROID: u32 = 1_000_129_005;
const STYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO: u32 = 1_000_011_000;
const STYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO: u32 = 1_000_128_004;

/// `AHARDWAREBUFFER_USAGE_GPU_SAMPLED_IMAGE | GPU_COLOR_OUTPUT`.
const AHB_USAGE_GPU: u64 = (1 << 8) | (1 << 9);

type R<T> = Result<T, CallError>;

fn result(r: i32) -> u64 {
    u64::from(r as u32)
}

use super::entry_point as f;

fn rd_u32(p: &Process, at: u64) -> R<u32> {
    Ok(u32::from_le_bytes(p.mem.read(at, 4).map_err(|_| CallError::Args)?.try_into().expect("4")))
}

fn rd_u64(p: &Process, at: u64) -> R<u64> {
    p.mem.read_u64(at).map_err(|_| CallError::Args)
}

fn wr(p: &Process, at: u64, bytes: &[u8]) -> R<()> {
    p.mem.write(at, bytes).map_err(|_| CallError::Args)
}

/// The guest's C strings at `names[0..count]`.
fn strings(p: &Process, names: u64, count: u32) -> R<Vec<CString>> {
    (0..u64::from(count))
        .map(|i| {
            let at = rd_u64(p, names + i * 8)?;
            let s = p.mem.read_cstr(at, 256).map_err(|_| CallError::Args)?;
            CString::new(s).map_err(|_| CallError::Args)
        })
        .collect()
}

/// A `pNext` chain in guest memory: the address of the `pNext` field that points at the first
/// node of type `stype` in the chain after `head`, and that node.
fn chain_find(p: &Process, head: u64, stype: u32) -> R<Option<(u64, u64)>> {
    let mut link = head + 8;
    for _ in 0..64 {
        let node = rd_u64(p, link)?;
        if node == 0 {
            return Ok(None);
        }
        if rd_u32(p, node)? == stype {
            return Ok(Some((link, node)));
        }
        link = node + 8;
    }
    Err(CallError::Args)
}

/// A node taken out of a guest chain for a host call, put back when this is dropped. The host
/// driver must not see a structure it does not know; the guest's memory is as it was after the
/// call.
struct Spliced<'a> {
    p: &'a Process,
    link: u64,
    node: u64,
}

impl Drop for Spliced<'_> {
    fn drop(&mut self) {
        let _ = self.p.mem.write(self.link, &self.node.to_le_bytes());
    }
}

fn splice(p: &Process, head: u64, stype: u32) -> R<Option<Spliced<'_>>> {
    let Some((link, node)) = chain_find(p, head, stype)? else { return Ok(None) };
    let next = rd_u64(p, node + 8)?;
    wr(p, link, &next.to_le_bytes())?;
    Ok(Some(Spliced { p, link, node }))
}

/// The extensions a host enumeration answered, as (name, spec version).
fn host_extensions(bytes: &[u8]) -> Vec<(String, u32)> {
    bytes
        .chunks_exact(260)
        .map(|e| {
            let name = e[..256].split(|&b| b == 0).next().unwrap_or_default();
            (String::from_utf8_lossy(name).into_owned(), u32::from_le_bytes(e[256..260].try_into().expect("4")))
        })
        .collect()
}

/// Answer an extension enumeration: `offered` into the guest's `pCount`/`pProps`.
fn write_extensions(p: &Process, count_at: u64, props_at: u64, offered: &[(String, u32)]) -> R<u64> {
    if props_at == 0 {
        wr(p, count_at, &(offered.len() as u32).to_le_bytes())?;
        return Ok(result(VK_SUCCESS));
    }
    let room = rd_u32(p, count_at)? as usize;
    let n = room.min(offered.len());
    for (i, (name, version)) in offered[..n].iter().enumerate() {
        let mut e = [0u8; 260];
        e[..name.len().min(255)].copy_from_slice(&name.as_bytes()[..name.len().min(255)]);
        e[256..].copy_from_slice(&version.to_le_bytes());
        wr(p, props_at + i as u64 * 260, &e)?;
    }
    wr(p, count_at, &(n as u32).to_le_bytes())?;
    Ok(result(if n < offered.len() { VK_INCOMPLETE } else { VK_SUCCESS }))
}

/// Whether the guest is offered extension `name` of kind `device` when the host has it.
fn forwarded(name: &str, device: bool) -> bool {
    EXTENSIONS.iter().any(|(n, d)| *n == name && *d == device) && !EMULATED.iter().any(|(e, _)| *e == name)
}

/// Extensions implemented here rather than by the host driver: offered whatever the host has,
/// and never enabled on it. (name, spec version)
const EMULATED: &[(&str, u32)] = &[
    ("VK_ANDROID_native_buffer", 8),
    ("VK_ANDROID_external_memory_android_hardware_buffer", 5),
    ("VK_KHR_external_semaphore_fd", 1),
    ("VK_KHR_external_fence_fd", 1),
];

const STYPE_EXPORT_SEMAPHORE_CREATE_INFO: u32 = 1_000_077_000;
const STYPE_EXPORT_FENCE_CREATE_INFO: u32 = 1_000_113_000;
const STYPE_EXPORT_MEMORY_ALLOCATE_INFO: u32 = 1_000_072_002;
const STYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO: u32 = 1_000_071_000;
const STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES: u32 = 1_000_071_001;
/// `VK_EXTERNAL_SEMAPHORE_HANDLE_TYPE_SYNC_FD_BIT`, `VK_EXTERNAL_FENCE_HANDLE_TYPE_SYNC_FD_BIT`.
const SEMAPHORE_SYNC_FD: u32 = 0x10;
const FENCE_SYNC_FD: u32 = 0x8;
const VK_ERROR_OUT_OF_HOST_MEMORY: i32 = -1;

type Enumerate = unsafe extern "system" fn(*const c_char, *mut u32, *mut u8) -> i32;
type EnumerateDev = unsafe extern "system" fn(u64, *const c_char, *mut u32, *mut u8) -> i32;

fn enumerate_host_instance_extensions() -> R<Vec<(String, u32)>> {
    const NAMES: &[&CStr] = &[c"vkEnumerateInstanceExtensionProperties"];
    let t = Table::instance(0)?;
    // SAFETY: the Vulkan signature.
    let e: Enumerate = unsafe { f(&t, g::ID_VK_ENUMERATE_INSTANCE_EXTENSION_PROPERTIES, NAMES)? };
    let mut n = 0u32;
    // SAFETY: a count query.
    unsafe { e(std::ptr::null(), &mut n, std::ptr::null_mut()) };
    let mut buf = vec![0u8; n as usize * 260];
    // SAFETY: `buf` holds `n` properties.
    unsafe { e(std::ptr::null(), &mut n, buf.as_mut_ptr()) };
    buf.truncate(n as usize * 260);
    Ok(host_extensions(&buf))
}

fn enumerate_host_device_extensions(t: &Table, pd: u64) -> R<Vec<(String, u32)>> {
    const NAMES: &[&CStr] = &[c"vkEnumerateDeviceExtensionProperties"];
    // SAFETY: the Vulkan signature.
    let e: EnumerateDev = unsafe { f(t, g::ID_VK_ENUMERATE_DEVICE_EXTENSION_PROPERTIES, NAMES)? };
    let mut n = 0u32;
    // SAFETY: a count query of a live physical device.
    unsafe { e(pd, std::ptr::null(), &mut n, std::ptr::null_mut()) };
    let mut buf = vec![0u8; n as usize * 260];
    // SAFETY: `buf` holds `n` properties.
    unsafe { e(pd, std::ptr::null(), &mut n, buf.as_mut_ptr()) };
    buf.truncate(n as usize * 260);
    Ok(host_extensions(&buf))
}

pub(crate) fn call(gpu: &Gpu, p: &Process, id: u32, a: &[u64]) -> R<u64> {
    match id {
        g::ID_VK_ENUMERATE_INSTANCE_VERSION => {
            const NAMES: &[&CStr] = &[c"vkEnumerateInstanceVersion"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(*mut u32) -> i32 = unsafe { f(&*Table::instance(0)?, id, NAMES)? };
            let mut v = 0u32;
            // SAFETY: a host out-pointer.
            let r = unsafe { e(&mut v) };
            wr(p, a[0], &v.to_le_bytes())?;
            Ok(result(r))
        }
        g::ID_VK_ENUMERATE_INSTANCE_EXTENSION_PROPERTIES => {
            if a[0] != 0 {
                return Ok(result(VK_ERROR_LAYER_NOT_PRESENT));
            }
            let offered: Vec<_> = enumerate_host_instance_extensions()?.into_iter().filter(|(n, _)| forwarded(n, false)).collect();
            write_extensions(p, a[1], a[2], &offered)
        }
        g::ID_VK_CREATE_INSTANCE => create_instance(gpu, p, a),
        g::ID_VK_DESTROY_INSTANCE => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkDestroyInstance"];
            // SAFETY: the Vulkan signature; `h` is a live host instance.
            let d: unsafe extern "system" fn(u64, *const c_void) = unsafe { f(&t, id, NAMES)? };
            unsafe { d(h, std::ptr::null()) };
            gpu.retain_objects(|k, o| *k != h && o.parent != h);
            Ok(0)
        }
        g::ID_VK_ENUMERATE_PHYSICAL_DEVICES => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkEnumeratePhysicalDevices"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, *mut u32, *mut u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            let mut n = if a[2] == 0 { 0 } else { rd_u32(p, a[1])? };
            let mut hosts = vec![0u64; n as usize];
            // SAFETY: `hosts` holds `n` handles (or is not asked for).
            let r = unsafe { e(h, &mut n, if a[2] == 0 { std::ptr::null_mut() } else { hosts.as_mut_ptr() }) };
            if a[2] != 0 {
                hosts.truncate(n as usize);
                for &pd in &hosts {
                    gpu.add(pd, Kind::PhysicalDevice, h, std::sync::Arc::clone(&t));
                }
                let bytes: Vec<u8> = hosts.iter().flat_map(|x| x.to_le_bytes()).collect();
                wr(p, a[2], &bytes)?;
            }
            wr(p, a[1], &n.to_le_bytes())?;
            Ok(result(r))
        }
        g::ID_VK_ENUMERATE_PHYSICAL_DEVICE_GROUPS => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkEnumeratePhysicalDeviceGroups", c"vkEnumeratePhysicalDeviceGroupsKHR"];
            // SAFETY: the Vulkan signature; the guest's array is host memory the driver fills.
            let e: unsafe extern "system" fn(u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            let r = unsafe { e(h, a[1], a[2]) };
            if a[2] != 0 {
                // VkPhysicalDeviceGroupProperties: count at 16, handles from 24, 32 of them, 288 bytes.
                let n = rd_u32(p, a[1])?;
                for gi in 0..u64::from(n) {
                    let at = a[2] + gi * 288;
                    for i in 0..u64::from(rd_u32(p, at + 16)?.min(32)) {
                        gpu.add(rd_u64(p, at + 24 + i * 8)?, Kind::PhysicalDevice, h, std::sync::Arc::clone(&t));
                    }
                }
            }
            Ok(result(r))
        }
        g::ID_VK_ENUMERATE_DEVICE_EXTENSION_PROPERTIES => {
            if a[1] != 0 {
                return Ok(result(VK_ERROR_LAYER_NOT_PRESENT));
            }
            let (pd, t) = gpu.dispatchable(p, a[0])?;
            let host = enumerate_host_device_extensions(&t, pd)?;
            let host_has_foreign = host.iter().any(|(n, _)| n == QUEUE_FAMILY_FOREIGN);
            let mut offered: Vec<_> = host.into_iter().filter(|(n, _)| forwarded(n, true)).collect();
            offered.extend(EMULATED.iter().map(|(n, v)| ((*n).to_string(), *v)));
            if !host_has_foreign {
                offered.push((QUEUE_FAMILY_FOREIGN.to_string(), 1));
            }
            write_extensions(p, a[2], a[3], &offered)
        }
        g::ID_VK_CREATE_DEVICE => create_device(gpu, p, a),
        g::ID_VK_DESTROY_DEVICE => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkDestroyDevice"];
            // SAFETY: the Vulkan signature; `h` is a live host device.
            let d: unsafe extern "system" fn(u64, *const c_void) = unsafe { f(&t, id, NAMES)? };
            unsafe { d(h, std::ptr::null()) };
            gpu.retain_objects(|k, o| *k != h && o.parent != h);
            gpu.devices.lock().remove(&h);
            gpu.native.lock().retain(|_, n| !n.belongs_to(h));
            super::ahb::forget_device(gpu, h);
            Ok(0)
        }
        g::ID_VK_GET_DEVICE_QUEUE | g::ID_VK_GET_DEVICE_QUEUE2 => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let mut q = 0u64;
            if id == g::ID_VK_GET_DEVICE_QUEUE {
                const NAMES: &[&CStr] = &[c"vkGetDeviceQueue"];
                // SAFETY: the Vulkan signature.
                let e: unsafe extern "system" fn(u64, u32, u32, *mut u64) = unsafe { f(&t, id, NAMES)? };
                unsafe { e(h, a[1] as u32, a[2] as u32, &mut q) };
            } else {
                const NAMES: &[&CStr] = &[c"vkGetDeviceQueue2"];
                // SAFETY: the Vulkan signature; the info struct is the guest's.
                let e: unsafe extern "system" fn(u64, u64, *mut u64) = unsafe { f(&t, id, NAMES)? };
                unsafe { e(h, a[1], &mut q) };
            }
            if q != 0 {
                let family = if id == g::ID_VK_GET_DEVICE_QUEUE { a[1] as u32 } else { rd_u32(p, a[1] + 20)? };
                if let Some(info) = gpu.devices.lock().get_mut(&h) {
                    info.queues.insert(q, family);
                }
                gpu.add(q, Kind::Queue, h, t);
            }
            wr(p, *a.last().expect("argc checked"), &q.to_le_bytes())?;
            Ok(0)
        }
        g::ID_VK_ALLOCATE_COMMAND_BUFFERS => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkAllocateCommandBuffers"];
            // SAFETY: the Vulkan signature; the driver writes the handles into the guest's array.
            let e: unsafe extern "system" fn(u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            let r = unsafe { e(h, a[1], a[2]) };
            if r == VK_SUCCESS {
                let n = rd_u32(p, a[1] + 28)?;
                for i in 0..u64::from(n) {
                    gpu.add(rd_u64(p, a[2] + i * 8)?, Kind::CommandBuffer, h, std::sync::Arc::clone(&t));
                }
            }
            Ok(result(r))
        }
        g::ID_VK_FREE_COMMAND_BUFFERS => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let hosts = unwrap_array(gpu, p, a[3], a[2] as u32)?;
            const NAMES: &[&CStr] = &[c"vkFreeCommandBuffers"];
            // SAFETY: the Vulkan signature; `hosts` are live command buffers of `h`.
            let e: unsafe extern "system" fn(u64, u64, u32, *const u64) = unsafe { f(&t, id, NAMES)? };
            unsafe { e(h, a[1], hosts.len() as u32, hosts.as_ptr()) };
            for cb in hosts {
                gpu.remove(cb);
            }
            Ok(0)
        }
        g::ID_VK_CMD_EXECUTE_COMMANDS => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let hosts = unwrap_array(gpu, p, a[2], a[1] as u32)?;
            const NAMES: &[&CStr] = &[c"vkCmdExecuteCommands"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u32, *const u64) = unsafe { f(&t, id, NAMES)? };
            unsafe { e(h, hosts.len() as u32, hosts.as_ptr()) };
            Ok(0)
        }
        g::ID_VK_QUEUE_SUBMIT => queue_submit(gpu, p, a),
        g::ID_VK_QUEUE_SUBMIT2 => queue_submit2(gpu, p, a),
        g::ID_VK_GET_PHYSICAL_DEVICE_PROPERTIES2 => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let presentation = splice(p, a[1], STYPE_PRESENTATION_PROPERTIES_ANDROID)?;
            const NAMES: &[&CStr] = &[c"vkGetPhysicalDeviceProperties2", c"vkGetPhysicalDeviceProperties2KHR"];
            // SAFETY: the Vulkan signature; the chain is the guest's, minus what the host lacks.
            let e: unsafe extern "system" fn(u64, u64) = unsafe { f(&t, id, NAMES)? };
            unsafe { e(h, a[1]) };
            if let Some(s) = presentation {
                // No shared presentable images.
                wr(p, s.node + 16, &0u32.to_le_bytes())?;
            }
            Ok(0)
        }
        g::ID_VK_GET_PHYSICAL_DEVICE_IMAGE_FORMAT_PROPERTIES2 => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let usage = splice(p, a[2], STYPE_AHB_USAGE_ANDROID)?;
            // Gralloc buffers are importable (and only by a dedicated allocation); the host driver
            // is asked about the image without that handle type.
            let external = match chain_find(p, a[1], STYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO)? {
                Some((_, node)) if rd_u32(p, node + 16)? == super::ahb::HANDLE_TYPE_AHB => splice(p, a[1], STYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO)?,
                _ => None,
            };
            let external_props = if external.is_some() { splice(p, a[2], STYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES)? } else { None };
            const NAMES: &[&CStr] = &[c"vkGetPhysicalDeviceImageFormatProperties2", c"vkGetPhysicalDeviceImageFormatProperties2KHR"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            let r = unsafe { e(h, a[1], a[2]) };
            if let Some(s) = usage {
                wr(p, s.node + 16, &AHB_USAGE_GPU.to_le_bytes())?;
            }
            if let Some(s) = external_props {
                // VkExternalMemoryProperties at 16: features (DEDICATED_ONLY | IMPORTABLE),
                // exportFromImportedHandleTypes, compatibleHandleTypes.
                let ahb = super::ahb::HANDLE_TYPE_AHB;
                let mut b = [0u8; 12];
                b[0..4].copy_from_slice(&(0x1u32 | 0x4).to_le_bytes());
                b[4..8].copy_from_slice(&ahb.to_le_bytes());
                b[8..12].copy_from_slice(&ahb.to_le_bytes());
                wr(p, s.node + 16, &b)?;
            }
            Ok(result(r))
        }
        g::ID_VK_CREATE_IMAGE => {
            // An external format (a YUV layout only the vendor's GPU knows) is none here; 0 means
            // "no external format" and is dropped.
            if let Some((_, node)) = chain_find(p, a[1], STYPE_EXTERNAL_FORMAT_ANDROID)? {
                if rd_u64(p, node + 16)? != 0 {
                    return Ok(result(VK_ERROR_FEATURE_NOT_PRESENT));
                }
            }
            if std::env::var("OMNI_GPU_TRACE").as_deref() == Ok("1") {
                let mut link = a[1] + 8;
                let mut chain = Vec::new();
                while let Ok(node) = rd_u64(p, link) {
                    if node == 0 || chain.len() > 16 {
                        break;
                    }
                    chain.push((rd_u32(p, node).unwrap_or(0), rd_u32(p, node + 16).unwrap_or(0)));
                    link = node + 8;
                }
                eprintln!("[gpu] vkCreateImage chain (sType, first u32): {chain:?}; flags {:#x} tiling {} usage {:#x}", rd_u32(p, a[1] + 16)?, rd_u32(p, a[1] + 52)?, rd_u32(p, a[1] + 56)?);
            }
            let _external_format = splice(p, a[1], STYPE_EXTERNAL_FORMAT_ANDROID)?;
            if let Some((_, node)) = chain_find(p, a[1], super::ahb::STYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO)? {
                if rd_u32(p, node + 16)? & super::ahb::HANDLE_TYPE_AHB != 0 {
                    let _external = splice(p, a[1], super::ahb::STYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO)?;
                    let (h, t) = gpu.dispatchable(p, a[0])?;
                    let info = p.mem.read(a[1], 88).map_err(|_| CallError::Args)?;
                    return super::ahb::create_image(gpu, p, h, &t, &info, a[3]);
                }
            }
            let _swapchain = splice(p, a[1], STYPE_SWAPCHAIN_IMAGE_CREATE_INFO_ANDROID)?;
            if let Some(native) = splice(p, a[1], STYPE_NATIVE_BUFFER_ANDROID)? {
                let (h, t) = gpu.dispatchable(p, a[0])?;
                let info = p.mem.read(a[1], 88).map_err(|_| CallError::Args)?;
                return match super::native::create_image(gpu, p, h, &t, &info, native.node) {
                    Ok(image) => {
                        wr(p, a[3], &image.to_le_bytes())?;
                        Ok(result(VK_SUCCESS))
                    }
                    Err(CallError::Host(r)) => Ok(result(r)),
                    Err(e) => Err(e),
                };
            }
            // (Bound, so the structure stays out of the chain until the image is made.)
            if let Some(_swapchain_info) = splice(p, a[1], STYPE_IMAGE_SWAPCHAIN_CREATE_INFO)? {
                let (h, t) = gpu.dispatchable(p, a[0])?;
                let info = p.mem.read(a[1], 88).map_err(|_| CallError::Args)?;
                return match super::native::create_swapchain_image(gpu, h, &t, &info) {
                    Ok(image) => {
                        wr(p, a[3], &image.to_le_bytes())?;
                        Ok(result(VK_SUCCESS))
                    }
                    Err(CallError::Host(r)) => Ok(result(r)),
                    Err(e) => Err(e),
                };
            }
            passthrough4(gpu, p, id, a, &[c"vkCreateImage"])
        }
        g::ID_VK_DESTROY_IMAGE => {
            gpu.swapchain_images.lock().remove(&a[1]);
            let (h, t) = gpu.dispatchable(p, a[0])?;
            if !super::native::destroy_image(gpu, &t, a[1])? {
                const NAMES: &[&CStr] = &[c"vkDestroyImage"];
                // SAFETY: the Vulkan signature.
                let e: unsafe extern "system" fn(u64, u64, *const c_void) = unsafe { f(&t, id, NAMES)? };
                unsafe { e(h, a[1], std::ptr::null()) };
            }
            Ok(0)
        }
        g::ID_VK_ALLOCATE_MEMORY => {
            if let Some(import) = splice(p, a[1], STYPE_IMPORT_AHB_INFO_ANDROID)? {
                // The guest passes the buffer's native handle in the allocator's place.
                let (h, t) = gpu.dispatchable(p, a[0])?;
                let _ = import;
                return match super::ahb::import(gpu, p, h, &t, a[1], a[2], a[3]) {
                    Err(CallError::Host(r)) => Ok(result(r)),
                    other => other,
                };
            }
            if let Some((_, node)) = chain_find(p, a[1], STYPE_EXPORT_MEMORY_ALLOCATE_INFO)? {
                if rd_u32(p, node + 16)? & super::ahb::HANDLE_TYPE_AHB != 0 {
                    // Exporting a gralloc buffer from device memory is not offered.
                    return Ok(result(VK_ERROR_OUT_OF_HOST_MEMORY));
                }
            }
            let r = passthrough4(gpu, p, id, a, &[c"vkAllocateMemory"])?;
            if r == result(VK_SUCCESS) && device_memory::on() {
                // VkMemoryAllocateInfo: allocationSize at 16, memoryTypeIndex at 24.
                let (size, index) = (rd_u64(p, a[1] + 16)?, rd_u32(p, a[1] + 24)?);
                let (host_device, _) = gpu.dispatchable(p, a[0])?;
                let flags = gpu.devices.lock().get(&host_device).and_then(|d| d.memory_types.get(index as usize).map(|m| m.property_flags.as_raw()));
                device_memory::allocated(rd_u64(p, a[3])?, size, index, flags.unwrap_or(0));
            }
            Ok(r)
        }
        g::ID_VK_FREE_MEMORY => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            super::ahb::forget_memory(gpu, a[1]);
            device_memory::freed(a[1]);
            const NAMES: &[&CStr] = &[c"vkFreeMemory"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, *const c_void) = unsafe { f(&t, id, NAMES)? };
            unsafe { e(h, a[1], std::ptr::null()) };
            Ok(0)
        }
        g::ID_VK_GET_ANDROID_HARDWARE_BUFFER_PROPERTIES_ANDROID => super::ahb::properties(gpu, p, a),
        g::ID_VK_GET_MEMORY_ANDROID_HARDWARE_BUFFER_ANDROID => Ok(result(VK_ERROR_OUT_OF_HOST_MEMORY)),
        g::ID_VK_CREATE_SEMAPHORE => {
            let _export = splice(p, a[1], STYPE_EXPORT_SEMAPHORE_CREATE_INFO)?;
            passthrough4(gpu, p, id, a, &[c"vkCreateSemaphore"])
        }
        g::ID_VK_CREATE_FENCE => {
            let _export = splice(p, a[1], STYPE_EXPORT_FENCE_CREATE_INFO)?;
            passthrough4(gpu, p, id, a, &[c"vkCreateFence"])
        }
        g::ID_VK_GET_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_PROPERTIES | g::ID_VK_GET_PHYSICAL_DEVICE_EXTERNAL_FENCE_PROPERTIES => {
            let semaphore = id == g::ID_VK_GET_PHYSICAL_DEVICE_EXTERNAL_SEMAPHORE_PROPERTIES;
            let sync_fd = if semaphore { SEMAPHORE_SYNC_FD } else { FENCE_SYNC_FD };
            if rd_u32(p, a[1] + 16)? == sync_fd {
                // Emulated here: exportable and importable, of itself only.
                let mut b = [0u8; 12];
                b[0..4].copy_from_slice(&sync_fd.to_le_bytes());
                b[4..8].copy_from_slice(&sync_fd.to_le_bytes());
                b[8..12].copy_from_slice(&0x3u32.to_le_bytes());
                wr(p, a[2] + 16, &b)?;
                return Ok(0);
            }
            let (h, t) = gpu.dispatchable(p, a[0])?;
            let names: &'static [&'static CStr] = if semaphore {
                &[c"vkGetPhysicalDeviceExternalSemaphoreProperties", c"vkGetPhysicalDeviceExternalSemaphorePropertiesKHR"]
            } else {
                &[c"vkGetPhysicalDeviceExternalFenceProperties", c"vkGetPhysicalDeviceExternalFencePropertiesKHR"]
            };
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, u64) = unsafe { f(&t, id, names)? };
            unsafe { e(h, a[1], a[2]) };
            Ok(0)
        }
        g::ID_VK_GET_SEMAPHORE_FD_KHR | g::ID_VK_GET_FENCE_FD_KHR => export_sync_fd(gpu, p, id, a),
        g::ID_VK_IMPORT_SEMAPHORE_FD_KHR | g::ID_VK_IMPORT_FENCE_FD_KHR => import_sync_fd(gpu, p, id, a),
        g::ID_VK_BIND_IMAGE_MEMORY => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkBindImageMemory"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            Ok(result(unsafe { e(h, a[1], a[2], a[3]) }))
        }
        g::ID_VK_BIND_IMAGE_MEMORY2 => bind_image_memory2(gpu, p, a),
        _ => {
            let name = g::COMMANDS.get(id as usize).map_or("?", |c| c.0);
            Err(CallError::Missing(name))
        }
    }
}

/// VK_ANDROID_native_buffer's commands (ids from [`ID_GRALLOC_USAGE`]).
pub(crate) fn extra(gpu: &Gpu, p: &Process, id: u32, a: &[u64]) -> R<u64> {
    let argc = match id {
        ID_GRALLOC_USAGE | ID_GRALLOC_USAGE3 | ID_GRALLOC_USAGE4 => if id == ID_GRALLOC_USAGE { 4 } else { 3 },
        ID_GRALLOC_USAGE2 => 6,
        ID_ACQUIRE_IMAGE | ID_QUEUE_SIGNAL_RELEASE_IMAGE => 5,
        _ => return Err(CallError::Args),
    };
    if a.len() != argc {
        return Err(CallError::Args);
    }
    gpu.dispatchable(p, a[0])?;
    let native = |r: R<u64>| match r {
        Err(CallError::Host(v)) => Ok(result(v)),
        other => other,
    };
    match id {
        // (device, format, imageUsage, int* grallocUsage)
        ID_GRALLOC_USAGE => wr(p, a[3], &(GRALLOC_USAGE_SWAPCHAIN as i32).to_le_bytes()).map(|()| 0),
        // (device, format, imageUsage, swapchainUsage, u64* consumer, u64* producer)
        ID_GRALLOC_USAGE2 => {
            wr(p, a[4], &0x100u64.to_le_bytes())?;
            wr(p, a[5], &0x200u64.to_le_bytes()).map(|()| 0)
        }
        // (device, info, u64* grallocUsage)
        ID_GRALLOC_USAGE3 | ID_GRALLOC_USAGE4 => wr(p, a[2], &GRALLOC_USAGE_SWAPCHAIN.to_le_bytes()).map(|()| 0),
        ID_ACQUIRE_IMAGE => native(super::native::acquire(gpu, p, a)),
        _ => native(super::native::release(gpu, p, a)),
    }
}

/// A command `(dispatchable, create info, allocator, out handle)` passed through, with no allocator.
fn passthrough4(gpu: &Gpu, p: &Process, id: u32, a: &[u64], names: &'static [&'static CStr]) -> R<u64> {
    let (h, t) = gpu.dispatchable(p, a[0])?;
    // SAFETY: `(handle, const T*, const VkAllocationCallbacks*, U*) -> VkResult`, as every such
    // create command is; the out-pointer is the guest's.
    let e: unsafe extern "system" fn(u64, u64, *const c_void, u64) -> i32 = unsafe { f(&t, id, names)? };
    Ok(result(unsafe { e(h, a[1], std::ptr::null(), a[3]) }))
}

/// `OMNI_GPU_MEM=<seconds>`: the app's live device memory by memory type (index, property flags:
/// 1 device-local, 2 host-visible, 4 coherent, 8 cached), every so often -- which of a host
/// process's commit is its own Vulkan allocations in host memory.
mod device_memory {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::OnceLock;
    use std::time::Duration;

    use parking_lot::Mutex;

    /// Live allocations: memory -> (size, type index, flags).
    static LIVE: Mutex<Option<HashMap<u64, (u64, u32, u32)>>> = Mutex::new(None);

    pub(super) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| {
            let Some(every) = std::env::var("OMNI_GPU_MEM").ok().and_then(|v| v.parse::<u64>().ok()) else { return false };
            *LIVE.lock() = Some(HashMap::new());
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(every.max(1)));
                report();
            });
            true
        })
    }

    pub(super) fn allocated(memory: u64, size: u64, index: u32, flags: u32) {
        if let Some(live) = LIVE.lock().as_mut() {
            live.insert(memory, (size, index, flags));
        }
    }

    pub(super) fn freed(memory: u64) {
        if let Some(live) = LIVE.lock().as_mut() {
            live.remove(&memory);
        }
    }

    fn report() {
        let by: BTreeMap<(u32, u32), (u64, u64, u64)> = {
            let live = LIVE.lock();
            let mut by = BTreeMap::new();
            for &(size, index, flags) in live.as_ref().map(|l| l.values()).into_iter().flatten() {
                let e: &mut (u64, u64, u64) = by.entry((index, flags)).or_default();
                e.0 += 1;
                e.1 += size;
                e.2 = e.2.max(size);
            }
            by
        };
        if by.is_empty() {
            return;
        }
        let rows: Vec<String> = by.iter().map(|((i, f), (n, bytes, max))| format!("type {i} (flags {f:#x}): {n} allocations {} MiB (largest {} KiB)", bytes >> 20, max >> 10)).collect();
        eprintln!("[gpu-mem] host pid {}: {}", std::process::id(), rows.join("; "));
    }
}

/// The host command buffers behind `count` guest wrappers at `at`.
fn unwrap_array(gpu: &Gpu, p: &Process, at: u64, count: u32) -> R<Vec<u64>> {
    if count > 1 << 16 {
        return Err(CallError::Args);
    }
    (0..u64::from(count)).map(|i| gpu.dispatchable_of(p, rd_u64(p, at + i * 8)?, Kind::CommandBuffer)).collect()
}

fn create_instance(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let ci = p.mem.read(a[0], 64).map_err(|_| CallError::Args)?;
    let at = |o: usize| u64::from_le_bytes(ci[o..o + 8].try_into().expect("8"));
    let ext_count = u32::from_le_bytes(ci[48..52].try_into().expect("4"));
    if ext_count > 256 {
        return Err(CallError::Args);
    }
    let host_has = enumerate_host_instance_extensions()?;
    let mut names: Vec<CString> =
        strings(p, at(56), ext_count)?.into_iter().filter(|n| host_has.iter().any(|(h, _)| h.as_bytes() == n.as_bytes())).collect();
    // A portability driver (MoltenVK) is enumerated only when asked for.
    let mut flags = u32::from_le_bytes(ci[16..20].try_into().expect("4"));
    if host_has.iter().any(|(h, _)| h == "VK_KHR_portability_enumeration") {
        names.push(c"VK_KHR_portability_enumeration".to_owned());
        flags |= 1;
    }
    let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
    // Debug callbacks are guest code the host cannot call.
    let _debug_report = splice(p, a[0], STYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO)?;
    let _debug_utils = splice(p, a[0], STYPE_DEBUG_UTILS_MESSENGER_CREATE_INFO)?;
    let mut host_ci = ci.clone();
    host_ci[8..16].copy_from_slice(&rd_u64(p, a[0] + 8)?.to_le_bytes());
    host_ci[16..20].copy_from_slice(&flags.to_le_bytes());
    host_ci[32..36].copy_from_slice(&0u32.to_le_bytes()); // no layers
    host_ci[40..48].copy_from_slice(&0u64.to_le_bytes());
    host_ci[48..52].copy_from_slice(&(ptrs.len() as u32).to_le_bytes());
    host_ci[56..64].copy_from_slice(&(ptrs.as_ptr() as u64).to_le_bytes());
    const NAMES: &[&CStr] = &[c"vkCreateInstance"];
    let t0 = Table::instance(0)?;
    // SAFETY: the Vulkan signature; `host_ci` is a VkInstanceCreateInfo whose names are host
    // strings and whose application info and chain are the guest's (one layout).
    let e: unsafe extern "system" fn(*const u8, *const c_void, *mut u64) -> i32 = unsafe { f(&t0, g::ID_VK_CREATE_INSTANCE, NAMES)? };
    let mut instance = 0u64;
    let r = unsafe { e(host_ci.as_ptr(), std::ptr::null(), &mut instance) };
    if r == VK_SUCCESS {
        gpu.add(instance, Kind::Instance, instance, Table::instance(instance)?);
        wr(p, a[2], &instance.to_le_bytes())?;
    }
    Ok(result(r))
}

fn create_device(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (pd, t) = gpu.dispatchable(p, a[0])?;
    let ci = p.mem.read(a[1], 72).map_err(|_| CallError::Args)?;
    let at = |o: usize| u64::from_le_bytes(ci[o..o + 8].try_into().expect("8"));
    let ext_count = u32::from_le_bytes(ci[48..52].try_into().expect("4"));
    if ext_count > 1024 {
        return Err(CallError::Args);
    }
    let asked = strings(p, at(56), ext_count)?;
    // VK_EXT_queue_family_foreign on a host without it (MoltenVK) is this layer's (`foreign_barrier`).
    let emulate_foreign = asked.iter().any(|n| n.as_bytes() == QUEUE_FAMILY_FOREIGN.as_bytes())
        && !enumerate_host_device_extensions(&t, pd)?.iter().any(|(n, _)| n == QUEUE_FAMILY_FOREIGN);
    let mut names: Vec<CString> = asked
        .into_iter()
        .filter(|n| !EMULATED.iter().any(|(e, _)| e.as_bytes() == n.as_bytes()))
        .filter(|n| !(emulate_foreign && n.as_bytes() == QUEUE_FAMILY_FOREIGN.as_bytes()))
        .collect();
    // `gralloc_direct` (`super::native::DIRECT`): the host device also able to import a gralloc
    // region's view as memory -- host-only extensions the guest never sees -- when asked for and
    // offered; made again without them if the driver refuses.
    let (mut import, mut appended) = (false, Vec::new());
    if super::native::direct_wanted() {
        let offered = enumerate_host_device_extensions(&t, pd)?;
        let has = |n: &str| offered.iter().any(|(o, _)| o == n);
        if has(EXTERNAL_MEMORY_HOST) {
            for extra in [EXTERNAL_MEMORY, EXTERNAL_MEMORY_HOST] {
                if has(extra) && !names.iter().any(|n| n.as_bytes() == extra.as_bytes()) {
                    names.push(CString::new(extra).expect("no NUL"));
                    appended.push(extra);
                }
            }
            import = true;
        }
    }
    const CREATE: &[&CStr] = &[c"vkCreateDevice"];
    // SAFETY: the Vulkan signature.
    let e: unsafe extern "system" fn(u64, *const u8, *const c_void, *mut u64) -> i32 = unsafe { f(&t, g::ID_VK_CREATE_DEVICE, CREATE)? };
    let create = |names: &[CString]| {
        let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
        let mut host_ci = ci.clone();
        host_ci[32..36].copy_from_slice(&0u32.to_le_bytes()); // no layers
        host_ci[40..48].copy_from_slice(&0u64.to_le_bytes());
        host_ci[48..52].copy_from_slice(&(ptrs.len() as u32).to_le_bytes());
        host_ci[56..64].copy_from_slice(&(ptrs.as_ptr() as u64).to_le_bytes());
        let mut device = 0u64;
        // SAFETY: `host_ci` is a VkDeviceCreateInfo with host extension names that outlive the call.
        let r = unsafe { e(pd, host_ci.as_ptr(), std::ptr::null(), &mut device) };
        (r, device)
    };
    let (mut r, mut device) = create(&names);
    if import && r != VK_SUCCESS && !appended.is_empty() {
        names.retain(|n| !appended.iter().any(|x: &&str| n.as_bytes() == x.as_bytes()));
        eprintln!("[gpu] gralloc_direct: the host refused a device with {EXTERNAL_MEMORY_HOST} ({r}); made without it");
        (r, device) = create(&names);
        import = false;
    }
    if r == VK_SUCCESS {
        const GDPA: &[&CStr] = &[c"vkGetDeviceProcAddr"];
        // SAFETY: `vkGetDeviceProcAddr`'s signature is `GetProcAddr`'s.
        let gdpa = unsafe { f(&t, g::ID_VK_GET_DEVICE_PROC_ADDR, GDPA)? };
        gpu.add(device, Kind::Device, device, Table::new(gdpa, device));
        const MEMORY: &[&CStr] = &[c"vkGetPhysicalDeviceMemoryProperties"];
        // SAFETY: the Vulkan signature.
        let mp: ash::vk::PFN_vkGetPhysicalDeviceMemoryProperties = unsafe { f(&t, g::ID_VK_GET_PHYSICAL_DEVICE_MEMORY_PROPERTIES, MEMORY)? };
        let mut memory = ash::vk::PhysicalDeviceMemoryProperties::default();
        unsafe { mp(ash::vk::Handle::from_raw(pd), &mut memory) };
        let mut info = super::native::DeviceInfo::new(&memory);
        info.host_import = import;
        if import {
            eprintln!("[gpu] gralloc_direct: device {device:#x} can import gralloc regions");
        }
        gpu.devices.lock().insert(device, info);
        if emulate_foreign {
            FOREIGN_EMULATED.lock().insert(device);
            ANY_FOREIGN_EMULATED.store(true, std::sync::atomic::Ordering::Release);
        }
        wr(p, a[3], &device.to_le_bytes())?;
    }
    Ok(result(r))
}

/// `VK_EXT_queue_family_foreign`.
const QUEUE_FAMILY_FOREIGN: &str = "VK_EXT_queue_family_foreign";

/// The host-only extensions `gralloc_direct` adds to a device (`super::native::DIRECT`).
const EXTERNAL_MEMORY_HOST: &str = "VK_EXT_external_memory_host";
const EXTERNAL_MEMORY: &str = "VK_KHR_external_memory";

/// Host devices on which `VK_EXT_queue_family_foreign` is emulated: the guest was offered it and
/// enabled it, and the host driver has none (MoltenVK). ANGLE turns on Android hardware-buffer
/// images only with it (`supportsAndroidHardwareBuffer` wants both extensions), and without those
/// SurfaceFlinger cannot compose an app's buffer (`eglCreateImageKHR` answered `EGL_BAD_PARAMETER`
/// on the M1).
static FOREIGN_EMULATED: parking_lot::Mutex<std::collections::BTreeSet<u64>> =
    parking_lot::Mutex::new(std::collections::BTreeSet::new());
/// Whether [`FOREIGN_EMULATED`] was ever given a device: what a barrier asks first. The lock is
/// global to the host process -- every barrier of every guest thread took it.
static ANY_FOREIGN_EMULATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `VK_QUEUE_FAMILY_FOREIGN_EXT`, `VK_QUEUE_FAMILY_EXTERNAL`, `VK_QUEUE_FAMILY_IGNORED`.
const QUEUE_FAMILY_FOREIGN_EXT: u32 = !2;
const QUEUE_FAMILY_EXTERNAL: u32 = !1;
const QUEUE_FAMILY_IGNORED: u32 = !0;

/// The barrier commands of a device on which `VK_EXT_queue_family_foreign` is emulated: `None`
/// (forward as generated) for any other command or device.
///
/// What the extension is for -- handing an image to "a queue outside this driver" -- is this layer's
/// business here, not the host driver's: the memory behind an Android hardware buffer is a gralloc
/// region the host keeps in step itself (`ahb`), and the host driver has no such queue. So a
/// transfer to or from `FOREIGN`/`EXTERNAL` becomes a plain barrier: both indices `IGNORED`, the
/// access masks and layout transition kept. The barriers are copied into host memory with the
/// indices changed; the guest's own structures are left as they were.
pub(crate) fn foreign_barrier(gpu: &Gpu, p: &Process, id: u32, a: &[u64]) -> Option<R<u64>> {
    if !matches!(id, g::ID_VK_CMD_PIPELINE_BARRIER | g::ID_VK_CMD_PIPELINE_BARRIER2 | g::ID_VK_CMD_WAIT_EVENTS | g::ID_VK_CMD_WAIT_EVENTS2) {
        return None;
    }
    // No such device in this host process (every NVIDIA/AMD host): no lock on a barrier's way.
    if !ANY_FOREIGN_EMULATED.load(std::sync::atomic::Ordering::Acquire) || FOREIGN_EMULATED.lock().is_empty() {
        return None;
    }
    let (h, t) = match gpu.dispatchable(p, a[0]) {
        Ok(d) => d,
        Err(e) => return Some(Err(e)),
    };
    if !FOREIGN_EMULATED.lock().contains(&t.owner()) {
        return None;
    }
    Some(foreign_barrier_on(p, id, a, h, &t))
}

/// `count` structures of `size` bytes at guest `at`, copied, with each `u32` at `offsets` that is a
/// foreign or external queue family made `IGNORED` -- the pair made `IGNORED` together, as a barrier
/// that transfers nothing must have it.
fn unforeign(p: &Process, at: u64, count: u32, size: usize, offsets: (usize, usize)) -> R<Vec<u8>> {
    if count == 0 || at == 0 {
        return Ok(Vec::new());
    }
    if count > 1 << 16 {
        return Err(CallError::Args);
    }
    let mut v = p.mem.read(at, count as usize * size).map_err(|_| CallError::Args)?;
    for s in v.chunks_exact_mut(size) {
        let q = |s: &[u8], o: usize| u32::from_le_bytes(s[o..o + 4].try_into().expect("4"));
        let (src, dst) = (q(s, offsets.0), q(s, offsets.1));
        if [src, dst].iter().any(|&i| i == QUEUE_FAMILY_FOREIGN_EXT || i == QUEUE_FAMILY_EXTERNAL) {
            s[offsets.0..offsets.0 + 4].copy_from_slice(&QUEUE_FAMILY_IGNORED.to_le_bytes());
            s[offsets.1..offsets.1 + 4].copy_from_slice(&QUEUE_FAMILY_IGNORED.to_le_bytes());
        }
    }
    Ok(v)
}

fn ptr_or_null(v: &[u8]) -> u64 {
    if v.is_empty() { 0 } else { v.as_ptr() as u64 }
}

/// A `VkDependencyInfo` (64 bytes) at guest `at`, copied with its buffer barriers (`VkBufferMemoryBarrier2`,
/// 80 bytes, indices at 48/52) and image barriers (`VkImageMemoryBarrier2`, 96 bytes, at 56/60)
/// made foreign-free. The returned buffers own the memory the copy points into.
fn unforeign_dependency(p: &Process, at: u64) -> R<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut info = p.mem.read(at, 64).map_err(|_| CallError::Args)?;
    let u32_at = |v: &[u8], o: usize| u32::from_le_bytes(v[o..o + 4].try_into().expect("4"));
    let u64_at = |v: &[u8], o: usize| u64::from_le_bytes(v[o..o + 8].try_into().expect("8"));
    let buffers = unforeign(p, u64_at(&info, 40), u32_at(&info, 32), 80, (48, 52))?;
    let images = unforeign(p, u64_at(&info, 56), u32_at(&info, 48), 96, (56, 60))?;
    info[40..48].copy_from_slice(&ptr_or_null(&buffers).to_le_bytes());
    info[56..64].copy_from_slice(&ptr_or_null(&images).to_le_bytes());
    Ok((info, buffers, images))
}

fn foreign_barrier_on(p: &Process, id: u32, a: &[u64], h: u64, t: &Table) -> R<u64> {
    match id {
        g::ID_VK_CMD_PIPELINE_BARRIER => {
            // (cb, srcStage, dstStage, flags, memCount, pMem, bufCount, pBuf, imgCount, pImg)
            let buffers = unforeign(p, a[7], a[6] as u32, 56, (24, 28))?;
            let images = unforeign(p, a[9], a[8] as u32, 72, (32, 36))?;
            const NAMES: &[&CStr] = &[c"vkCmdPipelineBarrier"];
            // SAFETY: the Vulkan signature; the barrier arrays are host copies alive for the call.
            let e: unsafe extern "system" fn(u64, u32, u32, u32, u32, u64, u32, u64, u32, u64) = unsafe { f(t, id, NAMES)? };
            unsafe { e(h, a[1] as u32, a[2] as u32, a[3] as u32, a[4] as u32, a[5], a[6] as u32, ptr_or_null(&buffers), a[8] as u32, ptr_or_null(&images)) };
        }
        g::ID_VK_CMD_WAIT_EVENTS => {
            // (cb, eventCount, pEvents, srcStage, dstStage, memCount, pMem, bufCount, pBuf, imgCount, pImg)
            let buffers = unforeign(p, a[8], a[7] as u32, 56, (24, 28))?;
            let images = unforeign(p, a[10], a[9] as u32, 72, (32, 36))?;
            const NAMES: &[&CStr] = &[c"vkCmdWaitEvents"];
            // SAFETY: as above.
            let e: unsafe extern "system" fn(u64, u32, u64, u32, u32, u32, u64, u32, u64, u32, u64) = unsafe { f(t, id, NAMES)? };
            unsafe { e(h, a[1] as u32, a[2], a[3] as u32, a[4] as u32, a[5] as u32, a[6], a[7] as u32, ptr_or_null(&buffers), a[9] as u32, ptr_or_null(&images)) };
        }
        g::ID_VK_CMD_PIPELINE_BARRIER2 => {
            let (info, _buffers, _images) = unforeign_dependency(p, a[1])?;
            const NAMES: &[&CStr] = &[c"vkCmdPipelineBarrier2", c"vkCmdPipelineBarrier2KHR"];
            // SAFETY: as above; `info` points into the two copies, alive for the call.
            let e: unsafe extern "system" fn(u64, u64) = unsafe { f(t, id, NAMES)? };
            unsafe { e(h, info.as_ptr() as u64) };
        }
        g::ID_VK_CMD_WAIT_EVENTS2 => {
            // (cb, eventCount, pEvents, pDependencyInfos): one VkDependencyInfo per event.
            let n = a[1] as u32;
            if n > 1 << 12 {
                return Err(CallError::Args);
            }
            let copies = (0..u64::from(n)).map(|i| unforeign_dependency(p, a[3] + i * 64)).collect::<R<Vec<_>>>()?;
            let infos: Vec<u8> = copies.iter().flat_map(|(info, _, _)| info.iter().copied()).collect();
            const NAMES: &[&CStr] = &[c"vkCmdWaitEvents2", c"vkCmdWaitEvents2KHR"];
            // SAFETY: as above.
            let e: unsafe extern "system" fn(u64, u32, u64, u64) = unsafe { f(t, id, NAMES)? };
            unsafe { e(h, n, a[2], ptr_or_null(&infos)) };
        }
        _ => unreachable!("foreign_barrier filters the ids"),
    }
    Ok(0)
}

/// `vkBindImageMemory2`: a swapchain image with a `VkNativeBufferANDROID` (what the loader turns a
/// `VkBindImageMemorySwapchainInfoKHR` into) is bound to its gralloc buffer here; the rest go to the
/// host driver, copied (`VkBindImageMemoryInfo`, 40 bytes), minus the swapchain structure.
fn bind_image_memory2(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (h, t) = gpu.dispatchable(p, a[0])?;
    let n = a[1] as u32;
    if n > 1 << 12 {
        return Err(CallError::Args);
    }
    let mut host: Vec<u8> = Vec::new();
    let mut spliced = Vec::new();
    for i in 0..u64::from(n) {
        let at = a[2] + i * 40;
        if let Some((_, native)) = chain_find(p, at, STYPE_NATIVE_BUFFER_ANDROID)? {
            let image = rd_u64(p, at + 16)?;
            match super::native::bind_swapchain_image(gpu, p, h, &t, image, native) {
                Ok(()) => continue,
                Err(CallError::Host(r)) => return Ok(result(r)),
                Err(e) => return Err(e),
            }
        }
        if let Some(s) = splice(p, at, STYPE_BIND_IMAGE_MEMORY_SWAPCHAIN_INFO)? {
            spliced.push(s);
        }
        host.extend_from_slice(&p.mem.read(at, 40).map_err(|_| CallError::Args)?);
    }
    if host.is_empty() {
        return Ok(result(VK_SUCCESS));
    }
    const NAMES: &[&CStr] = &[c"vkBindImageMemory2", c"vkBindImageMemory2KHR"];
    // SAFETY: the Vulkan signature; `host` holds whole VkBindImageMemoryInfos.
    let e: unsafe extern "system" fn(u64, u32, *const u8) -> i32 = unsafe { f(&t, g::ID_VK_BIND_IMAGE_MEMORY2, NAMES)? };
    let r = unsafe { e(h, (host.len() / 40) as u32, host.as_ptr()) };
    drop(spliced);
    Ok(result(r))
}

/// The device a queue belongs to.
fn device_of_queue(gpu: &Gpu, queue: u64) -> R<u64> {
    gpu.objects.lock().get(&queue).map(|o| o.parent).ok_or(CallError::Handle(queue))
}

/// Every queue of `device` idle.
fn device_wait_idle(gpu: &Gpu, device: u64, t: &Table) -> R<()> {
    const NAMES: &[&CStr] = &[c"vkDeviceWaitIdle"];
    // SAFETY: the Vulkan signature; `device` is a live host device.
    let e: unsafe extern "system" fn(u64) -> i32 = unsafe { f(t, g::ID_VK_DEVICE_WAIT_IDLE, NAMES)? };
    let _ = gpu;
    unsafe { e(device) };
    Ok(())
}

/// Submit nothing on some queue of `device`: waiting for `wait`, signalling `signal` and `fence`.
fn submit_nothing(gpu: &Gpu, device: u64, t: &Table, wait: u64, signal: u64, fence: u64) -> R<()> {
    let queue = gpu.devices.lock().get(&device).and_then(|i| i.queues.keys().next().copied()).ok_or(CallError::Missing("a queue"))?;
    let stage = ash::vk::PipelineStageFlags::ALL_COMMANDS.as_raw();
    let mut si = [0u8; 72];
    si[0..4].copy_from_slice(&4u32.to_le_bytes()); // VK_STRUCTURE_TYPE_SUBMIT_INFO
    if wait != 0 {
        si[16..20].copy_from_slice(&1u32.to_le_bytes());
        si[24..32].copy_from_slice(&(std::ptr::from_ref(&wait) as u64).to_le_bytes());
        si[32..40].copy_from_slice(&(std::ptr::from_ref(&stage) as u64).to_le_bytes());
    }
    if signal != 0 {
        si[56..60].copy_from_slice(&1u32.to_le_bytes());
        si[64..72].copy_from_slice(&(std::ptr::from_ref(&signal) as u64).to_le_bytes());
    }
    const NAMES: &[&CStr] = &[c"vkQueueSubmit"];
    // SAFETY: the Vulkan signature; `si` is a VkSubmitInfo whose pointers outlive the call.
    let e: unsafe extern "system" fn(u64, u32, *const u8, u64) -> i32 = unsafe { f(t, g::ID_VK_QUEUE_SUBMIT, NAMES)? };
    let r = unsafe { e(queue, 1, si.as_ptr(), fence) };
    if r == VK_SUCCESS { Ok(()) } else { Err(CallError::Host(r)) }
}

/// `vkGetSemaphoreFdKHR`/`vkGetFenceFdKHR` of a `SYNC_FD`: the work is waited for, and the fd is
/// a sync file, signalled (`crate::sync_file`) -- not -1, which the specification allows but
/// which ANGLE then polls forever. As a sync-file export does, the payload is taken: the semaphore
/// is waited on (unsignalled), the fence reset.
fn export_sync_fd(gpu: &Gpu, p: &Process, id: u32, a: &[u64]) -> R<u64> {
    let (device, t) = gpu.dispatchable(p, a[0])?;
    let object = rd_u64(p, a[1] + 16)?;
    if id == g::ID_VK_GET_SEMAPHORE_FD_KHR {
        device_wait_idle(gpu, device, &t)?;
        submit_nothing(gpu, device, &t, object, 0, 0)?;
        device_wait_idle(gpu, device, &t)?;
    } else {
        const WAIT: &[&CStr] = &[c"vkWaitForFences"];
        const RESET: &[&CStr] = &[c"vkResetFences"];
        // SAFETY: the Vulkan signatures.
        let w: unsafe extern "system" fn(u64, u32, *const u64, u32, u64) -> i32 = unsafe { f(&t, g::ID_VK_WAIT_FOR_FENCES, WAIT)? };
        let r: unsafe extern "system" fn(u64, u32, *const u64) -> i32 = unsafe { f(&t, g::ID_VK_RESET_FENCES, RESET)? };
        unsafe {
            w(device, 1, &object, 1, u64::MAX);
            r(device, 1, &object);
        }
    }
    let fd = crate::sync_file::signalled(p).map_err(|_| CallError::Args)?;
    wr(p, a[2], &fd.to_le_bytes())?;
    Ok(result(VK_SUCCESS))
}

/// `vkImportSemaphoreFdKHR`/`vkImportFenceFdKHR` of a `SYNC_FD`: every sync file here is already
/// signalled (an export is -1), so the object is signalled; a real descriptor is the caller's to
/// give up, and is closed.
fn import_sync_fd(gpu: &Gpu, p: &Process, id: u32, a: &[u64]) -> R<u64> {
    let (device, t) = gpu.dispatchable(p, a[0])?;
    let object = rd_u64(p, a[1] + 16)?;
    let fd = rd_u32(p, a[1] + 32)? as i32;
    if fd >= 0 {
        let _ = p.fds.remove(fd);
    }
    if id == g::ID_VK_IMPORT_SEMAPHORE_FD_KHR {
        submit_nothing(gpu, device, &t, 0, object, 0)?;
    } else {
        submit_nothing(gpu, device, &t, 0, 0, object)?;
    }
    Ok(result(VK_SUCCESS))
}

/// `vkQueueSubmit`: each `VkSubmitInfo` (72 bytes) copied with its command buffers unwrapped.
fn queue_submit(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (q, t) = gpu.dispatchable(p, a[0])?;
    let n = a[1] as u32;
    if n > 1 << 12 {
        return Err(CallError::Args);
    }
    let mut infos = if n == 0 { Vec::new() } else { p.mem.read(a[2], n as usize * 72).map_err(|_| CallError::Args)? };
    let mut arrays: Vec<Vec<u64>> = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let s = &mut infos[i * 72..i * 72 + 72];
        let count = u32::from_le_bytes(s[40..44].try_into().expect("4"));
        let ptr = u64::from_le_bytes(s[48..56].try_into().expect("8"));
        arrays.push(unwrap_array(gpu, p, ptr, count)?);
        s[48..56].copy_from_slice(&(arrays[i].as_ptr() as u64).to_le_bytes());
    }
    const NAMES: &[&CStr] = &[c"vkQueueSubmit"];
    // SAFETY: the Vulkan signature; `infos` are VkSubmitInfos whose command buffers are host ones.
    let e: unsafe extern "system" fn(u64, u32, *const u8, u64) -> i32 = unsafe { f(&t, g::ID_VK_QUEUE_SUBMIT, NAMES)? };
    let device = device_of_queue(gpu, q)?;
    super::ahb::before_submit(gpu, device);
    let r = unsafe { e(q, n, if n == 0 { std::ptr::null() } else { infos.as_ptr() }, a[3]) };
    if r == VK_SUCCESS {
        super::ahb::after_submit(gpu, &t, device, q)?;
    }
    Ok(result(r))
}

/// `vkQueueSubmit2`: each `VkSubmitInfo2` (64 bytes) copied, its `VkCommandBufferSubmitInfo`s
/// (32 bytes) copied with the command buffer unwrapped.
fn queue_submit2(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (q, t) = gpu.dispatchable(p, a[0])?;
    let n = a[1] as u32;
    if n > 1 << 12 {
        return Err(CallError::Args);
    }
    let mut infos = if n == 0 { Vec::new() } else { p.mem.read(a[2], n as usize * 64).map_err(|_| CallError::Args)? };
    let mut cbs: Vec<Vec<u8>> = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        let s = &mut infos[i * 64..i * 64 + 64];
        let count = u32::from_le_bytes(s[32..36].try_into().expect("4"));
        let ptr = u64::from_le_bytes(s[40..48].try_into().expect("8"));
        if count > 1 << 16 {
            return Err(CallError::Args);
        }
        let mut infos2 = if count == 0 { Vec::new() } else { p.mem.read(ptr, count as usize * 32).map_err(|_| CallError::Args)? };
        for j in 0..count as usize {
            let w = u64::from_le_bytes(infos2[j * 32 + 16..j * 32 + 24].try_into().expect("8"));
            let h = gpu.dispatchable_of(p, w, Kind::CommandBuffer)?;
            infos2[j * 32 + 16..j * 32 + 24].copy_from_slice(&h.to_le_bytes());
        }
        cbs.push(infos2);
        s[40..48].copy_from_slice(&(cbs[i].as_ptr() as u64).to_le_bytes());
    }
    const NAMES: &[&CStr] = &[c"vkQueueSubmit2", c"vkQueueSubmit2KHR"];
    // SAFETY: the Vulkan signature; the infos are VkSubmitInfo2s whose command buffers are host ones.
    let e: unsafe extern "system" fn(u64, u32, *const u8, u64) -> i32 = unsafe { f(&t, g::ID_VK_QUEUE_SUBMIT2, NAMES)? };
    let device = device_of_queue(gpu, q)?;
    super::ahb::before_submit(gpu, device);
    let r = unsafe { e(q, n, if n == 0 { std::ptr::null() } else { infos.as_ptr() }, a[3]) };
    if r == VK_SUCCESS {
        super::ahb::after_submit(gpu, &t, device, q)?;
    }
    Ok(result(r))
}
