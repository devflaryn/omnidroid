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
pub(crate) const ID_QUEUE_SIGNAL_RELEASE_IMAGE: u32 = 0x1005;

const VK_SUCCESS: i32 = 0;
const VK_INCOMPLETE: i32 = 5;
const VK_ERROR_LAYER_NOT_PRESENT: i32 = -6;
const VK_ERROR_FEATURE_NOT_PRESENT: i32 = -8;

const STYPE_NATIVE_BUFFER_ANDROID: u32 = 1_000_010_000;
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

/// A host entry point of `table`, as function type `F`.
///
/// # Safety
/// `F` must be the entry point's exact Vulkan signature.
unsafe fn f<F: Copy>(table: &Table, id: u32, names: &'static [&'static CStr]) -> R<F> {
    let p = table.get(id, names)?;
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
    // SAFETY: `p` is a non-null function address; the caller names its type.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

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
    EXTENSIONS.iter().any(|(n, d)| *n == name && *d == device) && !EMULATED.contains(&name)
}

/// Extensions implemented here rather than by the host driver: offered whatever the host has,
/// and never enabled on it.
const EMULATED: &[&str] = &[];

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
            gpu.objects.lock().retain(|k, o| *k != h && o.parent != h);
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
            let mut offered: Vec<_> = enumerate_host_device_extensions(&t, pd)?.into_iter().filter(|(n, _)| forwarded(n, true)).collect();
            offered.extend(EMULATED.iter().map(|n| ((*n).to_string(), 1)));
            write_extensions(p, a[2], a[3], &offered)
        }
        g::ID_VK_CREATE_DEVICE => create_device(gpu, p, a),
        g::ID_VK_DESTROY_DEVICE => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkDestroyDevice"];
            // SAFETY: the Vulkan signature; `h` is a live host device.
            let d: unsafe extern "system" fn(u64, *const c_void) = unsafe { f(&t, id, NAMES)? };
            unsafe { d(h, std::ptr::null()) };
            gpu.objects.lock().retain(|k, o| *k != h && o.parent != h);
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
            const NAMES: &[&CStr] = &[c"vkGetPhysicalDeviceImageFormatProperties2", c"vkGetPhysicalDeviceImageFormatProperties2KHR"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            let r = unsafe { e(h, a[1], a[2]) };
            if let Some(s) = usage {
                wr(p, s.node + 16, &AHB_USAGE_GPU.to_le_bytes())?;
            }
            Ok(result(r))
        }
        g::ID_VK_CREATE_IMAGE => {
            if chain_find(p, a[1], STYPE_NATIVE_BUFFER_ANDROID)?.is_some() || chain_find(p, a[1], STYPE_EXTERNAL_FORMAT_ANDROID)?.is_some() {
                return Ok(result(VK_ERROR_FEATURE_NOT_PRESENT));
            }
            passthrough4(gpu, p, id, a, &[c"vkCreateImage"])
        }
        g::ID_VK_ALLOCATE_MEMORY => {
            if chain_find(p, a[1], STYPE_IMPORT_AHB_INFO_ANDROID)?.is_some() {
                return Ok(result(VK_ERROR_FEATURE_NOT_PRESENT));
            }
            passthrough4(gpu, p, id, a, &[c"vkAllocateMemory"])
        }
        g::ID_VK_BIND_IMAGE_MEMORY => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkBindImageMemory"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u64, u64, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            Ok(result(unsafe { e(h, a[1], a[2], a[3]) }))
        }
        g::ID_VK_BIND_IMAGE_MEMORY2 => {
            let (h, t) = gpu.dispatchable(p, a[0])?;
            const NAMES: &[&CStr] = &[c"vkBindImageMemory2", c"vkBindImageMemory2KHR"];
            // SAFETY: the Vulkan signature.
            let e: unsafe extern "system" fn(u64, u32, u64) -> i32 = unsafe { f(&t, id, NAMES)? };
            Ok(result(unsafe { e(h, a[1] as u32, a[2]) }))
        }
        _ => {
            let name = g::COMMANDS.get(id as usize).map_or("?", |c| c.0);
            Err(CallError::Missing(name))
        }
    }
}

/// VK_ANDROID_native_buffer's commands (ids from [`ID_GRALLOC_USAGE`]).
pub(crate) fn extra(_gpu: &Gpu, _p: &Process, id: u32, _a: &[u64]) -> R<u64> {
    if (ID_GRALLOC_USAGE..=ID_QUEUE_SIGNAL_RELEASE_IMAGE).contains(&id) {
        return Err(CallError::Missing("VK_ANDROID_native_buffer"));
    }
    Err(CallError::Args)
}

/// A command `(dispatchable, create info, allocator, out handle)` passed through, with no allocator.
fn passthrough4(gpu: &Gpu, p: &Process, id: u32, a: &[u64], names: &'static [&'static CStr]) -> R<u64> {
    let (h, t) = gpu.dispatchable(p, a[0])?;
    // SAFETY: `(handle, const T*, const VkAllocationCallbacks*, U*) -> VkResult`, as every such
    // create command is; the out-pointer is the guest's.
    let e: unsafe extern "system" fn(u64, u64, *const c_void, u64) -> i32 = unsafe { f(&t, id, names)? };
    Ok(result(unsafe { e(h, a[1], std::ptr::null(), a[3]) }))
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
    let names: Vec<CString> = strings(p, at(56), ext_count)?.into_iter().filter(|n| !EMULATED.iter().any(|e| e.as_bytes() == n.as_bytes())).collect();
    let ptrs: Vec<*const c_char> = names.iter().map(|n| n.as_ptr()).collect();
    let mut host_ci = ci.clone();
    host_ci[32..36].copy_from_slice(&0u32.to_le_bytes()); // no layers
    host_ci[40..48].copy_from_slice(&0u64.to_le_bytes());
    host_ci[48..52].copy_from_slice(&(ptrs.len() as u32).to_le_bytes());
    host_ci[56..64].copy_from_slice(&(ptrs.as_ptr() as u64).to_le_bytes());
    const CREATE: &[&CStr] = &[c"vkCreateDevice"];
    // SAFETY: the Vulkan signature; `host_ci` is a VkDeviceCreateInfo with host extension names.
    let e: unsafe extern "system" fn(u64, *const u8, *const c_void, *mut u64) -> i32 = unsafe { f(&t, g::ID_VK_CREATE_DEVICE, CREATE)? };
    let mut device = 0u64;
    let r = unsafe { e(pd, host_ci.as_ptr(), std::ptr::null(), &mut device) };
    if r == VK_SUCCESS {
        const GDPA: &[&CStr] = &[c"vkGetDeviceProcAddr"];
        // SAFETY: `vkGetDeviceProcAddr`'s signature is `GetProcAddr`'s.
        let gdpa = unsafe { f(&t, g::ID_VK_GET_DEVICE_PROC_ADDR, GDPA)? };
        gpu.add(device, Kind::Device, device, Table::new(gdpa, device));
        wr(p, a[3], &device.to_le_bytes())?;
    }
    Ok(result(r))
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
    Ok(result(unsafe { e(q, n, if n == 0 { std::ptr::null() } else { infos.as_ptr() }, a[3]) }))
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
    Ok(result(unsafe { e(q, n, if n == 0 { std::ptr::null() } else { infos.as_ptr() }, a[3]) }))
}
