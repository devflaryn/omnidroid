//! **Stage 3: a surface over a real window, a physical device chosen, and a real `VkDevice` —
//! all driven from translated ARM64 through guest thunks.**
//!
//! ```text
//! cargo test -p omni-android --release --test vulkan_device
//! OMNI_GFX_WINDOW_TESTS=1 cargo test -p omni-android --release --test vulkan_device -- --ignored --test-threads=1
//! ```
//!
//! # The two halves, and why neither is enough
//!
//! `tests/vulkan_instance.rs` makes the argument in full and this file inherits it. The **ordinary**
//! tests run on every machine, with no GPU and no display, against [`StageThreeHost`] — a double
//! for the *host* side of [`VulkanHost`], never for anything the guest can see. Its job is to be a
//! driver whose answers the test chose, because the properties worth asserting here cannot be
//! asserted against a real one:
//!
//! * that a `pPropertyCount` smaller than the driver's count produces `VK_INCOMPLETE` and **does
//!   not write past the end of the guest's array** — no real driver can be made to report a
//!   different number of devices between two calls;
//! * that the surface substitution is recorded with both call names and the window system;
//! * that a `VkPhysicalDevice` passed where a `VkSurfaceKHR` belongs is a typed refusal;
//! * that the bytes the host answers with arrive in the guest's buffer **unchanged**, which
//!   needs a blob whose contents the test picked.
//!
//! The **live** tests are the other half and they are the ones that answer "does it work": a real
//! window on the screen, a real NVIDIA driver, a real `VkSurfaceKHR` over that window's `HWND`, a
//! real device, and the driver's own strings read back out of the **guest's** memory. They are
//! `#[ignore]`d and gated on `OMNI_GFX_WINDOW_TESTS`, and under `--ignored` without the gate they
//! **panic** naming the variable — nothing here is allowed to quietly pass on a machine that
//! cannot run it (`VERIFICATION.md` entry 4).
//!
//! # What these cannot see
//!
//! Nothing is drawn. Stage 3 stops at a device and a queue by design, so there is no swapchain and
//! no frame, and this file makes no claim about a pixel. `tests/ndk_host_window.rs` is where real
//! frames reach a real window, through `omni-gfx`'s own renderer rather than through the guest.

#![cfg(target_arch = "x86_64")]

mod harness;

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use harness::a64::*;
use harness::{serialized, Asm, Guest, BUDGET};
use omni_android::bionic::Bionic;
use omni_android::jni::Jni;
use omni_android::ndk::{HostWindowSource, Ndk, WindowSource, SURFACE_CLASS};
use omni_android::vulkan::{
    ChainLink, DeviceRequest, DriverAnswer, ImageFormatQuery, IMAGE_FORMAT_PROPERTIES_BYTES, HostDevice, HostExtension, HostInstance, HostPhysicalDevice,
    HostQueue, HostSurface, InstanceRequest, RewriteSite, SurfaceCreated, Vulkan, VulkanHost,
    ANDROID_SURFACE_CREATE_INFO_BYTES, DEVICE_CREATE_INFO_BYTES, FORMAT_PROPERTIES_BYTES, DEVICE_QUEUE_CREATE_INFO_BYTES,
    GUEST_SURFACE_EXTENSION, LOADER_ENTRY_POINT, LOADER_SONAMES,
    PHYSICAL_DEVICE_FEATURES_BYTES, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES,
    PHYSICAL_DEVICE_PROPERTIES_BYTES, QUEUE_FAMILY_PROPERTIES_BYTES, SURFACE_CAPABILITIES_BYTES,
    SURFACE_FORMAT_BYTES, STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, VK_INCOMPLETE, VK_SUCCESS,
    MAX_CHAIN_LINKS, PHYSICAL_DEVICE_FEATURES_2_BYTES, STYPE_PHYSICAL_DEVICE_FEATURES_2,
    IMAGE_FORMAT_PROPERTIES_2_BYTES, PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2_BYTES,
    STYPE_IMAGE_FORMAT_PROPERTIES_2, STYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
};
use omni_android::{AbiError, AbiResult, Boundary};
use omni_cpu::{ExitReason, GuestAddr};
use omni_platform::window::RawWindow;

/// `RTLD_NOW`, the `mov w1, #2` at `0x2595178`.
const RTLD_NOW: u64 = 2;

/// The gate, shared with `omni-gfx`'s `renderer_live.rs`, `tests/ndk_host_window.rs` and
/// `tests/vulkan_instance.rs`: one decision by one person covers the whole graphics bring-up.
const GATE: &str = "OMNI_GFX_WINDOW_TESTS";

/// A window handle the double is given. **Not a real `HWND`** — the double never calls an OS
/// function with it — and deliberately not zero, so that "the handle travelled" is a thing this
/// file can assert.
const FAKE_HWND: isize = 0x1234_5678;
/// Its `HINSTANCE`. A different number from [`FAKE_HWND`], because a shim that passed the same
/// field twice would otherwise pass every assertion.
const FAKE_HINSTANCE: isize = 0x0BAD_F00D;

/// Fail, naming the variable, if a live test was run without the opt-in.
fn require_gate() {
    let set = std::env::var(GATE).is_ok_and(|v| v == "1");
    assert!(
        set,
        "this test was run with --ignored but {GATE} is not set to 1. It opens a window, loads \
         the host's Vulkan driver, creates a real VkSurfaceKHR over that window and a real \
         VkDevice on this machine's GPU; it will not pretend to have passed on a machine that \
         cannot do that. Set {GATE}=1 to run it, or drop --ignored to skip it visibly."
    );
}

// =================================================================== the host test double

/// What a [`StageThreeHost`] was asked.
#[derive(Debug, Default)]
struct HostLog {
    /// Every `create_platform_surface`, with the window handle it was given.
    ///
    /// **The only way to see that the `ANativeWindow *` was resolved to the right window**, since
    /// the guest never learns the `HWND` and the rewrite log records the *call* rather than the
    /// handle.
    surfaces: Vec<(HostInstance, RawWindow)>,
    /// Every `vkCreateDevice` request, exactly as the shim decoded it.
    devices: Vec<DeviceRequest>,
    /// Every `(device, family, index)` `vkGetDeviceQueue` asked for, **including the repeats** —
    /// the registry deduplicates the handle and the host is still asked, which is what lets a
    /// test tell "the same handle came back" from "the call was skipped".
    queues: Vec<(HostDevice, u32, u32)>,
    /// Every `(device, name)` `vkGetDeviceProcAddr` asked about.
    device_procs: Vec<(HostDevice, String)>,
    /// Every `vkGetPhysicalDeviceFeatures2`: the entry point it was asked through, and the chain
    /// as it arrived.
    features2: Vec<(String, Vec<ChainLink>)>,
    /// Every `vkGetPhysicalDeviceImageFormatProperties2`: its entry point, its question, and the
    /// question's chain.
    image_format2: Vec<(String, ImageFormatQuery, Vec<ChainLink>)>,
}

/// A [`VulkanHost`] whose answers the test chooses. **Not a driver, and not guest-facing.**
#[derive(Debug)]
struct StageThreeHost {
    /// How many physical devices this "driver" has.
    device_count: usize,
    /// `VkPhysicalDeviceProperties`, one blob per device, filled by [`properties_blob`].
    properties: Vec<Vec<u8>>,
    /// One `VkQueueFamilyProperties` blob per family.
    families: Vec<Vec<u8>>,
    /// One `VkSurfaceFormatKHR` blob per format.
    formats: Vec<Vec<u8>>,
    present_modes: Vec<u32>,
    device_extensions: Vec<HostExtension>,
    /// Which queue families this "driver" says can present.
    presentable: Vec<u32>,
    /// Device-level entry points this "driver" has. Everything else is `false`.
    device_has: Vec<String>,
    /// Whether this "driver" has the platform surface call at all. `false` is the misconfigured
    /// host `vkGetInstanceProcAddr` must answer NULL for rather than hand out a thunk it cannot
    /// honour.
    has_platform_surface_call: bool,
    log: Mutex<HostLog>,
}

impl StageThreeHost {
    fn new() -> Arc<StageThreeHost> {
        Arc::new(StageThreeHost {
            device_count: 2,
            properties: vec![
                properties_blob("OMNI Reference GPU", 0x0040_1000, 0x10DE),
                properties_blob("OMNI Software Rasteriser", 0x0040_0000, 0x1AE0),
            ],
            // Two families, with **different** `queueFlags`, so a handler that returned the first
            // for both would be caught: family 0 is GRAPHICS|COMPUTE|TRANSFER, family 1 is
            // TRANSFER alone.
            families: vec![family_blob(0x0000_0007, 1), family_blob(0x0000_0004, 8)],
            formats: vec![format_blob(44, 0), format_blob(50, 0)],
            present_modes: vec![2, 0, 3],
            device_extensions: vec![
                HostExtension { name: "VK_KHR_swapchain".to_string(), spec_version: 70 },
                HostExtension { name: "VK_EXT_memory_budget".to_string(), spec_version: 1 },
            ],
            presentable: vec![0],
            device_has: vec![
                "vkCreateSwapchainKHR".to_string(),
                "vkQueueSubmit".to_string(),
                "vkAllocateMemory".to_string(),
                // On the far side of stage 5's frontier, and here so that a test can show the
                // frontier is named rather than assumed.
                "vkCreateComputePipelines".to_string(),
            ],
            has_platform_surface_call: true,
            log: Mutex::new(HostLog::default()),
        })
    }

    /// The same host, with no window-system integration behind it.
    fn without_a_platform_surface_call() -> Arc<StageThreeHost> {
        let mut host = Arc::into_inner(StageThreeHost::new()).expect("sole owner");
        host.has_platform_surface_call = false;
        Arc::new(host)
    }

    fn log(&self) -> std::sync::MutexGuard<'_, HostLog> {
        self.log.lock().expect("the log is never held across a panic")
    }
}

/// A `VkPhysicalDeviceProperties` blob with a recognisable name, version and vendor.
///
/// Only the fields a test reads are set; the rest are zero, which is a **valid** structure and is
/// how a driver that supports nothing optional would answer. The name is written at offset 20,
/// where `deviceName` is.
fn properties_blob(name: &str, api_version: u32, vendor: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; PHYSICAL_DEVICE_PROPERTIES_BYTES];
    bytes[0..4].copy_from_slice(&api_version.to_le_bytes());
    bytes[8..12].copy_from_slice(&vendor.to_le_bytes());
    bytes[16..20].copy_from_slice(&2u32.to_le_bytes()); // VK_PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
    bytes[20..20 + name.len()].copy_from_slice(name.as_bytes());
    // A byte in the *last* member, so a blob truncated anywhere is detectable.
    bytes[PHYSICAL_DEVICE_PROPERTIES_BYTES - 4..]
        .copy_from_slice(&0xABCD_1234u32.to_le_bytes());
    bytes
}

/// A `VkQueueFamilyProperties` blob: flags, count, timestamp bits, transfer granularity.
fn family_blob(flags: u32, count: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; QUEUE_FAMILY_PROPERTIES_BYTES];
    bytes[0..4].copy_from_slice(&flags.to_le_bytes());
    bytes[4..8].copy_from_slice(&count.to_le_bytes());
    bytes[8..12].copy_from_slice(&64u32.to_le_bytes());
    bytes[12..16].copy_from_slice(&1u32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes
}

/// A `VkSurfaceFormatKHR` blob.
fn format_blob(format: u32, colour_space: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; SURFACE_FORMAT_BYTES];
    bytes[0..4].copy_from_slice(&format.to_le_bytes());
    bytes[4..8].copy_from_slice(&colour_space.to_le_bytes());
    bytes
}

impl VulkanHost for StageThreeHost {
    fn platform_surface_extension(&self) -> AbiResult<String> {
        Ok("VK_KHR_win32_surface".to_string())
    }

    fn platform_surface_entry_point(&self) -> AbiResult<String> {
        if self.has_platform_surface_call {
            return Ok("vkCreateWin32SurfaceKHR".to_string());
        }
        // A host that reports the extension and has no call for it is not a shape any real
        // loader produces; it is here because it is the only way to exercise the branch where
        // `vkGetInstanceProcAddr` answers NULL for `vkCreateAndroidSurfaceKHR` on the driver's
        // authority rather than on this layer's.
        Ok("vkCreateNothingAnyHostHasKHR".to_string())
    }

    fn instance_extensions(
        &self,
        _layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        Ok(DriverAnswer::Ok(vec![
            HostExtension { name: "VK_KHR_surface".to_string(), spec_version: 25 },
            HostExtension { name: "VK_KHR_win32_surface".to_string(), spec_version: 6 },
        ]))
    }

    fn create_instance(&self, _request: &InstanceRequest) -> AbiResult<DriverAnswer<HostInstance>> {
        Ok(DriverAnswer::Ok(HostInstance::from_token(0)))
    }

    fn has_instance_proc(&self, _instance: HostInstance, name: &str) -> AbiResult<bool> {
        // **Deliberately does not have `vkCreateAndroidSurfaceKHR`**, because no host driver
        // does -- that is what the first live run found, and a double that had it would let this
        // whole file pass while the live path stranded the engine. What it does have is the
        // Win32 call, which is what this layer resolves the Android name on the strength of.
        Ok(name.starts_with("vkGet")
            || name.starts_with("vkEnumerate")
            || (self.has_platform_surface_call && name == "vkCreateWin32SurfaceKHR")
            || name == "vkCreateDevice"
            || name == "vkDestroyInstance")
    }

    fn create_platform_surface(
        &self,
        instance: HostInstance,
        window: RawWindow,
    ) -> AbiResult<DriverAnswer<SurfaceCreated>> {
        let mut log = self.log();
        log.surfaces.push((instance, window));
        Ok(DriverAnswer::Ok(SurfaceCreated {
            surface: HostSurface::from_token(log.surfaces.len() as u64 - 1),
            host_call: "vkCreateWin32SurfaceKHR".to_string(),
        }))
    }

    fn physical_devices(
        &self,
        _instance: HostInstance,
    ) -> AbiResult<DriverAnswer<Vec<HostPhysicalDevice>>> {
        Ok(DriverAnswer::Ok(
            (0..self.device_count as u64).map(HostPhysicalDevice::from_token).collect(),
        ))
    }

    fn physical_device_properties(&self, device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        Ok(self.properties[device.token() as usize].clone())
    }

    fn physical_device_features(&self, _device: HostPhysicalDevice) -> AbiResult<Vec<u8>> {
        let mut bytes = vec![0u8; PHYSICAL_DEVICE_FEATURES_BYTES];
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes()); // robustBufferAccess
        bytes[PHYSICAL_DEVICE_FEATURES_BYTES - 4..].copy_from_slice(&1u32.to_le_bytes());
        Ok(bytes)
    }

    /// The format's own number in all three members, plus 0, 1 and 2: a blob that says which
    /// format it answers for, and which member is which.
    fn physical_device_format_properties(
        &self,
        _device: HostPhysicalDevice,
        format: i32,
    ) -> AbiResult<Vec<u8>> {
        Ok((0..3u32).flat_map(|k| (format as u32 + k).to_le_bytes()).collect())
    }

    /// Format 75 is supported, and its answer carries the five scalars back in the first five
    /// words (and `maxResourceSize` 1 GiB), so a transposed pair is visible; every other format is
    /// the driver's `VK_ERROR_FORMAT_NOT_SUPPORTED` (-11).
    fn physical_device_image_format_properties(
        &self,
        _device: HostPhysicalDevice,
        query: ImageFormatQuery,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        if query.format != 75 {
            return Ok(DriverAnswer::Failed(-11));
        }
        let words = [
            query.format as u32,
            query.image_type as u32,
            query.tiling as u32,
            query.usage,
            query.flags,
            0,
        ];
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        bytes.extend_from_slice(&(1u64 << 30).to_le_bytes());
        Ok(DriverAnswer::Ok(bytes))
    }

    /// Format 1000156002 is supported -- the five scalars come back in the first five words, as
    /// above, and each answer-chain member is set to 1 -- and every other format is the driver's
    /// `VK_ERROR_FORMAT_NOT_SUPPORTED`.
    fn physical_device_image_format_properties2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        query: ImageFormatQuery,
        question: &[ChainLink],
        answers: &mut [ChainLink],
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        self.log().image_format2.push((entry.to_string(), query, question.to_vec()));
        if query.format != 1_000_156_002 {
            return Ok(DriverAnswer::Failed(-11));
        }
        for link in answers.iter_mut() {
            for member in link.body.chunks_mut(4) {
                member.copy_from_slice(&1u32.to_le_bytes());
            }
        }
        let words = [
            query.format as u32,
            query.image_type as u32,
            query.tiling as u32,
            query.usage,
            query.flags,
            0,
        ];
        let _ = device;
        let mut bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        bytes.extend_from_slice(&(1u64 << 30).to_le_bytes());
        Ok(DriverAnswer::Ok(bytes))
    }

    /// Every chained feature answered **supported**: a `VkBool32` 1 in each member, so a member
    /// this layer failed to write back reads as the guest's own 0.
    fn physical_device_features2(
        &self,
        device: HostPhysicalDevice,
        entry: &str,
        chain: &mut [ChainLink],
    ) -> AbiResult<Vec<u8>> {
        self.log().features2.push((entry.to_string(), chain.to_vec()));
        for link in chain.iter_mut() {
            for member in link.body.chunks_mut(4) {
                member.copy_from_slice(&1u32.to_le_bytes());
            }
        }
        self.physical_device_features(device)
    }

    fn queue_family_properties(&self, _device: HostPhysicalDevice) -> AbiResult<Vec<Vec<u8>>> {
        Ok(self.families.clone())
    }

    /// **This double can back its one memory type**, so nothing is masked and the blob the guest
    /// receives is the one below, byte for byte.
    ///
    /// Stage 5 made `vkGetPhysicalDeviceMemoryProperties` edit the property flags of every type
    /// this layer cannot import into (`omni_android::vulkan::physical::mask_memory_types`), and a
    /// double that answered `0` here would have every host-visible bit cleared — which is correct
    /// behaviour and would make the verbatim assertion below about the wrong thing. The masking
    /// itself is tested in `tests/vulkan_memory.rs`, against a double written for it.
    fn importable_memory_types(&self, _device: HostPhysicalDevice) -> AbiResult<u32> {
        Ok(0x1)
    }

    fn physical_device_memory_properties(
        &self,
        _device: HostPhysicalDevice,
    ) -> AbiResult<Vec<u8>> {
        let mut bytes = vec![0u8; PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES];
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes()); // memoryTypeCount
        bytes[4..8].copy_from_slice(&0x0000_0007u32.to_le_bytes()); // DEVICE_LOCAL|VISIBLE|COHERENT
        bytes[260..264].copy_from_slice(&1u32.to_le_bytes()); // memoryHeapCount
        bytes[264..272].copy_from_slice(&(8u64 << 30).to_le_bytes()); // an 8 GiB heap
        Ok(bytes)
    }

    fn surface_support(
        &self,
        _device: HostPhysicalDevice,
        queue_family: u32,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<bool>> {
        Ok(DriverAnswer::Ok(self.presentable.contains(&queue_family)))
    }

    fn surface_capabilities(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u8>>> {
        let mut bytes = vec![0u8; SURFACE_CAPABILITIES_BYTES];
        bytes[0..4].copy_from_slice(&2u32.to_le_bytes()); // minImageCount
        bytes[4..8].copy_from_slice(&8u32.to_le_bytes()); // maxImageCount
        bytes[8..12].copy_from_slice(&1024u32.to_le_bytes()); // currentExtent.width
        bytes[12..16].copy_from_slice(&576u32.to_le_bytes()); // currentExtent.height
        Ok(DriverAnswer::Ok(bytes))
    }

    fn surface_formats(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<Vec<u8>>>> {
        Ok(DriverAnswer::Ok(self.formats.clone()))
    }

    fn surface_present_modes(
        &self,
        _device: HostPhysicalDevice,
        _surface: HostSurface,
    ) -> AbiResult<DriverAnswer<Vec<u32>>> {
        Ok(DriverAnswer::Ok(self.present_modes.clone()))
    }

    fn device_extensions(
        &self,
        _device: HostPhysicalDevice,
        layer: Option<&str>,
    ) -> AbiResult<DriverAnswer<Vec<HostExtension>>> {
        if layer.is_some() {
            return Ok(DriverAnswer::Failed(-6)); // VK_ERROR_LAYER_NOT_PRESENT
        }
        Ok(DriverAnswer::Ok(self.device_extensions.clone()))
    }

    fn create_device(
        &self,
        _device: HostPhysicalDevice,
        request: &DeviceRequest,
    ) -> AbiResult<DriverAnswer<HostDevice>> {
        let mut log = self.log();
        log.devices.push(request.clone());
        Ok(DriverAnswer::Ok(HostDevice::from_token(log.devices.len() as u64 - 1)))
    }

    fn device_queue(
        &self,
        device: HostDevice,
        family: u32,
        index: u32,
    ) -> AbiResult<HostQueue> {
        self.log().queues.push((device, family, index));
        // One token per `(device, family, index)`, which is what a real driver guarantees and what
        // the registry's deduplication rests on.
        Ok(HostQueue::from_token(
            (u64::from(family) << 32) | u64::from(index) | (device.token() << 48),
        ))
    }

    fn has_device_proc(&self, device: HostDevice, name: &str) -> AbiResult<bool> {
        self.log().device_procs.push((device, name.to_string()));
        Ok(self.device_has.iter().any(|have| have == name))
    }
}

// ================================================================================ the fixture

/// A host directory that removes itself, so the bionic instance has a filesystem root.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let mut at = std::env::temp_dir();
        at.push(format!("omni-vkdev-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("a scratch directory");
        Scratch(at)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    guest: Guest,
    bionic: Arc<Bionic>,
    jni: Arc<Jni>,
    ndk: Arc<Ndk>,
    vulkan: Arc<Vulkan>,
    boundary: Arc<Boundary>,
    next: Cell<usize>,
    _root: Scratch,
}

/// Where the bump allocator starts, leaving the low bytes for results.
const ARENA_AT: usize = 0x800;

/// One reusable buffer for the live test's enumerations, sized from what a real driver reports.
///
/// The guest arena is [`harness::DATA_BYTES`] and a real NVIDIA driver's device-extension list is
/// around 40 KB of `VkExtensionProperties` on its own, so the live test allocates **one** of these
/// and reuses it rather than a fresh buffer per query. That is how this test first failed, and the
/// assertion beside each use names this constant so the next driver with a longer list says so.
const SCRATCH_BYTES: usize = 48 * 1024;

/// Enough thunk slots for bionic, the NDK, the JNI tables and the Vulkan pool.
///
/// The pool alone is `MAX_PROC_SLOTS + 1`, which stage 3 raised to 641 from a measurement of
/// `libroblox.so`; the rest is everything else this fixture binds. Sized here rather than computed
/// so that running out is a named failure at `bind_into` and not a mysterious one later.
const SLOTS: usize = 2048;

fn fixture(tag: &str, host: Option<Arc<dyn VulkanHost>>) -> Fixture {
    let guest = Guest::new();
    let bionic = Bionic::new(Arc::clone(&guest.space)).expect("a bionic instance");
    let ndk = Ndk::new(Arc::clone(&guest.space)).expect("an NDK instance");
    let jni = Jni::new(Arc::clone(&guest.space)).expect("a JNI instance");
    let builder = guest.boundary(SLOTS);
    bionic.bind_into(&builder).expect("bind every bionic handler");
    ndk.bind_into(&builder).expect("bind every NDK handler");
    jni.install_into(&builder).expect("install the JNI tables");
    bionic.set_log_to_stderr(false);
    let vulkan = Vulkan::new();
    let bound = vulkan.bind_into(&builder).expect("bind the Vulkan loader");
    assert_eq!(bound, omni_android::vulkan::BOUND_SYMBOLS);
    if let Some(host) = host {
        vulkan.set_host(host);
    }
    let root = Scratch::new(tag);
    bionic.set_filesystem_root(&root.0).expect("a filesystem root");
    let boundary = builder.finish();
    Fixture { guest, bionic, jni, ndk, vulkan, boundary, next: Cell::new(ARENA_AT), _root: root }
}

impl Fixture {
    fn vulkan(&self) -> &Arc<Vulkan> {
        &self.vulkan
    }

    fn thunk(&self, symbol: &str) -> GuestAddr {
        self.boundary.slot_named(symbol).unwrap_or_else(|| panic!("`{symbol}` is not bound")).address
    }

    fn alloc(&self, len: usize) -> u64 {
        let offset = (self.next.get() + 7) & !7;
        assert!(offset + len < harness::DATA_BYTES, "the guest arena is full");
        self.next.set(offset + len);
        (self.guest.data + offset) as u64
    }

    fn cstr(&self, text: &str) -> u64 {
        let bytes: Vec<u8> = text.bytes().chain(std::iter::once(0)).collect();
        let at = self.alloc(bytes.len());
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    fn u64_array(&self, values: &[u64]) -> u64 {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let at = self.alloc(bytes.len().max(8));
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// `len` bytes filled with `fill`, so that "nothing was written here" is checkable.
    fn poisoned(&self, len: usize, fill: u8) -> u64 {
        let at = self.alloc(len);
        self.guest.write_bytes(at as GuestAddr, &vec![fill; len]);
        at
    }

    fn run(&self, entry: GuestAddr) -> Result<ExitReason, AbiError> {
        let _bionic = self.bionic.activate().expect("publish the bionic instance");
        let _jni = self.jni.activate().expect("publish the JNI instance");
        let _ndk = self.ndk.activate();
        let _vulkan = self.vulkan.activate();
        let mut cpu = self.guest.thread(&self.boundary);
        self.boundary.run(&mut cpu, entry, BUDGET)
    }

    fn program_branching(&self, body: impl FnOnce(&mut Asm)) -> GuestAddr {
        let entry = self.guest.next_entry();
        let mut asm = Asm::at(entry);
        asm.push(mov_reg(21, 30));
        body(&mut asm);
        asm.mov(22, self.guest.data as u64);
        asm.push(str_imm(0, 22, 0));
        asm.push(ret(21));
        self.guest.load(asm.words());
        entry
    }

    /// Call `target` indirectly with `x0`-`x3` set, which is how the engine calls every Vulkan
    /// entry point (`blr x8` through the pointer `vkGetInstanceProcAddr` returned).
    fn call(&self, target: u64, args: [u64; 4]) -> Result<u64, AbiError> {
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            asm.mov(0, args[0]);
            asm.mov(1, args[1]);
            asm.mov(2, args[2]);
            asm.mov(3, args[3]);
            asm.push(blr(9));
        });
        let exit = self.run(program)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    /// Call `target` with `x0`-`x6` set: the seven-register calls, such as
    /// `vkGetPhysicalDeviceImageFormatProperties`.
    fn call7(&self, target: u64, args: [u64; 7]) -> Result<u64, AbiError> {
        let program = self.program_branching(|asm| {
            asm.mov(9, target);
            for (register, value) in args.iter().enumerate() {
                asm.mov(register as u32, *value);
            }
            asm.push(blr(9));
        });
        let exit = self.run(program)?;
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        Ok(self.guest.read_u64(self.guest.data))
    }

    /// Call a bound symbol directly, for the `ANativeWindow` calls that are not Vulkan thunks.
    fn call_symbol(&self, symbol: &str, args: [u64; 4]) -> Result<u64, AbiError> {
        self.call(self.thunk(symbol) as u64, args)
    }

    fn refusal(&self, target: u64, args: [u64; 4]) -> AbiError {
        match self.call(target, args) {
            Err(error) => error,
            Ok(value) => {
                panic!("a call to {target:#x} returned {value:#x} where a refusal was required")
            }
        }
    }

    /// `dlopen` + `dlsym` + `vkGetInstanceProcAddr`, as the engine does it.
    fn entry_point(&self) -> u64 {
        let soname = self.cstr(LOADER_SONAMES[0]);
        let symbol = self.cstr(LOADER_ENTRY_POINT);
        let dlopen = self.thunk("dlopen");
        let dlsym = self.thunk("dlsym");
        let program = self.program_branching(|asm| {
            asm.mov(0, soname);
            asm.mov(1, RTLD_NOW);
            asm.bl(dlopen);
            asm.mov(1, symbol);
            asm.bl(dlsym);
        });
        let exit = self.run(program).expect("the bootstrap must complete");
        assert!(matches!(exit, ExitReason::Returned { .. }), "{exit:?}");
        let at = self.guest.read_u64(self.guest.data);
        assert_eq!(at as GuestAddr, self.thunk(LOADER_ENTRY_POINT));
        at
    }

    fn proc_addr(&self, entry_point: u64, instance: u64, name: &str) -> Result<u64, AbiError> {
        let at = self.cstr(name);
        self.call(entry_point, [instance, at, 0, 0])
    }

    /// The thunk for `name`, resolved the way the engine resolves it.
    fn resolve(&self, entry_point: u64, instance: u64, name: &str) -> u64 {
        let at = self.proc_addr(entry_point, instance, name).expect("the lookup must complete");
        assert_ne!(at, 0, "`{name}` must resolve to a thunk on instance {instance:#x}");
        at
    }

    /// A `VkInstanceCreateInfo` enabling the two extensions a renderer needs, and the instance it
    /// produces.
    fn an_instance(&self, entry_point: u64) -> u64 {
        let create = self.resolve(entry_point, 0, "vkCreateInstance");
        let names = ["VK_KHR_surface", GUEST_SURFACE_EXTENSION];
        let pointers: Vec<u64> = names.iter().map(|name| self.cstr(name)).collect();
        let array = self.u64_array(&pointers);
        let mut bytes = vec![0u8; 64];
        bytes[0..4].copy_from_slice(&1u32.to_le_bytes()); // VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO
        bytes[48..52].copy_from_slice(&(names.len() as u32).to_le_bytes());
        bytes[56..64].copy_from_slice(&array.to_le_bytes());
        let info = self.alloc(64);
        self.guest.write_bytes(info as GuestAddr, &bytes);
        let out = self.alloc(8);
        assert_eq!(self.call(create, [info, 0, out, 0]).expect("create") as i32, VK_SUCCESS);
        let handle = self.guest.read_u64(out as GuestAddr);
        assert_ne!(handle, 0);
        handle
    }

    /// An `ANativeWindow *` the way §8 row 17 gets one: a Java `Surface`, then `fromSurface`.
    fn a_native_window(&self) -> u64 {
        let surface = self.jni.new_object(SURFACE_CLASS).expect("a Java Surface");
        let window = self
            .call_symbol("ANativeWindow_fromSurface", [0, surface, 0, 0])
            .expect("fromSurface must complete");
        assert_ne!(window, 0, "fromSurface needs no geometry and must produce a window");
        window
    }

    /// A `VkAndroidSurfaceCreateInfoKHR` in guest memory.
    fn android_surface_info(&self, window: u64) -> u64 {
        self.android_surface_info_raw(STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, 0, 0, window)
    }

    fn android_surface_info_raw(&self, stype: u32, next: u64, flags: u32, window: u64) -> u64 {
        let mut bytes = vec![0u8; ANDROID_SURFACE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&stype.to_le_bytes());
        bytes[8..16].copy_from_slice(&next.to_le_bytes());
        bytes[16..20].copy_from_slice(&flags.to_le_bytes());
        bytes[24..32].copy_from_slice(&window.to_le_bytes());
        let at = self.alloc(ANDROID_SURFACE_CREATE_INFO_BYTES);
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// A `VkDeviceCreateInfo` asking for `queues` = `(family, priorities)` and `extensions`.
    fn device_create_info(&self, queues: &[(u32, &[f32])], extensions: &[&str]) -> u64 {
        let mut queue_bytes = Vec::new();
        for (family, priorities) in queues {
            let raw: Vec<u8> = priorities.iter().flat_map(|p| p.to_le_bytes()).collect();
            let priorities_at = self.alloc(raw.len().max(4));
            self.guest.write_bytes(priorities_at as GuestAddr, &raw);
            let mut entry = vec![0u8; DEVICE_QUEUE_CREATE_INFO_BYTES];
            entry[0..4].copy_from_slice(&2u32.to_le_bytes()); // DEVICE_QUEUE_CREATE_INFO
            entry[20..24].copy_from_slice(&family.to_le_bytes());
            entry[24..28].copy_from_slice(&(priorities.len() as u32).to_le_bytes());
            entry[32..40].copy_from_slice(&priorities_at.to_le_bytes());
            queue_bytes.extend_from_slice(&entry);
        }
        let queues_at = self.alloc(queue_bytes.len().max(8));
        self.guest.write_bytes(queues_at as GuestAddr, &queue_bytes);

        let pointers: Vec<u64> = extensions.iter().map(|name| self.cstr(name)).collect();
        let extensions_at = self.u64_array(&pointers);

        let mut bytes = vec![0u8; DEVICE_CREATE_INFO_BYTES];
        bytes[0..4].copy_from_slice(&3u32.to_le_bytes()); // VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO
        bytes[20..24].copy_from_slice(&(queues.len() as u32).to_le_bytes());
        bytes[24..32].copy_from_slice(&queues_at.to_le_bytes());
        bytes[48..52].copy_from_slice(&(extensions.len() as u32).to_le_bytes());
        bytes[56..64].copy_from_slice(&extensions_at.to_le_bytes());
        let at = self.alloc(DEVICE_CREATE_INFO_BYTES);
        self.guest.write_bytes(at as GuestAddr, &bytes);
        at
    }

    /// Every `VkPhysicalDevice` the guest can see, through **both** halves of the protocol.
    ///
    /// Written out rather than shortened, because the shortcut is the bug: a caller that skips the
    /// count-only call and passes an uninitialised `pPropertyCount` is telling this layer its
    /// buffer has room for whatever was on the stack, and the correct response to that is to
    /// write that many entries -- which is zero, most of the time, and silently.
    fn physical_devices(&self, entry_point: u64, instance: u64) -> Vec<u64> {
        let enumerate = self.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
        let count_at = self.alloc(4);
        assert_eq!(
            self.call(enumerate, [instance, count_at, 0, 0]).expect("the count call") as i32,
            VK_SUCCESS
        );
        let count = self.read_u32(count_at) as usize;
        assert!(count > 0, "this driver reports no physical device at all");
        let array_at = self.alloc(count * 8);
        assert_eq!(
            self.call(enumerate, [instance, count_at, array_at, 0]).expect("the array call") as i32,
            VK_SUCCESS
        );
        (0..count).map(|i| self.guest.read_u64(array_at as GuestAddr + i * 8)).collect()
    }

    fn read_u32(&self, at: u64) -> u32 {
        self.guest.read_u64(at as GuestAddr) as u32
    }

    /// Read `len` bytes back out of guest memory, unaligned-safe.
    fn read_bytes(&self, at: u64, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        let mut offset = 0;
        while out.len() < len {
            out.extend_from_slice(&self.guest.read_u64(at as GuestAddr + offset).to_le_bytes());
            offset += 8;
        }
        out.truncate(len);
        out
    }
}

/// A window source with a handle, which is what `vkCreateAndroidSurfaceKHR` needs.
fn a_source_with_a_handle() -> Arc<HostWindowSource> {
    let source = HostWindowSource::unpublished();
    source.publish(1024, 576).expect("a publishable size");
    source.set_raw_window(RawWindow::Win32 { hwnd: FAKE_HWND, hinstance: FAKE_HINSTANCE });
    source
}

// ============================================================= vkCreateAndroidSurfaceKHR

/// **A surface is created over the window behind the guest's `ANativeWindow *`, and the
/// substitution is recorded.**
///
/// Four assertions, because any one alone can be satisfied by the wrong implementation:
///
/// * the guest receives a handle in this layer's **surface** registry — not the driver's value;
/// * the host was handed the `HWND` the window source published — not some other window;
/// * the rewrite log names both calls **and** the window system — a silent substitution would
///   fail this and nothing else;
/// * `Vulkan::report()` prints it, so a gate that never reads the log still sees it.
#[test]
fn a_surface_is_created_over_the_host_window_and_the_substitution_is_recorded() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("surface", Some(host.clone()));
    f.ndk.set_window_source(a_source_with_a_handle() as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let create = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");

    let window = f.a_native_window();
    let info = f.android_surface_info(window);
    let out = f.alloc(8);
    let result = f.call(create, [instance, info, 0, out]).expect("the call must complete");
    assert_eq!(result as i32, VK_SUCCESS);

    let handle = f.guest.read_u64(out as GuestAddr);
    assert_ne!(handle, 0, "a VkSurfaceKHR was written");
    let issued = f.vulkan().surface_handles();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].0 as u64, handle, "the handle is the registry's address");
    assert_eq!(issued[0].1, HostSurface::from_token(0));

    // The host was given **this** window and not another.
    let surfaces = host.log().surfaces.clone();
    assert_eq!(surfaces.len(), 1);
    assert_eq!(surfaces[0].0, HostInstance::from_token(0));
    assert_eq!(
        surfaces[0].1,
        RawWindow::Win32 { hwnd: FAKE_HWND, hinstance: FAKE_HINSTANCE },
        "both fields of the handle travelled, and neither was substituted for the other"
    );

    // **The log.** A surface that came out right by coincidence passes everything above.
    let surface_rewrites: Vec<_> = f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|r| matches!(r.site, RewriteSite::SurfaceCall { .. }))
        .collect();
    assert_eq!(surface_rewrites.len(), 1, "{:?}", f.vulkan().rewrites());
    assert_eq!(surface_rewrites[0].from, "vkCreateAndroidSurfaceKHR");
    assert_eq!(surface_rewrites[0].to, "vkCreateWin32SurfaceKHR");
    assert_eq!(surface_rewrites[0].site, RewriteSite::SurfaceCall { system: "win32" });
    assert_ne!(surface_rewrites[0].caller, 0, "and it points at the instruction that caused it");
    assert_eq!(f.vulkan().rewrites_dropped(), 0);

    let report = f.vulkan().report();
    assert!(report.contains("satisfied on this host's win32 window by"), "{report}");
    assert!(report.contains("vkCreateWin32SurfaceKHR"), "{report}");
    assert!(report.contains("VkSurfaceKHR handles issued: 1"), "{report}");
}

/// **`vkGetInstanceProcAddr("vkCreateAndroidSurfaceKHR")` answers a thunk the driver said it did
/// not have — and says so in the log.**
///
/// The regression test for what the first live run found. No host driver has
/// `vkCreateAndroidSurfaceKHR`, so [`VulkanHost::has_instance_proc`] answers `false` for it and a
/// layer that forwarded the question would hand the engine NULL one call after telling it
/// `VK_KHR_android_surface` exists. This layer answers on its own authority instead, conditional
/// on the driver having the host call it would satisfy the command with, and the substitution is a
/// [`RewriteSite::Resolved`] entry beside the two extension renames.
///
/// The double is deliberately built **without** `vkCreateAndroidSurfaceKHR`, so this test fails if
/// the special case is removed.
#[test]
fn the_android_surface_entry_point_resolves_although_the_driver_does_not_have_it() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("resolve", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);

    let thunk = f
        .proc_addr(entry_point, instance, "vkCreateAndroidSurfaceKHR")
        .expect("the lookup must complete");
    assert_ne!(
        thunk, 0,
        "the driver has no such command and this layer implements it, so a NULL here would          strand the engine one call after it was told the extension exists"
    );
    assert!(
        f.boundary.symbol_at(thunk as GuestAddr).is_some(),
        "and what the guest got is a thunk in this boundary, not a host address"
    );

    let resolved: Vec<_> = f
        .vulkan()
        .rewrites()
        .into_iter()
        .filter(|r| r.site == RewriteSite::Resolved)
        .collect();
    assert_eq!(resolved.len(), 1, "{:?}", f.vulkan().rewrites());
    assert_eq!(resolved[0].from, "vkCreateAndroidSurfaceKHR");
    assert_eq!(resolved[0].to, "vkCreateWin32SurfaceKHR", "the host named the call, not this crate");
    assert_eq!(resolved[0].spec_version, None, "a command has no specVersion");
    assert!(
        f.vulkan().report().contains("resolved to a thunk this layer satisfies with the host's"),
        "{}",
        f.vulkan().report()
    );

    // A second lookup gets the same address -- the engine stores and compares these -- and the
    // log does not grow, because the pool already had the name.
    let again = f.proc_addr(entry_point, instance, "vkCreateAndroidSurfaceKHR").expect("again");
    assert_eq!(again, thunk, "one function, one address");
}

/// **A host with no platform surface call answers NULL, on the driver's authority.**
///
/// The other side of the special case, and the reason it is a *measurement* rather than an
/// assertion: this layer never claims it can create a surface. It asks the host what its own
/// surface call is and asks the driver whether it has that, and a host that has none produces the
/// NULL a conforming loader owes — with **no** rewrite recorded, because nothing was substituted.
#[test]
fn a_host_with_no_platform_surface_call_answers_null_and_logs_no_substitution() {
    let _serial = serialized();
    let host = StageThreeHost::without_a_platform_surface_call();
    let f = fixture("noplatformcall", Some(host));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);

    let answer = f
        .proc_addr(entry_point, instance, "vkCreateAndroidSurfaceKHR")
        .expect("the lookup must complete");
    assert_eq!(answer, 0, "the driver has nothing to satisfy it with");
    assert!(
        f.vulkan().rewrites().iter().all(|r| r.site != RewriteSite::Resolved),
        "and nothing was logged as a substitution that did not happen: {:?}",
        f.vulkan().rewrites()
    );
    // The census records it as the **driver's** NULL rather than the specification's, which is
    // what lets a reader tell a misconfigured host from a name this layer has no table entry for.
    let answers: Vec<String> = f
        .vulkan()
        .requests()
        .into_iter()
        .filter(|r| r.name == "vkCreateAndroidSurfaceKHR")
        .map(|r| format!("{:?}", r.answer))
        .collect();
    assert_eq!(answers.len(), 1);
    assert!(answers[0].contains("host driver"), "{answers:?}");
}

/// **An embedding with no window source refuses by name**, and names what to call.
///
/// A geometry constant is deliberately **not** enough: a width and a height are a size, and a
/// surface needs a window. That is asserted here rather than left implicit, because
/// `Ndk::set_window_geometry` is what every other test in this workspace uses.
#[test]
fn a_surface_without_a_window_source_refuses_and_names_set_window_source() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("nosource", Some(host.clone()));
    f.ndk.set_window_geometry(
        omni_android::ndk::WindowGeometry::new(1280, 720).expect("a positive geometry"),
    );

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let create = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
    let window = f.a_native_window();
    let info = f.android_surface_info(window);
    let out = f.alloc(8);

    let text = f.refusal(create, [instance, info, 0, out]).to_string();
    assert!(text.contains("Ndk::set_window_source"), "{text}");
    assert!(text.contains("HostWindowSource::watching"), "{text}");
    assert!(text.contains("a width and a height are a size, and a surface needs a window"), "{text}");
    assert!(host.log().surfaces.is_empty(), "the host was never asked");
    assert!(f.vulkan().surface_handles().is_empty(), "and nothing was issued");
    assert!(
        f.vulkan().rewrites().iter().all(|r| !matches!(r.site, RewriteSite::SurfaceCall { .. })),
        "and nothing was logged as a substitution that did not happen"
    );
}

/// **A source that reports no OS handle refuses naming the method**, which is a different host
/// state from having no source at all and has a different fix.
#[test]
fn a_source_with_no_raw_window_refuses_naming_the_method_that_publishes_one() {
    let _serial = serialized();
    let f = fixture("nohandle", Some(StageThreeHost::new()));
    // Fed by hand, which is a legitimate source with a geometry and no window.
    let source = HostWindowSource::unpublished();
    source.publish(800, 600).expect("a publishable size");
    f.ndk.set_window_source(source as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let create = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
    let window = f.a_native_window();
    let info = f.android_surface_info(window);
    let out = f.alloc(8);

    let text = f.refusal(create, [instance, info, 0, out]).to_string();
    assert!(text.contains("WindowSource::raw_window"), "{text}");
    assert!(text.contains("HostWindowSource::set_raw_window"), "{text}");
    assert!(text.contains("VK_ERROR_NATIVE_WINDOW_IN_USE_KHR"), "and why not that: {text}");
}

/// **A wild `ANativeWindow *` refuses and never reaches the host**, and so do a wrong `sType`, a
/// `pNext` chain and a non-zero reserved `flags`.
#[test]
fn the_surface_create_info_is_checked_field_by_field() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("surfinfo", Some(host.clone()));
    f.ndk.set_window_source(a_source_with_a_handle() as Arc<dyn WindowSource>);
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let create = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
    let out = f.alloc(8);
    let window = f.a_native_window();

    // A window pointer the guest invented.
    let wild = f.android_surface_info(window + 0x4000);
    let text = f.refusal(create, [instance, wild, 0, out]).to_string();
    assert!(text.contains("not a live `ANativeWindow`"), "{text}");
    assert!(text.contains("whichever window this host happens to have"), "{text}");

    // NULL.
    let null = f.android_surface_info(0);
    let text = f.refusal(create, [instance, null, 0, out]).to_string();
    assert!(text.contains("window = NULL"), "{text}");
    assert!(text.contains("ANativeWindow_fromSurface"), "{text}");

    // The wrong structure type.
    let wrong = f.android_surface_info_raw(1, 0, 0, window);
    let text = f.refusal(create, [instance, wrong, 0, out]).to_string();
    assert!(text.contains("ANDROID_SURFACE_CREATE_INFO_KHR"), "{text}");
    assert!(text.contains("byte 24"), "{text}");

    // A pNext chain, whose address the refusal names so it can be decoded next time.
    let chain = f.alloc(32);
    let chained = f.android_surface_info_raw(STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, chain, 0, window);
    let text = f.refusal(create, [instance, chained, 0, out]).to_string();
    assert!(text.contains(&format!("{chain:#x}")), "{text}");

    // A reserved flags field the guest set.
    let flagged =
        f.android_surface_info_raw(STYPE_ANDROID_SURFACE_CREATE_INFO_KHR, 0, 1, window);
    let text = f.refusal(create, [instance, flagged, 0, out]).to_string();
    assert!(text.contains("reserved for future use"), "{text}");

    // A non-null pAllocator, which is counted as well as refused.
    let good = f.android_surface_info(window);
    let text = f.refusal(create, [instance, good, 0xCAFE_1000, out]).to_string();
    assert!(text.contains("VkAllocationCallbacks"), "{text}");
    assert!(f.vulkan().allocator_non_null() >= 1);
    assert_eq!(f.vulkan().first_allocator(), Some(0xCAFE_1000));
    assert_eq!(f.vulkan().first_allocator_call().as_deref(), Some("vkCreateAndroidSurfaceKHR"));

    assert!(host.log().surfaces.is_empty(), "not one of those reached the host");
    assert_eq!(f.guest.read_u64(out as GuestAddr), 0, "and nothing was written back");
}

// ========================================================== the two-count idiom

/// **The two-call protocol, end to end, on `vkEnumeratePhysicalDevices`.**
///
/// The three things this asserts are the three the idiom can get wrong:
///
/// * the count-only call writes the count and nothing else;
/// * the array call writes exactly `min(capacity, available)` entries and writes **that** back as
///   the count — not the driver's;
/// * asking twice produces **the same handles**, which the specification requires and which only
///   a deduplicating registry provides.
#[test]
fn enumerate_physical_devices_follows_the_two_call_protocol_and_is_stable() {
    let _serial = serialized();
    let f = fixture("enumerate", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");

    // 1. The count alone.
    let count_at = f.alloc(4);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    assert_eq!(f.read_u32(count_at), 2, "the driver's own count");

    // 2. The array, with room for both.
    let array_at = f.poisoned(4 * 8, 0xAA);
    assert_eq!(
        f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32,
        VK_SUCCESS,
        "the array was big enough, so this is not VK_INCOMPLETE"
    );
    assert_eq!(f.read_u32(count_at), 2);
    let first = f.guest.read_u64(array_at as GuestAddr);
    let second = f.guest.read_u64(array_at as GuestAddr + 8);
    assert_ne!(first, 0);
    assert_ne!(first, second, "two devices are two handles");
    assert_eq!(
        f.guest.read_u64(array_at as GuestAddr + 16),
        0xAAAA_AAAA_AAAA_AAAA,
        "and nothing was written past the two entries there are"
    );

    // Both handles are addresses in **this layer's** registry, not the driver's pointers.
    let issued = f.vulkan().physical_device_handles();
    assert_eq!(issued.len(), 2);
    assert_eq!(issued[0].0 as u64, first);
    assert_eq!(issued[1].0 as u64, second);

    // 3. Asking again gets the same handles and consumes no further slots.
    let again = f.poisoned(2 * 8, 0xBB);
    f.guest.write_u32(count_at as GuestAddr, 2);
    assert_eq!(f.call(enumerate, [instance, count_at, again, 0]).expect("again") as i32, VK_SUCCESS);
    assert_eq!(f.guest.read_u64(again as GuestAddr), first, "the same handle, both times");
    assert_eq!(f.guest.read_u64(again as GuestAddr + 8), second);
    assert_eq!(f.vulkan().physical_device_handles().len(), 2, "and no slot was consumed");
}

/// **A `pPropertyCount` smaller than the driver's count gets `VK_INCOMPLETE`, the count of what
/// fitted, and not one byte past the end of the guest's buffer.**
///
/// This is the assertion Global Constraint 11 is about on this path. The array is deliberately
/// sized for **one** entry with a poison byte beyond it, so a handler that wrote `available`
/// entries would be caught here rather than in a driver crash three frames later.
#[test]
fn a_short_array_is_vk_incomplete_and_writes_nothing_past_the_capacity() {
    let _serial = serialized();
    let f = fixture("incomplete", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");

    let count_at = f.alloc(4);
    let array_at = f.poisoned(4 * 8, 0xAA);
    f.guest.write_u32(count_at as GuestAddr, 1); // room for one, and there are two
    let result = f.call(enumerate, [instance, count_at, array_at, 0]).expect("it must complete");
    assert_eq!(result as i32, VK_INCOMPLETE, "one of two fitted");
    assert_eq!(f.read_u32(count_at), 1, "and the count written back is what fitted");
    assert_ne!(f.guest.read_u64(array_at as GuestAddr), 0xAAAA_AAAA_AAAA_AAAA, "one was written");
    assert_eq!(
        f.guest.read_u64(array_at as GuestAddr + 8),
        0xAAAA_AAAA_AAAA_AAAA,
        "and the second entry -- which the guest has no room for -- was not touched"
    );

    // A capacity of exactly the available count is VK_SUCCESS: the boundary case a `<` written as
    // `<=` gets wrong in the direction nobody notices.
    f.guest.write_u32(count_at as GuestAddr, 2);
    assert_eq!(f.call(enumerate, [instance, count_at, array_at, 0]).expect("exact") as i32, VK_SUCCESS);
    assert_eq!(f.read_u32(count_at), 2);

    // A capacity of zero with a non-NULL array is legal and writes nothing at all.
    let empty = f.poisoned(8, 0xCC);
    f.guest.write_u32(count_at as GuestAddr, 0);
    assert_eq!(f.call(enumerate, [instance, count_at, empty, 0]).expect("zero") as i32, VK_INCOMPLETE);
    assert_eq!(f.read_u32(count_at), 0);
    assert_eq!(f.guest.read_u64(empty as GuestAddr), 0xCCCC_CCCC_CCCC_CCCC, "untouched");
}

/// **`vkGetPhysicalDeviceQueueFamilyProperties` returns `void` and still truncates correctly.**
///
/// There is no `VK_INCOMPLETE` to answer with, so the count written back is the only thing that
/// says the array was too small — and a handler that invented a `VkResult` would be writing into a
/// register the guest is not going to read. The families are deliberately **different** from each
/// other, so a handler that wrote the first one twice is caught.
#[test]
fn the_queue_family_query_truncates_without_inventing_a_result() {
    let _serial = serialized();
    let f = fixture("families", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let families = f.resolve(entry_point, instance, "vkGetPhysicalDeviceQueueFamilyProperties");

    let device = f.physical_devices(entry_point, instance)[0];
    let count_at = f.alloc(4);

    // The count alone.
    f.call(families, [device, count_at, 0, 0]).expect("the count call");
    assert_eq!(f.read_u32(count_at), 2);

    // Room for one of two.
    let out = f.poisoned(2 * QUEUE_FAMILY_PROPERTIES_BYTES, 0x5A);
    f.guest.write_u32(count_at as GuestAddr, 1);
    f.call(families, [device, count_at, out, 0]).expect("the array call");
    assert_eq!(f.read_u32(count_at), 1, "the count is the only truncation signal this call has");
    let written = f.read_bytes(out, QUEUE_FAMILY_PROPERTIES_BYTES);
    assert_eq!(
        u32::from_le_bytes(written[0..4].try_into().expect("four")),
        0x0000_0007,
        "family 0's flags, verbatim"
    );
    assert_eq!(u32::from_le_bytes(written[4..8].try_into().expect("four")), 1, "its queueCount");
    let past = f.read_bytes(out + QUEUE_FAMILY_PROPERTIES_BYTES as u64, 8);
    assert!(past.iter().all(|&b| b == 0x5A), "the second family was not written: {past:?}");

    // Room for both: the two entries are different, which a handler writing one twice would fail.
    f.guest.write_u32(count_at as GuestAddr, 2);
    f.call(families, [device, count_at, out, 0]).expect("both");
    assert_eq!(f.read_u32(count_at), 2);
    let both = f.read_bytes(out, 2 * QUEUE_FAMILY_PROPERTIES_BYTES);
    assert_eq!(u32::from_le_bytes(both[0..4].try_into().expect("four")), 0x0000_0007);
    assert_eq!(
        u32::from_le_bytes(
            both[QUEUE_FAMILY_PROPERTIES_BYTES..QUEUE_FAMILY_PROPERTIES_BYTES + 4]
                .try_into()
                .expect("four")
        ),
        0x0000_0004,
        "family 1's flags, which are not family 0's"
    );
    assert_eq!(
        u32::from_le_bytes(
            both[QUEUE_FAMILY_PROPERTIES_BYTES + 4..QUEUE_FAMILY_PROPERTIES_BYTES + 8]
                .try_into()
                .expect("four")
        ),
        8
    );
}

// ========================================================== the single-structure queries

/// **The driver's bytes arrive in the guest's buffer unchanged, to the last byte.**
///
/// The blob the double answers with has a marker in its **final** four bytes, so a write that was
/// short by any amount is caught — which is the whole risk of carrying a structure as bytes.
#[test]
fn a_structure_query_writes_the_drivers_bytes_verbatim() {
    let _serial = serialized();
    let f = fixture("structs", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let properties = f.resolve(entry_point, instance, "vkGetPhysicalDeviceProperties");
    let memory = f.resolve(entry_point, instance, "vkGetPhysicalDeviceMemoryProperties");
    let features = f.resolve(entry_point, instance, "vkGetPhysicalDeviceFeatures");

    let devices = f.physical_devices(entry_point, instance);
    let (first, second) = (devices[0], devices[1]);

    let out = f.poisoned(PHYSICAL_DEVICE_PROPERTIES_BYTES, 0x77);
    f.call(properties, [first, out, 0, 0]).expect("properties");
    let bytes = f.read_bytes(out, PHYSICAL_DEVICE_PROPERTIES_BYTES);
    assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().expect("four")), 0x0040_1000, "apiVersion");
    assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().expect("four")), 0x10DE, "vendorID");
    let name_end = bytes[20..276].iter().position(|&b| b == 0).expect("a NUL-terminated name");
    assert_eq!(
        std::str::from_utf8(&bytes[20..20 + name_end]).expect("ASCII"),
        "OMNI Reference GPU",
        "deviceName, at offset 20"
    );
    assert_eq!(
        u32::from_le_bytes(
            bytes[PHYSICAL_DEVICE_PROPERTIES_BYTES - 4..].try_into().expect("four")
        ),
        0xABCD_1234,
        "the marker in the final four bytes: the whole 824 arrived"
    );

    // The **second** device answers with the second blob, which is what says the handle selected
    // a device rather than the handler answering about whichever one came first.
    f.call(properties, [second, out, 0, 0]).expect("the other device");
    let other = f.read_bytes(out, PHYSICAL_DEVICE_PROPERTIES_BYTES);
    let end = other[20..276].iter().position(|&b| b == 0).expect("a name");
    assert_eq!(std::str::from_utf8(&other[20..20 + end]).expect("ASCII"), "OMNI Software Rasteriser");

    // Memory properties, whose last written field is a heap size eight bytes wide.
    let memory_out = f.poisoned(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES, 0x33);
    f.call(memory, [first, memory_out, 0, 0]).expect("memory");
    let bytes = f.read_bytes(memory_out, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().expect("four")), 1, "memoryTypeCount");
    assert_eq!(u32::from_le_bytes(bytes[260..264].try_into().expect("four")), 1, "memoryHeapCount");
    assert_eq!(
        u64::from_le_bytes(bytes[264..272].try_into().expect("eight")),
        8 << 30,
        "the heap's VkDeviceSize, 8 bytes at offset 264"
    );

    // Format properties: the format the guest named reached the host, in `w1`, and the three
    // members came back in order. The upper half of `x1` is set to show it is not read.
    let format_properties = f.resolve(entry_point, instance, "vkGetPhysicalDeviceFormatProperties");
    let format_out = f.poisoned(FORMAT_PROPERTIES_BYTES, 0x11);
    f.call(format_properties, [first, 0xFFFF_FFFF_0000_0053, format_out, 0]).expect("formats");
    let bytes = f.read_bytes(format_out, FORMAT_PROPERTIES_BYTES);
    let members: Vec<u32> =
        bytes.chunks(4).map(|word| u32::from_le_bytes(word.try_into().expect("four"))).collect();
    assert_eq!(members, vec![83, 84, 85], "format 83's linear, optimal and buffer features");

    // Features, whose 220th byte is the last VkBool32.
    let features_out = f.poisoned(PHYSICAL_DEVICE_FEATURES_BYTES, 0x11);
    f.call(features, [first, features_out, 0, 0]).expect("features");
    let bytes = f.read_bytes(features_out, PHYSICAL_DEVICE_FEATURES_BYTES);
    assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().expect("four")), 1);
    assert_eq!(
        u32::from_le_bytes(bytes[PHYSICAL_DEVICE_FEATURES_BYTES - 4..].try_into().expect("four")),
        1,
        "the 55th VkBool32 arrived"
    );
}

/// **A NULL output pointer refuses for every query that has exactly one output.**
///
/// These calls return `void` or a `VkResult` with no other channel, so a handler that returned
/// quietly would leave the guest reading its own uninitialised buffer as the driver's answer.
#[test]
fn a_null_output_pointer_refuses_for_every_single_structure_query() {
    let _serial = serialized();
    let f = fixture("nullout", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let device = f.physical_devices(entry_point, instance)[0];

    for (name, field) in [
        ("vkGetPhysicalDeviceProperties", "pProperties"),
        ("vkGetPhysicalDeviceFeatures", "pFeatures"),
        ("vkGetPhysicalDeviceMemoryProperties", "pMemoryProperties"),
        // Its output is the third argument; the NULL in `x1` is `VK_FORMAT_UNDEFINED`.
        ("vkGetPhysicalDeviceFormatProperties", "pFormatProperties"),
    ] {
        let thunk = f.resolve(entry_point, instance, name);
        let text = f.refusal(thunk, [device, 0, 0, 0]).to_string();
        assert!(text.contains(&format!("{field} = NULL")), "{name}: {text}");
        assert!(text.contains("reading whatever was already in its own buffer"), "{name}: {text}");
    }

    // And the count pointer in the two-call idiom, which is required in **both** halves.
    let families = f.resolve(entry_point, instance, "vkGetPhysicalDeviceQueueFamilyProperties");
    let text = f.refusal(families, [device, 0, 0, 0]).to_string();
    assert!(text.contains("NULL count pointer"), "{text}");
    assert!(text.contains("both**"), "and that it is needed in both halves: {text}");
}

// ================================================================== handles across families

/// **A handle of the wrong family is a typed refusal that names both families.**
///
/// This is what the per-family registries buy that one opaque check would not:
/// `vkGetPhysicalDeviceSurfaceSupportKHR` takes a `VkPhysicalDevice` **and** a `VkSurfaceKHR`, and
/// a guest that swapped them gets a refusal saying which argument was of the wrong kind rather
/// than a lookup that lands somewhere.
#[test]
fn a_handle_of_the_wrong_family_refuses_and_names_the_family() {
    let _serial = serialized();
    let f = fixture("families-mixed", Some(StageThreeHost::new()));
    f.ndk.set_window_source(a_source_with_a_handle() as Arc<dyn WindowSource>);
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);

    let device = f.physical_devices(entry_point, instance)[0];

    let create = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
    let window = f.a_native_window();
    let info = f.android_surface_info(window);
    let out = f.alloc(8);
    f.call(create, [instance, info, 0, out]).expect("surface");
    let surface = f.guest.read_u64(out as GuestAddr);

    let support = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceSupportKHR");
    let supported_at = f.alloc(4);

    // The right way round works, so the test is not passing because everything refuses.
    assert_eq!(
        f.call(support, [device, 0, surface, supported_at]).expect("support") as i32,
        VK_SUCCESS
    );
    assert_eq!(f.read_u32(supported_at), 1, "family 0 can present");
    assert_eq!(
        f.call(support, [device, 1, surface, supported_at]).expect("support") as i32,
        VK_SUCCESS
    );
    assert_eq!(f.read_u32(supported_at), 0, "family 1 cannot, and VK_FALSE is not a failure");

    // Swapped.
    let text = f.refusal(support, [surface, 0, device, supported_at]).to_string();
    assert!(text.contains("`VkPhysicalDevice`"), "{text}");
    assert!(text.contains("not a handle this layer issued"), "{text}");

    // A handle one byte off a slot boundary, which is the check the registry exists for.
    for offset in [1u64, 4, 8, 15] {
        let text = f.refusal(support, [device + offset, 0, surface, supported_at]).to_string();
        assert!(
            text.contains("not a handle this layer issued"),
            "{offset} bytes past a handle must not name it: {text}"
        );
    }

    // And a NULL `pSupported`, which is this call's only output.
    let text = f.refusal(support, [device, 0, surface, 0]).to_string();
    assert!(text.contains("pSupported = NULL"), "{text}");
    assert!(text.contains("can this queue family present?"), "{text}");
}

// ============================================================================ vkCreateDevice

/// **A real device: the request reaches the host decoded field by field, the guest gets a registry
/// handle, and the queues deduplicate.**
#[test]
fn vk_create_device_decodes_the_request_and_the_queues_deduplicate() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("device", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);

    let physical = f.physical_devices(entry_point, instance)[0];

    let create = f.resolve(entry_point, instance, "vkCreateDevice");
    let info = f.device_create_info(&[(0, &[1.0, 0.5]), (1, &[0.25])], &["VK_KHR_swapchain"]);
    let out = f.alloc(8);
    assert_eq!(f.call(create, [physical, info, 0, out]).expect("create") as i32, VK_SUCCESS);

    // What the **host** was asked for.
    let requests = host.log().devices.clone();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].extensions, vec!["VK_KHR_swapchain".to_string()]);
    assert!(requests[0].layers.is_empty());
    assert_eq!(requests[0].queues.len(), 2);
    assert_eq!(requests[0].queues[0].family_index, 0);
    assert_eq!(requests[0].queues[0].priorities, vec![1.0, 0.5], "both floats, in order");
    assert_eq!(requests[0].queues[1].family_index, 1);
    assert_eq!(requests[0].queues[1].priorities, vec![0.25]);
    assert!(requests[0].features.is_none(), "a NULL pEnabledFeatures stays None");

    // What the **guest** got.
    let device = f.guest.read_u64(out as GuestAddr);
    assert_ne!(device, 0);
    let issued = f.vulkan().device_handles();
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].0 as u64, device);

    // Queues: the same family and index is the same handle, a different one is not.
    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [device, 0, 0, queue_at]).expect("queue");
    let first = f.guest.read_u64(queue_at as GuestAddr);
    assert_ne!(first, 0);
    f.call(get_queue, [device, 0, 0, queue_at]).expect("queue again");
    assert_eq!(f.guest.read_u64(queue_at as GuestAddr), first, "one (family, index), one VkQueue");
    f.call(get_queue, [device, 0, 1, queue_at]).expect("another index");
    let second = f.guest.read_u64(queue_at as GuestAddr);
    assert_ne!(second, first, "a different index is a different queue");
    assert_eq!(f.vulkan().queue_handles().len(), 2, "three calls, two handles");
    assert_eq!(host.log().queues.len(), 3, "and the host was asked every time");

    // A NULL pQueue refuses: this call returns `void`, so there is no other channel.
    let text = f.refusal(get_queue, [device, 0, 0, 0]).to_string();
    assert!(text.contains("pQueue = NULL"), "{text}");
    assert!(text.contains("returns `void`"), "{text}");

    let report = f.vulkan().report();
    assert!(report.contains("VkDevice handles issued: 1"), "{report}");
    assert!(report.contains("VkQueue handles issued: 2"), "{report}");
    assert!(report.contains("deduplicated"), "{report}");
}

/// **`pEnabledFeatures` travels as its 220 bytes, the engine's `pNext` chain as its members, and
/// a chained structure this layer does not carry refuses naming its `sType` and address.**
#[test]
fn enabled_features_and_the_measured_chain_travel_verbatim_and_other_chains_refuse() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("features", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let physical = f.physical_devices(entry_point, instance)[0];
    let create = f.resolve(entry_point, instance, "vkCreateDevice");
    let out = f.alloc(8);

    // A features structure with the first and the last member set.
    let mut features = vec![0u8; PHYSICAL_DEVICE_FEATURES_BYTES];
    features[0..4].copy_from_slice(&1u32.to_le_bytes());
    features[PHYSICAL_DEVICE_FEATURES_BYTES - 4..].copy_from_slice(&1u32.to_le_bytes());
    let features_at = f.alloc(PHYSICAL_DEVICE_FEATURES_BYTES);
    f.guest.write_bytes(features_at as GuestAddr, &features);

    let info = f.device_create_info(&[(0, &[1.0])], &[]);
    f.guest.write_bytes(info as GuestAddr + 64, &features_at.to_le_bytes());
    assert_eq!(f.call(create, [physical, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let request = host.log().devices.last().cloned().expect("one request");
    assert_eq!(
        request.features.as_deref(),
        Some(features.as_slice()),
        "all 220 bytes, byte for byte"
    );

    // The engine's chain (`0x2590794`..`0x25907f8`): extended dynamic state, then YCbCr, then
    // multiview. It travels in that order, as member bytes, with no guest pointer in it.
    let eds = f.alloc(24);
    let ycbcr = f.alloc(24);
    let multiview = f.alloc(32);
    let link = |at: u64, s_type: u32, next: u64, members: &[u32]| {
        f.guest.write_u64(at as GuestAddr, u64::from(s_type));
        f.guest.write_u64(at as GuestAddr + 8, next);
        for (k, member) in members.iter().enumerate() {
            f.guest.write_u32(at as GuestAddr + 16 + 4 * k as GuestAddr, *member);
        }
    };
    link(eds, 1_000_267_000, ycbcr, &[1]);
    link(ycbcr, 1_000_156_004, multiview, &[1]);
    link(multiview, 1_000_053_001, 0, &[1, 0, 1]);
    let chained = f.device_create_info(&[(0, &[1.0])], &[]);
    f.guest.write_bytes(chained as GuestAddr + 8, &eds.to_le_bytes());
    assert_eq!(f.call(create, [physical, chained, 0, out]).expect("create") as i32, VK_SUCCESS);
    let members = |values: &[u32]| -> Vec<u8> { values.iter().flat_map(|v| v.to_le_bytes()).collect() };
    assert_eq!(
        host.log().devices.last().expect("a request").chain,
        vec![
            ChainLink { s_type: 1_000_267_000, body: members(&[1]) },
            ChainLink { s_type: 1_000_156_004, body: members(&[1]) },
            ChainLink { s_type: 1_000_053_001, body: members(&[1, 0, 1]) },
        ],
        "every structure, in the guest's order, members only"
    );

    // A structure this layer does not carry -- `VkPhysicalDeviceVulkan12Features` (51) -- refuses,
    // naming its sType and where it is, rather than being dropped.
    let chain = f.alloc(64);
    f.guest.write_u32(chain as GuestAddr, 51);
    link(multiview, 1_000_053_001, chain, &[1, 0, 1]);
    let text = f.refusal(create, [physical, chained, 0, out]).to_string();
    assert!(text.contains(&format!("{chain:#x}")), "{text}");
    assert!(text.contains("`sType` 51"), "{text}");
    assert!(text.contains("structure 3"), "and its place in the chain: {text}");

    // A wrong queue sType, which would make `pQueuePriorities` be read from the wrong offset.
    let bad = f.device_create_info(&[(0, &[1.0])], &[]);
    let queues_at = f.guest.read_u64(bad as GuestAddr + 24);
    f.guest.write_u32(queues_at as GuestAddr, 99);
    let text = f.refusal(create, [physical, bad, 0, out]).to_string();
    assert!(text.contains("DEVICE_QUEUE_CREATE_INFO"), "{text}");
}

/// **`vkGetPhysicalDeviceFeatures2KHR` writes the host's features and answers each chained
/// structure in place -- members only.** The guest's `sType`s, `pNext`s and tail padding are never
/// written, and the host is asked through the name the guest called. The engine's own shape: a
/// YCbCr structure chained to an extended-dynamic-state one (`0x2590580`..`0x25905e8`).
#[test]
fn features2_answers_the_features_and_each_chained_structure_in_place() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("features2", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let physical = f.physical_devices(entry_point, instance)[0];
    let features2 = f.resolve(entry_point, instance, "vkGetPhysicalDeviceFeatures2KHR");

    let head = f.alloc(PHYSICAL_DEVICE_FEATURES_2_BYTES);
    let ycbcr = f.alloc(24);
    let eds = f.alloc(24);
    let header = |at: u64, s_type: u32, next: u64| {
        f.guest.write_u64(at as GuestAddr, u64::from(s_type));
        f.guest.write_u64(at as GuestAddr + 8, next);
    };
    header(head, STYPE_PHYSICAL_DEVICE_FEATURES_2, ycbcr);
    header(ycbcr, 1_000_156_004, eds);
    header(eds, 1_000_267_000, 0);
    // The member 0, and a sentinel in the tail padding (20..24), which is nobody's to write.
    for at in [ycbcr, eds] {
        f.guest.write_u64(at as GuestAddr + 16, 0xEEEE_EEEE_0000_0000);
    }

    f.call(features2, [physical, head, 0, 0]).expect("the call");
    assert_eq!(
        f.guest.read_u64(head as GuestAddr + 16) as u32,
        1,
        "robustBufferAccess, the host's first member, at offset 16"
    );
    assert_eq!(
        f.guest.read_u64(head as GuestAddr + 16 + PHYSICAL_DEVICE_FEATURES_BYTES as GuestAddr - 4)
            as u32,
        1,
        "and its last"
    );
    assert_eq!(f.guest.read_u64(head as GuestAddr + 8), ycbcr, "the head's pNext, the guest's");
    for (at, s_type, next) in [(ycbcr, 1_000_156_004u32, eds), (eds, 1_000_267_000, 0)] {
        assert_eq!(f.guest.read_u64(at as GuestAddr), u64::from(s_type), "the guest's sType");
        assert_eq!(f.guest.read_u64(at as GuestAddr + 8), next, "the guest's pNext");
        assert_eq!(
            f.guest.read_u64(at as GuestAddr + 16),
            0xEEEE_EEEE_0000_0001,
            "the host's answer in the member, the padding untouched"
        );
    }
    let asked = host.log().features2.clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].0, "vkGetPhysicalDeviceFeatures2KHR", "the name the guest called");
    assert_eq!(
        asked[0].1.iter().map(|link| link.s_type).collect::<Vec<_>>(),
        vec![1_000_156_004, 1_000_267_000],
        "in the guest's order"
    );

    // A chained structure this layer does not carry: named, with its address.
    let unknown = f.alloc(64);
    header(unknown, 51, 0);
    header(eds, 1_000_267_000, unknown);
    let text = f.refusal(features2, [physical, head, 0, 0]).to_string();
    assert!(text.contains("`sType` 51"), "{text}");
    assert!(text.contains(&format!("{unknown:#x}")), "{text}");
    // A chain that points back into itself ends at the bound rather than never.
    header(eds, 1_000_267_000, ycbcr);
    let text = f.refusal(features2, [physical, head, 0, 0]).to_string();
    assert!(text.contains(&format!("longer than {MAX_CHAIN_LINKS}")), "{text}");
    // The wrong structure at the head: its features would land on something else.
    header(head, 1_000_059_001, 0);
    let text = f.refusal(features2, [physical, head, 0, 0]).to_string();
    assert!(text.contains("VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2"), "{text}");
}

/// **`vkGetPhysicalDeviceImageFormatProperties` carries seven registers' worth of question to the
/// host and its answer back** -- or the driver's own `VK_ERROR_FORMAT_NOT_SUPPORTED`, with nothing
/// written. The engine's first question, `(75, 3D, OPTIMAL, 0xd, 0)`, is the one asked here; the
/// upper halves of the scalar registers are set to show only the `w` halves are read.
#[test]
fn image_format_properties_forwards_all_five_scalars_and_the_drivers_refusal() {
    let _serial = serialized();
    let f = fixture("imgfmt", Some(StageThreeHost::new()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let device = f.physical_devices(entry_point, instance)[0];
    let query = f.resolve(entry_point, instance, "vkGetPhysicalDeviceImageFormatProperties");
    let high = 0xAAAA_AAAA_0000_0000u64;

    let out = f.poisoned(IMAGE_FORMAT_PROPERTIES_BYTES, 0x11);
    let result = f.call7(query, [device, high | 75, high | 2, high, high | 0xd, high, out]);
    assert_eq!(result.expect("the call") as i32, VK_SUCCESS);
    let bytes = f.read_bytes(out, IMAGE_FORMAT_PROPERTIES_BYTES);
    let words: Vec<u32> =
        bytes[..24].chunks(4).map(|word| u32::from_le_bytes(word.try_into().expect("four"))).collect();
    assert_eq!(words, vec![75, 2, 0, 0xd, 0, 0], "format, type, tiling, usage, flags, in order");
    assert_eq!(u64::from_le_bytes(bytes[24..32].try_into().expect("eight")), 1 << 30);

    let untouched = f.poisoned(IMAGE_FORMAT_PROPERTIES_BYTES, 0x22);
    let result = f.call7(query, [device, 76, 1, 0, 4, 0, untouched]);
    assert_eq!(result.expect("the call") as i32, -11, "the driver's own answer");
    assert_eq!(
        f.read_bytes(untouched, IMAGE_FORMAT_PROPERTIES_BYTES),
        vec![0x22; IMAGE_FORMAT_PROPERTIES_BYTES],
        "and nothing written"
    );

    let error = f.call7(query, [device, 75, 2, 0, 0xd, 0, 0]).expect_err("a NULL output");
    assert!(error.to_string().contains("pImageFormatProperties = NULL"), "{error}");
}

/// **`vkGetPhysicalDeviceImageFormatProperties2KHR`: the question's five members reach the host,
/// the answer and its chained YCbCr structure come back in place, members only** -- the engine's
/// own call, about format 1000156002 (`0x2590fb8`..`0x2591030`). An unsupported format is the
/// driver's `VK_ERROR_FORMAT_NOT_SUPPORTED` with nothing written; a wrong `sType` refuses.
#[test]
fn image_format_properties2_carries_the_question_and_answers_its_chain_in_place() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("imgfmt2", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let device = f.physical_devices(entry_point, instance)[0];
    let query = f.resolve(entry_point, instance, "vkGetPhysicalDeviceImageFormatProperties2KHR");

    let info = f.alloc(PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2_BYTES);
    let words = |at: u64, values: &[u32]| {
        for (k, value) in values.iter().enumerate() {
            f.guest.write_u32(at as GuestAddr + 4 * k as GuestAddr, *value);
        }
    };
    let ask = |format: u32| {
        f.guest.write_u64(info as GuestAddr, u64::from(STYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2));
        f.guest.write_u64(info as GuestAddr + 8, 0);
        words(info + 16, &[format, 1, 0, 4, 0]);
    };
    let answer = f.poisoned(IMAGE_FORMAT_PROPERTIES_2_BYTES, 0x33);
    let ycbcr = f.alloc(24);
    f.guest.write_u64(answer as GuestAddr, u64::from(STYPE_IMAGE_FORMAT_PROPERTIES_2));
    f.guest.write_u64(answer as GuestAddr + 8, ycbcr);
    f.guest.write_u64(ycbcr as GuestAddr, 1_000_156_005);
    f.guest.write_u64(ycbcr as GuestAddr + 8, 0);
    f.guest.write_u64(ycbcr as GuestAddr + 16, 0xEEEE_EEEE_0000_0000);

    ask(1_000_156_002);
    assert_eq!(f.call(query, [device, info, answer, 0]).expect("the call") as i32, VK_SUCCESS);
    let bytes = f.read_bytes(answer + 16, 32);
    let got: Vec<u32> =
        bytes[..24].chunks(4).map(|word| u32::from_le_bytes(word.try_into().expect("four"))).collect();
    assert_eq!(got, vec![1_000_156_002, 1, 0, 4, 0, 0], "the question's five members, in order");
    assert_eq!(f.guest.read_u64(answer as GuestAddr + 8), ycbcr, "the guest's pNext");
    assert_eq!(
        f.guest.read_u64(ycbcr as GuestAddr + 16),
        0xEEEE_EEEE_0000_0001,
        "combinedImageSamplerDescriptorCount answered, the padding untouched"
    );
    let asked = host.log().image_format2.clone();
    assert_eq!(asked[0].0, "vkGetPhysicalDeviceImageFormatProperties2KHR");
    assert!(asked[0].2.is_empty(), "the engine hangs nothing from the question");

    let untouched = f.poisoned(IMAGE_FORMAT_PROPERTIES_2_BYTES, 0x44);
    f.guest.write_u64(untouched as GuestAddr, u64::from(STYPE_IMAGE_FORMAT_PROPERTIES_2));
    f.guest.write_u64(untouched as GuestAddr + 8, 0);
    ask(44);
    assert_eq!(f.call(query, [device, info, untouched, 0]).expect("the call") as i32, -11);
    assert_eq!(f.read_bytes(untouched + 16, 32), vec![0x44; 32], "nothing written");

    f.guest.write_u64(info as GuestAddr, 1_000_059_003);
    let text = f.refusal(query, [device, info, answer, 0]).to_string();
    assert!(text.contains("PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2"), "{text}");
}

// ========================================================================= vkGetDeviceProcAddr

/// **`vkGetDeviceProcAddr` returns guest thunks, the driver's NULL, and NULL for a null device —
/// and never a host function pointer.**
#[test]
fn device_proc_addr_answers_thunks_and_the_two_nulls() {
    let _serial = serialized();
    let host = StageThreeHost::new();
    let f = fixture("deviceproc", Some(host.clone()));
    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);
    let physical = f.physical_devices(entry_point, instance)[0];
    let create = f.resolve(entry_point, instance, "vkCreateDevice");
    let out = f.alloc(8);
    let info = f.device_create_info(&[(0, &[1.0])], &["VK_KHR_swapchain"]);
    assert_eq!(f.call(create, [physical, info, 0, out]).expect("create") as i32, VK_SUCCESS);
    let device = f.guest.read_u64(out as GuestAddr);

    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");

    // A command the driver has: a **guest** thunk, inside this boundary's thunk region.
    let name = f.cstr("vkCreateSwapchainKHR");
    let swapchain = f.call(get_proc, [device, name, 0, 0]).expect("a lookup");
    assert_ne!(swapchain, 0);
    assert!(
        f.boundary.symbol_at(swapchain as GuestAddr).is_some(),
        "{swapchain:#x} must be a slot in this boundary's thunk region, not a host address"
    );
    assert_eq!(f.vulkan().thunk_for("vkCreateSwapchainKHR"), Some(swapchain as GuestAddr));

    // A command the driver does not have: NULL, on the driver's authority.
    let absent = f.cstr("vkCmdDrawMeshTasksNV");
    assert_eq!(f.call(get_proc, [device, absent, 0, 0]).expect("a lookup"), 0);

    // A null device: NULL, on the specification's authority. A different fact, and the census
    // keeps them apart.
    let any = f.cstr("vkQueueSubmit");
    assert_eq!(f.call(get_proc, [0, any, 0, 0]).expect("a lookup"), 0);

    let answers: Vec<(String, String)> = f
        .vulkan()
        .requests()
        .into_iter()
        .map(|r| (r.name, format!("{:?}", r.answer)))
        .collect();
    assert!(
        answers.iter().any(|(n, a)| n == "vkCmdDrawMeshTasksNV" && a.contains("host driver")),
        "the driver's NULL is recorded as the driver's: {answers:?}"
    );
    assert!(
        answers.iter().any(|(n, a)| n == "vkQueueSubmit" && a.contains("per specification")),
        "and the specification's NULL as the specification's: {answers:?}"
    );

    // The census says which call did each lookup.
    let report = f.vulkan().report();
    assert!(report.contains("vkGetDeviceProcAddr(device = "), "{report}");
    assert!(report.contains("vkGetInstanceProcAddr(instance = "), "{report}");

    // **And `entry_calls` still pairs with the instance-level rows alone**, which is the identity
    // `Vulkan::entry_calls` states and which stage 3 narrowed: a device lookup is a census row and
    // is not an entry into `vkGetInstanceProcAddr`.
    let requests = f.vulkan().requests();
    let instance_rows =
        requests.iter().filter(|r| r.via == omni_android::vulkan::ProcVia::Instance).count();
    let device_rows =
        requests.iter().filter(|r| r.via == omni_android::vulkan::ProcVia::Device).count();
    assert_eq!(f.vulkan().requests_dropped(), 0);
    assert_eq!(f.vulkan().entry_calls() as usize, instance_rows);
    assert_eq!(device_rows, 3, "two device lookups plus the null-device one");
    assert!(instance_rows > 0);

    // The host was asked about exactly the two non-null-device names, with the token it issued.
    assert_eq!(
        host.log().device_procs,
        vec![
            (HostDevice::from_token(0), "vkCreateSwapchainKHR".to_string()),
            (HostDevice::from_token(0), "vkCmdDrawMeshTasksNV".to_string()),
        ]
    );

    // A wild VkDevice refuses rather than reaching the driver.
    let text = f.refusal(get_proc, [device + 4, any, 0, 0]).to_string();
    assert!(text.contains("`VkDevice`"), "{text}");
    assert!(text.contains("NULL is not the answer either"), "{text}");

    // **And a device-level thunk now goes somewhere**, which is what stage 4 changed. The
    // swapchain thunk is a real handler, so calling it with a NULL `pCreateInfo` refuses for the
    // *argument* rather than for the function -- an assertion that is worth keeping precisely
    // because it used to say the opposite, and the difference between the two messages is the
    // whole of what this stage added.
    let text = f.refusal(swapchain, [device, 0, 0, 0]).to_string();
    assert!(text.contains("vkCreateSwapchainKHR"), "{text}");
    assert!(text.contains("pCreateInfo = NULL"), "the handler is real now: {text}");

    // What still refuses by *name* is the batch after this one. **The example had to change in
    // stage 5**: `vkAllocateMemory` is implemented now, so the frontier moved, and
    // `vkCreateComputePipelines` is on the far side of it. That the assertion had to be rewritten
    // is the point of keeping it — it is what makes "the frontier is named" a claim about where
    // the frontier actually is rather than a sentence that would pass whatever happened.
    let name = f.cstr("vkCreateComputePipelines");
    let compute = f.call(get_proc, [device, name, 0, 0]).expect("a lookup");
    assert_ne!(compute, 0, "the driver has vkCreateComputePipelines, so a thunk is handed out");
    let text = f.refusal(compute, [device, 0, 0, 0]).to_string();
    assert!(text.contains("vkCreateComputePipelines"), "{text}");
    assert!(text.contains("stage 5"), "the frontier is named: {text}");
    assert!(text.contains("compute pipelines"), "and what is on the far side of it: {text}");

    // And the batch this stage *did* implement is reachable through the same lookup, which is the
    // other half of the same claim: `vkAllocateMemory` refuses for its **argument** now, not for
    // its name.
    let memory = f.cstr("vkAllocateMemory");
    let allocate = f.call(get_proc, [device, memory, 0, 0]).expect("a lookup");
    assert_ne!(allocate, 0);
    let text = f.refusal(allocate, [device, 0, 0, 0]).to_string();
    assert!(text.contains("pAllocateInfo = NULL"), "the handler is real now: {text}");
}

// ============================================================================ the live tests

/// **A real `VkSurfaceKHR` over a real window, a real physical device chosen, and a real
/// `VkDevice` — all driven from translated ARM64 through guest thunks.**
///
/// This is the evidence stage 3 owes. Everything above it can be satisfied by a shim that marshals
/// correctly into a host that is not a driver; this one cannot, and the proof is that the strings
/// and numbers printed at the end were read **out of the guest's own memory** after a real NVIDIA
/// driver wrote them there.
#[test]
#[ignore = "opens a window and the host Vulkan driver; set OMNI_GFX_WINDOW_TESTS=1 and run with --ignored"]
fn a_real_surface_device_and_queue_are_chosen_through_the_guest_path() {
    require_gate();
    let _serial = serialized();

    // A real window, on the screen, whose handle the source publishes.
    let mut window = omni_platform::window::Window::new(&omni_platform::window::WindowDesc::new(
        "Omnidroid — Vulkan stage 3",
        1024,
        576,
    ))
    .unwrap_or_else(|err| panic!("could not create the window: {err}"));
    window.show();
    let _ = window.poll_events().count();
    let source = HostWindowSource::watching(&window).expect("a source watching the window");
    let client = source.geometry().expect("the window has a client area");

    let host = omni_gfx::GfxVulkanHost::load().expect(
        "this machine must have a Vulkan loader: the gate was set, so a missing driver is a \
         failure and not a skip",
    );
    let f = fixture("live-stage3", Some(host.clone()));
    f.ndk.set_window_source(Arc::clone(&source) as Arc<dyn WindowSource>);

    let entry_point = f.entry_point();
    let instance = f.an_instance(entry_point);

    // 1. The surface, through the call the host has no counterpart for.
    let create_surface = f.resolve(entry_point, instance, "vkCreateAndroidSurfaceKHR");
    let native_window = f.a_native_window();
    let info = f.android_surface_info(native_window);
    let out = f.alloc(8);
    let result = f.call(create_surface, [instance, info, 0, out]).expect("the call must complete");
    assert_eq!(result as i32, VK_SUCCESS, "the driver answered VkResult {}", result as i32);
    let surface = f.guest.read_u64(out as GuestAddr);
    assert_ne!(surface, 0);
    assert_eq!(f.vulkan().surface_handles().len(), 1);

    // **One scratch buffer, reused.** The guest arena is 64 KB and a real driver's device
    // extension list alone is around 40 KB of `VkExtensionProperties`; allocating a fresh buffer
    // per query fills it, which is how this test first failed. Each enumeration below finishes
    // with its bytes before the next begins.
    let scratch = f.alloc(SCRATCH_BYTES);

    // 2. The devices.
    let enumerate = f.resolve(entry_point, instance, "vkEnumeratePhysicalDevices");
    let count_at = f.alloc(4);
    assert_eq!(f.call(enumerate, [instance, count_at, 0, 0]).expect("count") as i32, VK_SUCCESS);
    let count = f.read_u32(count_at) as usize;
    assert!(count > 0, "a machine with a Vulkan loader and no physical device cannot render");
    let array_at = f.alloc(count * 8);
    assert_eq!(
        f.call(enumerate, [instance, count_at, array_at, 0]).expect("array") as i32,
        VK_SUCCESS
    );
    let devices: Vec<u64> =
        (0..count).map(|i| f.guest.read_u64(array_at as GuestAddr + i * 8)).collect();

    // 3. The queries a renderer makes to choose one, on every device it found.
    let properties = f.resolve(entry_point, instance, "vkGetPhysicalDeviceProperties");
    let families = f.resolve(entry_point, instance, "vkGetPhysicalDeviceQueueFamilyProperties");
    let support = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceSupportKHR");
    let memory = f.resolve(entry_point, instance, "vkGetPhysicalDeviceMemoryProperties");
    let features = f.resolve(entry_point, instance, "vkGetPhysicalDeviceFeatures");
    let capabilities =
        f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceCapabilitiesKHR");
    let formats = f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfaceFormatsKHR");
    let present_modes =
        f.resolve(entry_point, instance, "vkGetPhysicalDeviceSurfacePresentModesKHR");
    let device_extensions = f.resolve(entry_point, instance, "vkEnumerateDeviceExtensionProperties");

    let properties_at = f.alloc(PHYSICAL_DEVICE_PROPERTIES_BYTES);
    let mut found: Vec<(u64, String, u32)> = Vec::new();
    for &device in &devices {
        f.call(properties, [device, properties_at, 0, 0]).expect("properties");
        let bytes = f.read_bytes(properties_at, PHYSICAL_DEVICE_PROPERTIES_BYTES);
        let end = bytes[20..276].iter().position(|&b| b == 0).expect("a NUL-terminated deviceName");
        let name = String::from_utf8_lossy(&bytes[20..20 + end]).into_owned();
        let kind = u32::from_le_bytes(bytes[16..20].try_into().expect("four"));
        assert!(!name.is_empty(), "a driver always names its device");
        found.push((device, name, kind));
    }

    // Pick the first device with a queue family that can both render and present to this surface.
    let supported_at = f.alloc(4);
    let mut chosen: Option<(u64, String, u32, u32)> = None;
    for (device, name, _) in &found {
        f.call(families, [*device, count_at, 0, 0]).expect("family count");
        let family_count = f.read_u32(count_at) as usize;
        assert!(family_count * QUEUE_FAMILY_PROPERTIES_BYTES <= SCRATCH_BYTES);
        f.guest.write_u32(count_at as GuestAddr, family_count as u32);
        f.call(families, [*device, count_at, scratch, 0]).expect("families");
        let bytes = f.read_bytes(scratch, family_count * QUEUE_FAMILY_PROPERTIES_BYTES);
        for index in 0..family_count {
            let entry = &bytes[index * QUEUE_FAMILY_PROPERTIES_BYTES..];
            let flags = u32::from_le_bytes(entry[0..4].try_into().expect("four"));
            let queues = u32::from_le_bytes(entry[4..8].try_into().expect("four"));
            if flags & 0x1 == 0 || queues == 0 {
                continue; // not a VK_QUEUE_GRAPHICS_BIT family
            }
            let result = f
                .call(support, [*device, index as u64, surface, supported_at])
                .expect("surface support");
            assert_eq!(result as i32, VK_SUCCESS);
            if f.read_u32(supported_at) == 1 {
                chosen = Some((*device, name.clone(), index as u32, queues));
                break;
            }
        }
        if chosen.is_some() {
            break;
        }
    }
    let (device, device_name, family, family_queues) = chosen.expect(
        "this machine has a Vulkan driver and a window, so some queue family must be able to \
         render and present to a surface over that window",
    );

    // The rest of the queries, on the chosen device, so that all ten are exercised live.
    let features_at = f.alloc(PHYSICAL_DEVICE_FEATURES_BYTES);
    f.call(features, [device, features_at, 0, 0]).expect("features");
    let memory_at = f.alloc(PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    f.call(memory, [device, memory_at, 0, 0]).expect("memory properties");
    let memory_bytes = f.read_bytes(memory_at, PHYSICAL_DEVICE_MEMORY_PROPERTIES_BYTES);
    let memory_types = u32::from_le_bytes(memory_bytes[0..4].try_into().expect("four"));
    let memory_heaps = u32::from_le_bytes(memory_bytes[260..264].try_into().expect("four"));
    assert!(memory_types > 0 && memory_heaps > 0, "a device always has memory");

    let capabilities_at = f.alloc(SURFACE_CAPABILITIES_BYTES);
    assert_eq!(
        f.call(capabilities, [device, surface, capabilities_at, 0]).expect("capabilities") as i32,
        VK_SUCCESS
    );
    let caps = f.read_bytes(capabilities_at, SURFACE_CAPABILITIES_BYTES);
    let extent = (
        u32::from_le_bytes(caps[8..12].try_into().expect("four")),
        u32::from_le_bytes(caps[12..16].try_into().expect("four")),
    );
    let min_images = u32::from_le_bytes(caps[0..4].try_into().expect("four"));

    assert_eq!(
        f.call(formats, [device, surface, count_at, 0]).expect("format count") as i32,
        VK_SUCCESS
    );
    let format_count = f.read_u32(count_at) as usize;
    assert!(format_count > 0, "a presentable surface offers at least one format");
    assert!(format_count * SURFACE_FORMAT_BYTES <= SCRATCH_BYTES);
    f.guest.write_u32(count_at as GuestAddr, format_count as u32);
    assert_eq!(
        f.call(formats, [device, surface, count_at, scratch]).expect("formats") as i32,
        VK_SUCCESS
    );
    let format_bytes = f.read_bytes(scratch, format_count * SURFACE_FORMAT_BYTES);
    let first_format = u32::from_le_bytes(format_bytes[0..4].try_into().expect("four"));

    assert_eq!(
        f.call(present_modes, [device, surface, count_at, 0]).expect("mode count") as i32,
        VK_SUCCESS
    );
    let mode_count = f.read_u32(count_at) as usize;
    assert!(mode_count * 4 <= SCRATCH_BYTES);
    f.guest.write_u32(count_at as GuestAddr, mode_count as u32);
    assert_eq!(
        f.call(present_modes, [device, surface, count_at, scratch]).expect("modes") as i32,
        VK_SUCCESS
    );
    let mode_bytes = f.read_bytes(scratch, mode_count * 4);
    let modes: Vec<u32> = (0..mode_count)
        .map(|i| u32::from_le_bytes(mode_bytes[i * 4..i * 4 + 4].try_into().expect("four")))
        .collect();
    assert!(modes.contains(&2), "every implementation supports VK_PRESENT_MODE_FIFO_KHR (2)");

    assert_eq!(
        f.call(device_extensions, [device, 0, count_at, 0]).expect("extension count") as i32,
        VK_SUCCESS
    );
    let extension_count = f.read_u32(count_at) as usize;
    assert!(extension_count > 0, "a device that can present reports device extensions");

    // **The two-count idiom's truncating half, against a real driver.** This machine's NVIDIA
    // driver reports 263 device extensions, which is 68,380 bytes of `VkExtensionProperties` and
    // does not fit the 64 KB guest arena this harness gives the guest — so the guest asks for as
    // many as its buffer holds, exactly as an engine with a fixed-size array would, and the
    // answer must be `VK_INCOMPLETE` with the count of what fitted. That is not a workaround for
    // the arena: it is the only live exercise of the path where `available` exceeds `capacity`,
    // and it is the path that writes past the end of a guest buffer when it is wrong.
    let fits = SCRATCH_BYTES / omni_android::vulkan::EXTENSION_PROPERTIES_BYTES;
    let asking = fits.min(extension_count);
    f.guest.write_u32(count_at as GuestAddr, asking as u32);
    let result =
        f.call(device_extensions, [device, 0, count_at, scratch]).expect("extensions") as i32;
    let truncated = asking < extension_count;
    assert_eq!(
        result,
        if truncated { VK_INCOMPLETE } else { VK_SUCCESS },
        "asked for {asking} of {extension_count}"
    );
    assert_eq!(f.read_u32(count_at) as usize, asking, "the count written back is what fitted");
    let extension_bytes =
        f.read_bytes(scratch, asking * omni_android::vulkan::EXTENSION_PROPERTIES_BYTES);
    let device_extension_names: Vec<String> = (0..asking)
        .map(|i| {
            let entry = &extension_bytes[i * omni_android::vulkan::EXTENSION_PROPERTIES_BYTES..];
            let end = entry[..256].iter().position(|&b| b == 0).expect("a NUL");
            String::from_utf8_lossy(&entry[..end]).into_owned()
        })
        .collect();
    assert!(
        device_extension_names.iter().all(|name| name.starts_with("VK_")),
        "every entry that fitted is a real extension name, so the array was written whole: {:?}",
        &device_extension_names[..4.min(device_extension_names.len())]
    );

    // 4. The device, and a queue out of it.
    let create_device = f.resolve(entry_point, instance, "vkCreateDevice");
    let info = f.device_create_info(&[(family, &[1.0])], &["VK_KHR_swapchain"]);
    let device_out = f.alloc(8);
    let result = f.call(create_device, [device, info, 0, device_out]).expect("the call completes");
    assert_eq!(
        result as i32,
        VK_SUCCESS,
        "the driver answered VkResult {} -- and since the request enables `VK_KHR_swapchain`, a          success here is also the proof that this device has that extension, which the truncated          enumeration above could not establish on its own",
        result as i32
    );
    let logical = f.guest.read_u64(device_out as GuestAddr);
    assert_ne!(logical, 0);

    let get_queue = f.resolve(entry_point, instance, "vkGetDeviceQueue");
    let queue_at = f.alloc(8);
    f.call(get_queue, [logical, u64::from(family), 0, queue_at]).expect("queue");
    let queue = f.guest.read_u64(queue_at as GuestAddr);
    assert_ne!(queue, 0);
    f.call(get_queue, [logical, u64::from(family), 0, queue_at]).expect("queue again");
    assert_eq!(f.guest.read_u64(queue_at as GuestAddr), queue, "one (family, index), one VkQueue");

    // 5. And device-level resolution goes to the real driver, returning **guest** thunks.
    let get_proc = f.resolve(entry_point, instance, "vkGetDeviceProcAddr");
    let name = f.cstr("vkCreateSwapchainKHR");
    let swapchain = f.call(get_proc, [logical, name, 0, 0]).expect("a lookup");
    assert_ne!(swapchain, 0, "a device with VK_KHR_swapchain enabled has vkCreateSwapchainKHR");
    assert!(
        f.boundary.symbol_at(swapchain as GuestAddr).is_some(),
        "and what the guest got is a thunk in this boundary, not the driver's function pointer"
    );
    let absent = f.cstr("vkThisIsNotAVulkanCommand");
    assert_eq!(f.call(get_proc, [logical, absent, 0, 0]).expect("a lookup"), 0, "the driver's NULL");

    // The substitution is recorded, in both directions and for the surface call.
    let rewrites = f.vulkan().rewrites();
    assert!(
        rewrites.iter().any(|r| matches!(r.site, RewriteSite::SurfaceCall { system: "win32" })),
        "the surface substitution must be recorded: {rewrites:?}"
    );

    eprintln!("\n=== stage 3 live evidence ===");
    eprintln!("host: {host:?}");
    let (surfaces, logical_devices, queues) = host.objects();
    eprintln!(
        "the host holds {surfaces} surface(s), {logical_devices} device(s), {queues} queue(s)"
    );
    eprintln!("the window's client area is {}x{}", client.width, client.height);
    eprintln!(
        "vkCreateAndroidSurfaceKHR -> VkResult 0; the guest's VkSurfaceKHR handle is {surface:#x}"
    );
    eprintln!("vkEnumeratePhysicalDevices reported {count} device(s):");
    for (handle, name, kind) in &found {
        eprintln!("    {handle:#x} -> \"{name}\" (VkPhysicalDeviceType {kind})");
    }
    eprintln!("chosen: \"{device_name}\", queue family {family} of {family_queues} queue(s)");
    eprintln!("    memory: {memory_types} type(s) across {memory_heaps} heap(s)");
    eprintln!(
        "    surface: currentExtent {}x{}, minImageCount {min_images}",
        extent.0, extent.1
    );
    eprintln!("    {format_count} surface format(s), first VkFormat {first_format}");
    eprintln!("    present modes {modes:?}");
    eprintln!(
        "    {extension_count} device extension(s); the guest asked for {asking} and got VkResult {result}{note}",
        result = if truncated { VK_INCOMPLETE } else { VK_SUCCESS },
        note = if truncated { " -- VK_INCOMPLETE, as it must be" } else { "" }
    );
    eprintln!(
        "    VK_KHR_swapchain: enabled at vkCreateDevice, which succeeded -- {}",
        if device_extension_names.iter().any(|n| n == "VK_KHR_swapchain") {
            "and it is in the part of the list that fitted"
        } else {
            "it is past the part of the list that fitted"
        }
    );
    eprintln!("vkCreateDevice -> VkResult 0; the guest's VkDevice handle is {logical:#x}");
    eprintln!("vkGetDeviceQueue -> the guest's VkQueue handle is {queue:#x}");
    eprintln!("vkGetDeviceProcAddr(\"vkCreateSwapchainKHR\") -> guest thunk {swapchain:#x}");
    eprintln!("\n{}", f.vulkan().report());
}
