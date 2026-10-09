//! `VK_ANDROID_native_buffer`: the images the Android loader's swapchain makes on gralloc buffers
//! (D2), whose pixels must end up in the buffer's `shm` region for the buffer's consumer
//! (SurfaceFlinger, the composer, a screenshot) to see them.
//!
//! Such an image is an ordinary image of the host's GPU. When the loader releases it to the window
//! (`vkQueueSignalReleaseImageANDROID`, the image then in `PRESENT_SRC_KHR`), it is copied through a
//! staging buffer into the region, row for row at the buffer's stride, and the region's content
//! generation is bumped (D3a design, "Gralloc buffers on the host GPU").
//!
//! **The release does not wait for the GPU.** The copy into the staging buffer is submitted on the
//! app's thread; waiting for it (~3.1 ms a frame of wall time in a Roblox world, on the engine's
//! render thread -- a wait that sleeps, measured: see [`RELEASE_WAIT_US`]) and the copy into the
//! region are a worker's. The region's
//! metadata page says a copy is on its way ([`PENDING_GENERATION_AT`]: the generation it will have),
//! and whoever reads the pixels -- the composer -- waits for that ([`wait_written`]) the way a
//! consumer waits on a release fence. An image's next release waits for its previous copy (its
//! staging buffer and command buffer are reused), so do its destruction and a device's.
use std::collections::HashMap;
use std::ffi::CStr;
use std::sync::Arc;

use ash::vk::{self, Handle};

use super::generated as g;
use super::{entry_point, CallError, Gpu, Table};
use crate::fd::FileKind;
use crate::process::Process;
use crate::shm::Shm;

type R<T> = Result<T, CallError>;

macro_rules! vkfn {
    ($t:expr, $id:ident, $name:literal, $ty:ty) => {{
        const NAMES: &[&CStr] = &[$name];
        // SAFETY: `$ty` is the command's Vulkan signature (ash's PFN type for it). (A call site
        // may already be in an `unsafe` block.)
        #[allow(unused_unsafe)]
        let f: $ty = unsafe { entry_point::<$ty>(&$t, g::$id, NAMES)? };
        f
    }};
}

/// The gralloc handle's ints (`hal::gralloc`): magic, then ... the stride at 8 and the pixels'
/// offset at 13.
const HANDLE_MAGIC: u32 = 0x4247_4d4f;
const HANDLE_INTS: u32 = 14;
/// Where a region's content generation is (`hal::gralloc`, `mapper.c`).
pub(crate) const CONTENT_GENERATION_AT: u64 = 4088;
/// The generation the region will have once the copy on its way lands (0 or not above the content
/// generation: nothing on its way).
pub(crate) const PENDING_GENERATION_AT: u64 = 4080;

/// What the host keeps of a device for the Android extensions: its physical device's memory
/// types, the queues it handed out, and a copier per queue family.
pub(crate) struct DeviceInfo {
    pub memory_types: Vec<vk::MemoryType>,
    /// Each queue the guest got, and its family.
    pub queues: HashMap<u64, u32>,
    copiers: HashMap<u32, Copier>,
    /// The command pool of each family's copier, which the images' own come from.
    pools: HashMap<u32, vk::CommandPool>,
    /// The host device was made with `VK_EXT_external_memory_host` (`gralloc_direct`): a gralloc
    /// region's view can be imported as its images' copy target.
    pub host_import: bool,
    /// The host device was made able to export share images (`present_zero`, `super::share`):
    /// its GPU's device and driver UUIDs.
    pub share: Option<([u8; 16], [u8; 16])>,
}

impl DeviceInfo {
    pub(crate) fn new(memory: &vk::PhysicalDeviceMemoryProperties) -> Self {
        Self { memory_types: memory.memory_types[..memory.memory_type_count as usize].to_vec(), queues: HashMap::new(), copiers: HashMap::new(), pools: HashMap::new(), host_import: false, share: None }
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> R<u32> {
        (0..self.memory_types.len() as u32)
            .find(|&i| bits & (1 << i) != 0 && self.memory_types[i as usize].property_flags.contains(want))
            .ok_or(CallError::Missing("a memory type"))
    }
}

/// **The release's copy straight into the gralloc region** (`gralloc_direct=0|1` in the lever file,
/// read live; **off by default**). The copy went GPU image -> host-cached staging buffer, then the
/// worker copied the staging buffer into the region on the CPU (5.6 MB a frame at 1575x890, after
/// waiting for the GPU). With this on, the region's own host view is the copy's destination -- the
/// view imported as Vulkan memory (`VK_EXT_external_memory_host`, the region's pixels page-aligned
/// at `PIXELS_AT` 4096) -- and the worker only waits for the fence and bumps the generation. Same
/// bytes in the same place at the same moment; one CPU copy a frame fewer in the app's process.
///
/// The host device needs the extension from its creation: `OMNI_GRALLOC_DIRECT=ready` asks for it
/// (the path still off, for an A/B inside one session: `gralloc_direct=0|1`), `=1` asks for it and
/// turns the path on. Without either, devices are made as before and the lever finds nothing to
/// switch to. Probed: `gpu::window_present::tests::a_gralloc_region_imports_as_gpu_memory`.
pub static DIRECT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether devices are made able to import gralloc regions (`OMNI_GRALLOC_DIRECT=ready|1`, or the
/// lever already on); `OMNI_GRALLOC_DIRECT=1` also turns [`DIRECT`] on.
pub(crate) fn direct_wanted() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let env = *ENV.get_or_init(|| match std::env::var("OMNI_GRALLOC_DIRECT").as_deref() {
        Ok("1") => {
            DIRECT.store(true, std::sync::atomic::Ordering::Relaxed);
            true
        }
        Ok("ready") => true,
        _ => false,
    });
    env || DIRECT.load(std::sync::atomic::Ordering::Relaxed)
}

/// A gralloc region's view imported as a buffer of the device ([`DIRECT`]).
struct Direct {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

/// Import `bytes` of `shm`'s view at `pixels_at` as a transfer-destination buffer of device `d`;
/// the view stays pinned until the buffer is freed (`destroy_image`).
fn import_region(t: &Arc<Table>, d: vk::Device, memory_types: &[vk::MemoryType], shm: &Shm, pixels_at: u64, bytes: usize) -> R<Direct> {
    let size = bytes.div_ceil(4096) * 4096;
    let ptr = shm.pin_view(pixels_at, bytes).ok_or(CallError::Missing("a pinned view"))?;
    let made = (|| -> R<Direct> {
        let props_fn = t.lookup(c"vkGetMemoryHostPointerPropertiesEXT").ok_or(CallError::Missing("vkGetMemoryHostPointerPropertiesEXT"))?;
        // SAFETY: the entry point's Vulkan signature.
        let props_fn: vk::PFN_vkGetMemoryHostPointerPropertiesEXT = unsafe { std::mem::transmute(props_fn) };
        let mut props = vk::MemoryHostPointerPropertiesEXT::default();
        // SAFETY: `ptr` is the pinned view, page-aligned (the view is, and `pixels_at` is 4096).
        check(unsafe { props_fn(d, vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT, ptr.cast(), &mut props) })?;
        let mut external = vk::ExternalMemoryBufferCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
        let bci = vk::BufferCreateInfo::default().size(bytes as u64).usage(vk::BufferUsageFlags::TRANSFER_DST).push_next(&mut external);
        let mut buffer = vk::Buffer::null();
        check(unsafe { vkfn!(t, ID_VK_CREATE_BUFFER, c"vkCreateBuffer", vk::PFN_vkCreateBuffer)(d, &bci, std::ptr::null(), &mut buffer) })?;
        let mut req = vk::MemoryRequirements::default();
        unsafe { vkfn!(t, ID_VK_GET_BUFFER_MEMORY_REQUIREMENTS, c"vkGetBufferMemoryRequirements", vk::PFN_vkGetBufferMemoryRequirements)(d, buffer, &mut req) };
        let bits = props.memory_type_bits & req.memory_type_bits;
        let coherent = (0..memory_types.len() as u32).find(|&i| bits & (1 << i) != 0 && memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::HOST_COHERENT));
        let destroy = vkfn!(t, ID_VK_DESTROY_BUFFER, c"vkDestroyBuffer", vk::PFN_vkDestroyBuffer);
        let free = vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory);
        // SAFETY: the buffer made above, used by nothing yet.
        let destroy_buffer = || unsafe { destroy(d, buffer, std::ptr::null()) };
        let Some(kind) = coherent else {
            destroy_buffer();
            return Err(CallError::Missing("a coherent memory type for the region"));
        };
        let mut import = vk::ImportMemoryHostPointerInfoEXT::default().handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT).host_pointer(ptr.cast());
        let ai = vk::MemoryAllocateInfo::default().allocation_size(size.max(req.size as usize) as u64).memory_type_index(kind).push_next(&mut import);
        let mut memory = vk::DeviceMemory::null();
        if let Err(e) = check(unsafe { vkfn!(t, ID_VK_ALLOCATE_MEMORY, c"vkAllocateMemory", vk::PFN_vkAllocateMemory)(d, &ai, std::ptr::null(), &mut memory) }) {
            destroy_buffer();
            return Err(e);
        }
        if let Err(e) = check(unsafe { vkfn!(t, ID_VK_BIND_BUFFER_MEMORY, c"vkBindBufferMemory", vk::PFN_vkBindBufferMemory)(d, buffer, memory, 0) }) {
            destroy_buffer();
            // SAFETY: the memory imported above, bound to nothing.
            unsafe { free(d, memory, std::ptr::null()) };
            return Err(e);
        }
        Ok(Direct { buffer, memory })
    })();
    if made.is_err() {
        shm.unpin_view();
    }
    made
}

/// A gralloc image's share image (`present_zero`, [`super::share`]): device-local, its memory
/// exported as a named Win32 handle, which is held open for as long as the image is.
struct Share {
    image: vk::Image,
    memory: vk::DeviceMemory,
    /// The named handle (`vkGetMemoryWin32HandleKHR`), closed at `destroy_image`.
    handle: usize,
}

/// Make the share image of a `width` x `height` gralloc image of `format` on device `d`, export it
/// by a new name, and describe it in `shm`'s metadata page.
#[allow(clippy::too_many_arguments)]
fn export_share(t: &Arc<Table>, d: vk::Device, memory_types: &[vk::MemoryType], uuids: ([u8; 16], [u8; 16]), shm: &Shm, format: vk::Format, width: u32, height: u32) -> R<Share> {
    let format = super::share::share_format(format).ok_or(CallError::Missing("a shareable format"))?;
    let name = super::share::next_name();
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
    let ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(format)
        .extent(vk::Extent3D { width, height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(super::share::USAGE)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);
    let mut image = vk::Image::null();
    check(unsafe { vkfn!(t, ID_VK_CREATE_IMAGE, c"vkCreateImage", vk::PFN_vkCreateImage)(d, &ci, std::ptr::null(), &mut image) })?;
    let destroy_image = vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage);
    let free = vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory);
    let made = (|| -> R<(vk::DeviceMemory, usize, u64)> {
        let mut req = vk::MemoryRequirements::default();
        unsafe { vkfn!(t, ID_VK_GET_IMAGE_MEMORY_REQUIREMENTS, c"vkGetImageMemoryRequirements", vk::PFN_vkGetImageMemoryRequirements)(d, image, &mut req) };
        let kind = (0..memory_types.len() as u32).find(|&i| req.memory_type_bits & (1 << i) != 0 && memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)).ok_or(CallError::Missing("device-local memory"))?;
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
        // GENERIC_ALL: the system host process opens it by name for its own device.
        let mut named = vk::ExportMemoryWin32HandleInfoKHR::default().dw_access(0x1000_0000).name(wide.as_ptr());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let ai = vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(kind).push_next(&mut export).push_next(&mut named).push_next(&mut dedicated);
        let mut memory = vk::DeviceMemory::null();
        check(unsafe { vkfn!(t, ID_VK_ALLOCATE_MEMORY, c"vkAllocateMemory", vk::PFN_vkAllocateMemory)(d, &ai, std::ptr::null(), &mut memory) })?;
        let bound = (|| -> R<usize> {
            check(unsafe { vkfn!(t, ID_VK_BIND_IMAGE_MEMORY, c"vkBindImageMemory", vk::PFN_vkBindImageMemory)(d, image, memory, 0) })?;
            let get = t.lookup(c"vkGetMemoryWin32HandleKHR").ok_or(CallError::Missing("vkGetMemoryWin32HandleKHR"))?;
            // SAFETY: the entry point's Vulkan signature.
            let get: vk::PFN_vkGetMemoryWin32HandleKHR = unsafe { std::mem::transmute(get) };
            let info = vk::MemoryGetWin32HandleInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
            let mut handle: vk::HANDLE = 0;
            // SAFETY: exportable memory of this device; the handle is ours to close.
            check(unsafe { get(d, &info, &mut handle) })?;
            Ok(handle as usize)
        })();
        match bound {
            Ok(handle) => Ok((memory, handle, req.size)),
            Err(e) => {
                unsafe { free(d, memory, std::ptr::null()) };
                Err(e)
            }
        }
    })();
    match made {
        Ok((memory, handle, size)) => {
            let desc = super::share::ShareDesc { format, width, height, size, device_uuid: uuids.0, driver_uuid: uuids.1, name };
            desc.write(shm);
            super::share::write_generation(shm, 0);
            Ok(Share { image, memory, handle })
        }
        Err(e) => {
            unsafe { destroy_image(d, image, std::ptr::null()) };
            Err(e)
        }
    }
}

/// Close a share image's named handle (the name goes with the last handle to it).
fn close_handle(handle: usize) {
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle as _;
        // SAFETY: a handle `vkGetMemoryWin32HandleKHR` gave this process, closed once.
        drop(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle as *mut std::ffi::c_void) });
    }
    #[cfg(not(windows))]
    let _ = handle;
}

/// A command buffer and fence for the host's own copies on one queue family.
struct Copier {
    cb: vk::CommandBuffer,
    fence: vk::Fence,
}

/// An image on a gralloc buffer, and the staging buffer its pixels leave through.
pub(crate) struct NativeImage {
    device: u64,
    memory: vk::DeviceMemory,
    staging: vk::Buffer,
    staging_memory: vk::DeviceMemory,
    mapped: usize,
    /// The staging memory is not host-coherent: invalidated before the CPU reads it.
    invalidate: bool,
    bytes: usize,
    width: u32,
    height: u32,
    stride: u32,
    shm: Arc<Shm>,
    pixels_at: u64,
    /// Its own copy command buffer and fence, made at its first release.
    copier: Option<Copier>,
    /// A copy of it is on its way (the worker's).
    in_flight: Arc<InFlight>,
    /// Its region's view as a buffer of the device, for [`DIRECT`] (when the device can import).
    direct: Option<Direct>,
    /// Its share image, for `present_zero` (when the device can export).
    share: Option<Share>,
}

/// Whether an image's copy is on its way, and a wait for it to land.
#[derive(Default)]
struct InFlight {
    busy: parking_lot::Mutex<bool>,
    done: parking_lot::Condvar,
}

impl InFlight {
    fn wait(&self) {
        let mut busy = self.busy.lock();
        while *busy {
            self.done.wait(&mut busy);
        }
    }
}

/// A copy the worker lands: the fence the GPU's copy signals, then the staging bytes into the region.
struct Landing {
    table: Arc<Table>,
    device: u64,
    fence: vk::Fence,
    invalidate: bool,
    staging_memory: vk::DeviceMemory,
    mapped: usize,
    bytes: usize,
    shm: Arc<Shm>,
    pixels_at: u64,
    in_flight: Arc<InFlight>,
    /// The copy went straight into the region ([`DIRECT`]): nothing to copy once it lands.
    direct: bool,
}

// SAFETY: the Vulkan handles are plain handles; the staging mapping (`mapped`) is the image's, read
// only by the worker while `in_flight` is set, and nothing else touches it meanwhile.
unsafe impl Send for Landing {}

/// **How the host waits for a fence it submitted itself** (`release_wait=spin|poll`,
/// `release_poll=<us>` in the lever file): 0 is the driver's own wait (`spin`, the default), else
/// the fence asked every that many microseconds with a sleep between (`poll`). The release worker's
/// wait for its copy, and a sync-file export's wait. The driver's wait was taken to spin on NVIDIA;
/// MEASURED (`wait_tests::release_wait_cost`, RTX 4060, driver 591.86, 4.8 ms of GPU work a wait,
/// the waiting thread's cycles): the driver's wait 0.078 ms of CPU, a timeline semaphore's
/// `vkWaitSemaphores` 0.085 ms and no later; polling every 50/100/250/500 us 0.20/0.20/0.19/0.15 ms
/// and +0.28/+0.31/+0.36/+0.69 ms of latency. An exported fence's Win32 handle
/// (`VK_KHR_external_fence_win32`, opaque) is no event: `WaitForSingleObject` on it returns at
/// once, the fence unsignalled. So `spin` stays the default and there is no `event`; `poll` is for
/// an in-world A/B, where the `[gpu-stats]` line now gives the wait's thread CPU beside its wall time.
pub static RELEASE_WAIT_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Wait for `fence` of device `d`: [`RELEASE_WAIT_US`]'s way (or `fence_poll`'s, when that is set
/// and this is not).
pub(crate) fn wait_fence(t: &Table, d: vk::Device, fence: vk::Fence) -> R<()> {
    let wait = vkfn!(t, ID_VK_WAIT_FOR_FENCES, c"vkWaitForFences", vk::PFN_vkWaitForFences);
    let poll = match RELEASE_WAIT_US.load(std::sync::atomic::Ordering::Relaxed) {
        0 => super::FENCE_POLL_US.load(std::sync::atomic::Ordering::Relaxed),
        us => us,
    };
    if poll > 0 {
        // SAFETY: a live fence of a live device; a zero timeout only asks.
        while unsafe { wait(d, 1, &fence, vk::TRUE, 0) } == vk::Result::TIMEOUT {
            std::thread::sleep(std::time::Duration::from_micros(u64::from(poll)));
        }
    }
    // SAFETY: as above.
    check(unsafe { wait(d, 1, &fence, vk::TRUE, u64::MAX) })
}

thread_local! {
    /// This host thread, for the release wait's CPU in `[gpu-stats]`.
    static THIS_THREAD: Option<omni_platform::sampler::HostThread> = omni_platform::sampler::HostThread::current().ok();
}

impl Landing {
    fn land(self) {
        let t = &self.table;
        let d = vk::Device::from_raw(self.device);
        let waited = std::time::Instant::now();
        let stats = super::stats::enabled();
        let before = if stats { THIS_THREAD.with(|me| me.as_ref().and_then(|me| Some((me.cpu_time().ok()?, me.cycles().ok()?)))) } else { None };
        let ok = (|| -> R<()> {
            wait_fence(t, d, self.fence)?;
            if stats {
                super::stats::add(super::special::ID_GRALLOC_USAGE + 6, waited.elapsed());
                let after = THIS_THREAD.with(|me| me.as_ref().and_then(|me| Some((me.cpu_time().ok()?, me.cycles().ok()?))));
                if let (Some((c0, k0)), Some((c1, k1))) = (before, after) {
                    super::stats::add_release_wait_cpu(c1.saturating_sub(c0), k1.saturating_sub(k0));
                }
            }
            if self.invalidate && !self.direct {
                let range = vk::MappedMemoryRange { memory: self.staging_memory, offset: 0, size: vk::WHOLE_SIZE, ..Default::default() };
                check(unsafe { vkfn!(t, ID_VK_INVALIDATE_MAPPED_MEMORY_RANGES, c"vkInvalidateMappedMemoryRanges", vk::PFN_vkInvalidateMappedMemoryRanges)(d, 1, &range) })?;
            }
            Ok(())
        })();
        let written = std::time::Instant::now();
        if ok.is_ok() && !self.direct {
            // SAFETY: `mapped` is the staging memory's host mapping, `bytes` long, written by the copy
            // the fence has just seen finish.
            let pixels = unsafe { std::slice::from_raw_parts(self.mapped as *const u8, self.bytes) };
            let _ = self.shm.write_at(pixels, self.pixels_at);
        }
        // Landed (or given up on): the generation it was promised, so no reader waits for ever.
        bump_generation(&self.shm);
        if super::stats::enabled() {
            super::stats::add(super::special::ID_GRALLOC_USAGE + 7, written.elapsed());
        }
        *self.in_flight.busy.lock() = false;
        self.in_flight.done.notify_all();
    }
}

/// The worker that lands the copies, in the order they were released.
fn landings() -> &'static parking_lot::Mutex<std::sync::mpsc::Sender<Landing>> {
    static WORKER: std::sync::OnceLock<parking_lot::Mutex<std::sync::mpsc::Sender<Landing>>> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Landing>();
        let _ = std::thread::Builder::new().name("omni-gpu-release".into()).spawn(move || {
            for landing in rx {
                landing.land();
            }
        });
        parking_lot::Mutex::new(tx)
    })
}

/// Wait (at most `limit`) until the copy on its way into `shm`, if any, has landed: what a reader of
/// a released buffer's pixels waits on, as a consumer waits on a release fence.
pub(crate) fn wait_written(shm: &Shm, limit: std::time::Duration) {
    let word = |at: u64| {
        let mut g = [0u8; 8];
        let _ = shm.read_at(&mut g, at);
        u64::from_le_bytes(g)
    };
    let started = std::time::Instant::now();
    while word(PENDING_GENERATION_AT) > word(CONTENT_GENERATION_AT) && started.elapsed() < limit {
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

impl NativeImage {
    pub(crate) fn belongs_to(&self, device: u64) -> bool {
        self.device == device
    }
}

/// Bytes per pixel of the formats gralloc buffers have.
fn bytes_per_pixel(format: vk::Format) -> Option<u32> {
    match format {
        vk::Format::R8G8B8A8_UNORM | vk::Format::R8G8B8A8_SRGB | vk::Format::B8G8R8A8_UNORM | vk::Format::B8G8R8A8_SRGB | vk::Format::A2B10G10R10_UNORM_PACK32 => Some(4),
        vk::Format::R5G6B5_UNORM_PACK16 => Some(2),
        vk::Format::R16G16B16A16_SFLOAT => Some(8),
        vk::Format::R8_UNORM => Some(1),
        _ => None,
    }
}

fn rd_u32(p: &Process, at: u64) -> R<u32> {
    Ok(u32::from_le_bytes(p.mem.read(at, 4).map_err(|_| CallError::Args)?.try_into().expect("4")))
}

/// The gralloc buffer a guest `native_handle_t*` names: its region, stride and pixel offset.
pub(crate) fn gralloc_buffer(p: &Process, handle: u64) -> R<(Arc<Shm>, u32, u64)> {
    let (num_fds, num_ints) = (rd_u32(p, handle + 4)?, rd_u32(p, handle + 8)?);
    if num_fds != 1 || num_ints != HANDLE_INTS {
        return Err(CallError::Args);
    }
    let int = |i: u64| rd_u32(p, handle + 16 + i * 4);
    if int(0)? != HANDLE_MAGIC {
        return Err(CallError::Args);
    }
    let fd = rd_u32(p, handle + 12)? as i32;
    let file = p.fds.get(fd).map_err(|_| CallError::Args)?;
    let shm = match &*file.kind.lock() {
        FileKind::Shared(m) => Arc::clone(m),
        _ => return Err(CallError::Args),
    };
    shm.as_graphics_buffer();
    Ok((shm, int(8)?, u64::from(int(13)?)))
}

/// Bump a region's content generation: its pixels changed.
pub(crate) fn bump_generation(shm: &Shm) {
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, CONTENT_GENERATION_AT);
    let _ = shm.write_at(&(u64::from_le_bytes(g).wrapping_add(1)).to_le_bytes(), CONTENT_GENERATION_AT);
}


fn check(r: vk::Result) -> R<()> {
    if r == vk::Result::SUCCESS { Ok(()) } else { Err(CallError::Host(r.as_raw())) }
}

/// `vkCreateImage` with a `VkNativeBufferANDROID` (at `native`, already out of the guest's chain):
/// the image, its memory, and its staging buffer. The `VkImageCreateInfo` (88 bytes) is `info`,
/// its chain what the host may see.
pub(crate) fn create_image(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, info: &[u8], native: u64) -> R<u64> {
    let (image, format, width, height) = create_for_swapchain(device, t, info)?;
    match attach(gpu, p, device, t, image, format, width, height, native) {
        Ok(n) => {
            gpu.native.lock().insert(image.as_raw(), n);
            Ok(image.as_raw())
        }
        Err(e) => {
            unsafe { vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage)(vk::Device::from_raw(device), image, std::ptr::null()) };
            Err(e)
        }
    }
}

/// The image of a swapchain (its `VkImageSwapchainCreateInfoKHR` names the loader's swapchain, out
/// of the chain): made now, bound to its gralloc buffer when the loader binds it
/// (`vkBindImageMemory2` with a `VkNativeBufferANDROID`, spec version 8).
pub(crate) fn create_swapchain_image(gpu: &Gpu, device: u64, t: &Arc<Table>, info: &[u8]) -> R<u64> {
    let (image, format, width, height) = create_for_swapchain(device, t, info)?;
    gpu.swapchain_images.lock().insert(image.as_raw(), (format, width, height));
    Ok(image.as_raw())
}

/// `vkBindImageMemory2`'s `VkBindImageMemoryInfo` of a swapchain image with a
/// `VkNativeBufferANDROID` at `native`: the image gets its memory and staging buffer.
pub(crate) fn bind_swapchain_image(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, image: u64, native: u64) -> R<()> {
    let (format, width, height) = gpu.swapchain_images.lock().remove(&image).ok_or(CallError::Args)?;
    let n = attach(gpu, p, device, t, vk::Image::from_raw(image), format, width, height, native)?;
    gpu.native.lock().insert(image, n);
    Ok(())
}

/// An image to hold a gralloc buffer's pixels: as the guest described it, and a copy source.
fn create_for_swapchain(device: u64, t: &Arc<Table>, info: &[u8]) -> R<(vk::Image, vk::Format, u32, u32)> {
    // SAFETY: `info` is a whole VkImageCreateInfo (the caller read 88 bytes of one).
    let mut ci: vk::ImageCreateInfo<'static> = unsafe { std::ptr::read_unaligned(info.as_ptr().cast()) };
    ci.usage |= vk::ImageUsageFlags::TRANSFER_SRC;
    bytes_per_pixel(ci.format).ok_or(CallError::Args)?;
    let mut image = vk::Image::null();
    check(unsafe { vkfn!(t, ID_VK_CREATE_IMAGE, c"vkCreateImage", vk::PFN_vkCreateImage)(vk::Device::from_raw(device), &ci, std::ptr::null(), &mut image) })?;
    Ok((image, ci.format, ci.extent.width, ci.extent.height))
}

/// Give `image` memory of its own and a staging buffer its pixels leave through into the gralloc
/// buffer a `VkNativeBufferANDROID` (at `native`) names.
#[allow(clippy::too_many_arguments)]
fn attach(gpu: &Gpu, p: &Process, device: u64, t: &Arc<Table>, image: vk::Image, format: vk::Format, width: u32, height: u32, native: u64) -> R<NativeImage> {
    let handle = p.mem.read_u64(native + 16).map_err(|_| CallError::Args)?;
    let (shm, stride, pixels_at) = gralloc_buffer(p, handle)?;
    let bpp = bytes_per_pixel(format).ok_or(CallError::Args)?;
    if stride < width || u64::from(stride) * u64::from(height) * u64::from(bpp) > shm.len().saturating_sub(pixels_at) {
        return Err(CallError::Args);
    }
    let d = vk::Device::from_raw(device);
    {
        let mut req = vk::MemoryRequirements::default();
        unsafe { vkfn!(t, ID_VK_GET_IMAGE_MEMORY_REQUIREMENTS, c"vkGetImageMemoryRequirements", vk::PFN_vkGetImageMemoryRequirements)(d, image, &mut req) };
        let devices = gpu.devices.lock();
        let info = devices.get(&device).ok_or(CallError::Handle(device))?;
        let memory = allocate(t, d, req.size, info.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|_| info.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::empty()))?)?;
        check(unsafe { vkfn!(t, ID_VK_BIND_IMAGE_MEMORY, c"vkBindImageMemory", vk::PFN_vkBindImageMemory)(d, image, memory, 0) })?;
        let bytes = stride as usize * height as usize * bpp as usize;
        let bci = vk::BufferCreateInfo { size: bytes as u64, usage: vk::BufferUsageFlags::TRANSFER_DST, ..Default::default() };
        let mut staging = vk::Buffer::null();
        check(unsafe { vkfn!(t, ID_VK_CREATE_BUFFER, c"vkCreateBuffer", vk::PFN_vkCreateBuffer)(d, &bci, std::ptr::null(), &mut staging) })?;
        unsafe { vkfn!(t, ID_VK_GET_BUFFER_MEMORY_REQUIREMENTS, c"vkGetBufferMemoryRequirements", vk::PFN_vkGetBufferMemoryRequirements)(d, staging, &mut req) };
        // The CPU reads every frame out of it: host-cached memory. Uncached (write-combined)
        // host-visible memory -- what `HOST_VISIBLE | HOST_COHERENT` alone finds first on NVIDIA --
        // reads at a fraction of memory speed: MEASURED (Windows, RTX 4060, 2026-09-28), a 1280x720
        // frame took 13-15 ms to copy out of it into a mapped region, against ~1 ms for the copy.
        let (visible, coherent, cached) = (vk::MemoryPropertyFlags::HOST_VISIBLE, vk::MemoryPropertyFlags::HOST_COHERENT, vk::MemoryPropertyFlags::HOST_CACHED);
        let (staging_type, invalidate) = match info.memory_type(req.memory_type_bits, visible | coherent | cached) {
            Ok(i) => (i, false),
            Err(_) => match info.memory_type(req.memory_type_bits, visible | cached) {
                Ok(i) => (i, true),
                Err(_) => (info.memory_type(req.memory_type_bits, visible | coherent)?, false),
            },
        };
        let staging_memory = allocate(t, d, req.size, staging_type)?;
        let (host_import, memory_types, share_ids) = (info.host_import, info.memory_types.clone(), info.share);
        drop(devices);
        check(unsafe { vkfn!(t, ID_VK_BIND_BUFFER_MEMORY, c"vkBindBufferMemory", vk::PFN_vkBindBufferMemory)(d, staging, staging_memory, 0) })?;
        let mut mapped = std::ptr::null_mut();
        check(unsafe { vkfn!(t, ID_VK_MAP_MEMORY, c"vkMapMemory", vk::PFN_vkMapMemory)(d, staging_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty(), &mut mapped) })?;
        // `gralloc_direct`: the region's view as a second copy target (the staging buffer stays, for
        // the lever off and for a region that cannot be imported).
        let direct = if host_import && pixels_at % 4096 == 0 {
            match import_region(t, d, &memory_types, &shm, pixels_at, bytes) {
                Ok(direct) => Some(direct),
                Err(e) => {
                    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        eprintln!("[gpu] gralloc_direct: a region could not be imported ({e:?}); its copies go through the staging buffer");
                    }
                    None
                }
            }
        } else {
            None
        };
        // `present_zero`: its share image, exported by name and described in the region.
        let share = share_ids.and_then(|ids| match export_share(t, d, &memory_types, ids, &shm, format, width, height) {
            Ok(share) => Some(share),
            Err(e) => {
                static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("[gpu] present_zero: no share image for a {width}x{height} buffer of VkFormat {} ({e:?}); it is composed on the CPU", format.as_raw());
                }
                None
            }
        });
        Ok(NativeImage { device, memory, staging, staging_memory, mapped: mapped as usize, invalidate, bytes, width, height, stride, shm, pixels_at, copier: None, in_flight: Arc::default(), direct, share })
    }
}

fn allocate(t: &Arc<Table>, d: vk::Device, size: u64, memory_type_index: u32) -> R<vk::DeviceMemory> {
    let ai = vk::MemoryAllocateInfo { allocation_size: size, memory_type_index, ..Default::default() };
    let mut m = vk::DeviceMemory::null();
    check(unsafe { vkfn!(t, ID_VK_ALLOCATE_MEMORY, c"vkAllocateMemory", vk::PFN_vkAllocateMemory)(d, &ai, std::ptr::null(), &mut m) })?;
    Ok(m)
}

/// `vkDestroyImage` of a native image: it, its memory and its staging buffer. False when `image`
/// is not one.
pub(crate) fn destroy_image(gpu: &Gpu, t: &Arc<Table>, image: u64) -> R<bool> {
    let Some(n) = gpu.native.lock().remove(&image) else { return Ok(false) };
    // Its copy on its way lands first: it reads the staging buffer.
    n.in_flight.wait();
    let d = vk::Device::from_raw(n.device);
    unsafe {
        if let Some(c) = &n.copier {
            vkfn!(t, ID_VK_DESTROY_FENCE, c"vkDestroyFence", vk::PFN_vkDestroyFence)(d, c.fence, std::ptr::null());
        }
        vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage)(d, vk::Image::from_raw(image), std::ptr::null());
        vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, n.memory, std::ptr::null());
        vkfn!(t, ID_VK_DESTROY_BUFFER, c"vkDestroyBuffer", vk::PFN_vkDestroyBuffer)(d, n.staging, std::ptr::null());
        vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, n.staging_memory, std::ptr::null());
        if let Some(direct) = &n.direct {
            vkfn!(t, ID_VK_DESTROY_BUFFER, c"vkDestroyBuffer", vk::PFN_vkDestroyBuffer)(d, direct.buffer, std::ptr::null());
            vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, direct.memory, std::ptr::null());
        }
        if let Some(share) = &n.share {
            // No reader may take it for the buffer's frame any more.
            super::share::write_generation(&n.shm, 0);
            vkfn!(t, ID_VK_DESTROY_IMAGE, c"vkDestroyImage", vk::PFN_vkDestroyImage)(d, share.image, std::ptr::null());
            vkfn!(t, ID_VK_FREE_MEMORY, c"vkFreeMemory", vk::PFN_vkFreeMemory)(d, share.memory, std::ptr::null());
            close_handle(share.handle);
        }
    }
    if n.direct.is_some() {
        // The import is freed: the view may go.
        n.shm.unpin_view();
    }
    Ok(true)
}

/// A queue of `device` the guest has, with its family.
fn any_queue(gpu: &Gpu, device: u64) -> R<(u64, u32)> {
    gpu.devices.lock().get(&device).and_then(|i| i.queues.iter().next().map(|(q, f)| (*q, *f))).ok_or(CallError::Missing("a queue"))
}

/// Submit nothing on `queue`, waiting for `waits` and signalling `signal`/`fence`.
fn submit_empty(t: &Arc<Table>, queue: u64, waits: &[vk::Semaphore], signal: vk::Semaphore, fence: vk::Fence) -> R<()> {
    let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
    let si = vk::SubmitInfo {
        wait_semaphore_count: waits.len() as u32,
        p_wait_semaphores: waits.as_ptr(),
        p_wait_dst_stage_mask: stages.as_ptr(),
        signal_semaphore_count: u32::from(signal != vk::Semaphore::null()),
        p_signal_semaphores: &signal,
        ..Default::default()
    };
    check(unsafe { vkfn!(t, ID_VK_QUEUE_SUBMIT, c"vkQueueSubmit", vk::PFN_vkQueueSubmit)(vk::Queue::from_raw(queue), 1, &si, fence) })
}

/// `vkAcquireImageANDROID(device, image, nativeFenceFd, semaphore, fence)`.
pub(crate) fn acquire(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (device, t) = gpu.dispatchable(p, a[0])?;
    let fd = a[2] as i32;
    if fd >= 0 {
        // The fence of whoever released the buffer: every fence here is signalled when it is made
        // (composition is synchronous), so there is nothing to wait for; it is ours to close.
        let _ = p.fds.remove(fd);
    }
    let (semaphore, fence) = (vk::Semaphore::from_raw(a[3]), vk::Fence::from_raw(a[4]));
    if semaphore != vk::Semaphore::null() || fence != vk::Fence::null() {
        let (queue, _) = any_queue(gpu, device)?;
        submit_empty(&t, queue, &[], semaphore, fence)?;
    }
    Ok(0)
}

/// `vkQueueSignalReleaseImageANDROID(queue, waitCount, pWaits, image, pNativeFenceFd)`: the
/// image's pixels, once its rendering is done, into the gralloc buffer.
pub(crate) fn release(gpu: &Gpu, p: &Process, a: &[u64]) -> R<u64> {
    let (queue, t) = gpu.dispatchable(p, a[0])?;
    let n = a[1] as u32;
    if n > 64 {
        return Err(CallError::Args);
    }
    let waits: Vec<vk::Semaphore> = (0..u64::from(n)).map(|i| p.mem.read_u64(a[2] + i * 8).map(vk::Semaphore::from_raw).map_err(|_| CallError::Args)).collect::<R<_>>()?;
    let image = a[3];
    // Its previous copy lands first: the staging buffer and the command buffer are reused.
    let in_flight = gpu.native.lock().get(&image).map(|img| Arc::clone(&img.in_flight)).ok_or(CallError::Args)?;
    in_flight.wait();
    let natives = gpu.native.lock();
    let Some(img) = natives.get(&image) else {
        return Err(CallError::Args);
    };
    let (device, width, height, stride, staging) = (img.device, img.width, img.height, img.stride, img.staging);
    let (mapped, bytes, shm, pixels_at) = (img.mapped, img.bytes, Arc::clone(&img.shm), img.pixels_at);
    let (invalidate, staging_memory) = (img.invalidate, img.staging_memory);
    // `gralloc_direct`: the copy goes into the region itself, and lands with nothing left to copy.
    let direct = img.direct.as_ref().filter(|_| DIRECT.load(std::sync::atomic::Ordering::Relaxed)).map(|d| d.buffer);
    let staging = direct.unwrap_or(staging);
    // `present_zero`: the frame into the share image too, in the same submit.
    let share = img.share.as_ref().filter(|_| super::share::on()).map(|s| s.image);
    let has_share = img.share.is_some();
    let own = img.copier.as_ref().map(|c| (c.cb, c.fence));
    drop(natives);
    let d = vk::Device::from_raw(device);
    let mut devices = gpu.devices.lock();
    let info = devices.get_mut(&device).ok_or(CallError::Handle(device))?;
    let family = *info.queues.get(&queue).ok_or(CallError::Handle(queue))?;
    if let std::collections::hash_map::Entry::Vacant(slot) = info.copiers.entry(family) {
        let pci = vk::CommandPoolCreateInfo { flags: vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER, queue_family_index: family, ..Default::default() };
        let mut pool = vk::CommandPool::null();
        check(unsafe { vkfn!(t, ID_VK_CREATE_COMMAND_POOL, c"vkCreateCommandPool", vk::PFN_vkCreateCommandPool)(d, &pci, std::ptr::null(), &mut pool) })?;
        let cai = vk::CommandBufferAllocateInfo { command_pool: pool, level: vk::CommandBufferLevel::PRIMARY, command_buffer_count: 1, ..Default::default() };
        let mut cb = vk::CommandBuffer::null();
        check(unsafe { vkfn!(t, ID_VK_ALLOCATE_COMMAND_BUFFERS, c"vkAllocateCommandBuffers", vk::PFN_vkAllocateCommandBuffers)(d, &cai, &mut cb) })?;
        // A host command buffer is dispatchable: the host's loader wrote its dispatch into it.
        let mut fence = vk::Fence::null();
        let fci = vk::FenceCreateInfo::default();
        check(unsafe { vkfn!(t, ID_VK_CREATE_FENCE, c"vkCreateFence", vk::PFN_vkCreateFence)(d, &fci, std::ptr::null(), &mut fence) })?;
        slot.insert(Copier { cb, fence });
        info.pools.insert(family, pool);
    }
    // The image's own command buffer and fence, from its family's pool (made once, under this lock).
    let (cb, fence) = match own {
        Some(c) => c,
        None => {
            let pool = info.pools.get(&family).copied().ok_or(CallError::Missing("a copy pool"))?;
            let cai = vk::CommandBufferAllocateInfo { command_pool: pool, level: vk::CommandBufferLevel::PRIMARY, command_buffer_count: 1, ..Default::default() };
            let mut cb = vk::CommandBuffer::null();
            check(unsafe { vkfn!(t, ID_VK_ALLOCATE_COMMAND_BUFFERS, c"vkAllocateCommandBuffers", vk::PFN_vkAllocateCommandBuffers)(d, &cai, &mut cb) })?;
            let mut fence = vk::Fence::null();
            let fci = vk::FenceCreateInfo::default();
            check(unsafe { vkfn!(t, ID_VK_CREATE_FENCE, c"vkCreateFence", vk::PFN_vkCreateFence)(d, &fci, std::ptr::null(), &mut fence) })?;
            if let Some(img) = gpu.native.lock().get_mut(&image) {
                img.copier = Some(Copier { cb, fence });
            }
            (cb, fence)
        }
    };
    let range = vk::ImageSubresourceRange { aspect_mask: vk::ImageAspectFlags::COLOR, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 };
    let to_src = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::MEMORY_WRITE,
        dst_access_mask: vk::AccessFlags::TRANSFER_READ,
        old_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        new_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        src_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        dst_queue_family_index: vk::QUEUE_FAMILY_IGNORED,
        image: vk::Image::from_raw(image),
        subresource_range: range,
        ..Default::default()
    };
    let back = vk::ImageMemoryBarrier {
        src_access_mask: vk::AccessFlags::TRANSFER_READ,
        dst_access_mask: vk::AccessFlags::empty(),
        old_layout: vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
        new_layout: vk::ImageLayout::PRESENT_SRC_KHR,
        ..to_src
    };
    let copy = vk::BufferImageCopy {
        buffer_offset: 0,
        buffer_row_length: stride,
        buffer_image_height: height,
        image_subresource: vk::ImageSubresourceLayers { aspect_mask: vk::ImageAspectFlags::COLOR, mip_level: 0, base_array_layer: 0, layer_count: 1 },
        image_offset: vk::Offset3D::default(),
        image_extent: vk::Extent3D { width, height, depth: 1 },
    };
    unsafe {
        check(vkfn!(t, ID_VK_RESET_COMMAND_BUFFER, c"vkResetCommandBuffer", vk::PFN_vkResetCommandBuffer)(cb, vk::CommandBufferResetFlags::empty()))?;
        let bi = vk::CommandBufferBeginInfo { flags: vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT, ..Default::default() };
        check(vkfn!(t, ID_VK_BEGIN_COMMAND_BUFFER, c"vkBeginCommandBuffer", vk::PFN_vkBeginCommandBuffer)(cb, &bi))?;
        let barrier = vkfn!(t, ID_VK_CMD_PIPELINE_BARRIER, c"vkCmdPipelineBarrier", vk::PFN_vkCmdPipelineBarrier);
        barrier(cb, vk::PipelineStageFlags::ALL_COMMANDS, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &to_src);
        vkfn!(t, ID_VK_CMD_COPY_IMAGE_TO_BUFFER, c"vkCmdCopyImageToBuffer", vk::PFN_vkCmdCopyImageToBuffer)(
            cb,
            vk::Image::from_raw(image),
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            staging,
            1,
            &copy,
        );
        if let Some(share) = share {
            // Its last contents are not kept; written whole, then handed to whichever process
            // reads it (`QUEUE_FAMILY_EXTERNAL`), in `GENERAL`, the layout the reader samples it in.
            let into = vk::ImageMemoryBarrier {
                src_access_mask: vk::AccessFlags::empty(),
                dst_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                old_layout: vk::ImageLayout::UNDEFINED,
                new_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                image: share,
                ..to_src
            };
            barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &into);
            let region = vk::ImageCopy {
                src_subresource: copy.image_subresource,
                src_offset: vk::Offset3D::default(),
                dst_subresource: copy.image_subresource,
                dst_offset: vk::Offset3D::default(),
                extent: vk::Extent3D { width, height, depth: 1 },
            };
            vkfn!(t, ID_VK_CMD_COPY_IMAGE, c"vkCmdCopyImage", vk::PFN_vkCmdCopyImage)(cb, vk::Image::from_raw(image), vk::ImageLayout::TRANSFER_SRC_OPTIMAL, share, vk::ImageLayout::TRANSFER_DST_OPTIMAL, 1, &region);
            let out = vk::ImageMemoryBarrier {
                src_access_mask: vk::AccessFlags::TRANSFER_WRITE,
                dst_access_mask: vk::AccessFlags::empty(),
                old_layout: vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                new_layout: vk::ImageLayout::GENERAL,
                src_queue_family_index: family,
                dst_queue_family_index: vk::QUEUE_FAMILY_EXTERNAL,
                image: share,
                ..to_src
            };
            barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &out);
        }
        barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), 0, std::ptr::null(), 0, std::ptr::null(), 1, &back);
        check(vkfn!(t, ID_VK_END_COMMAND_BUFFER, c"vkEndCommandBuffer", vk::PFN_vkEndCommandBuffer)(cb))?;
        check(vkfn!(t, ID_VK_RESET_FENCES, c"vkResetFences", vk::PFN_vkResetFences)(d, 1, &fence))?;
        let stages = vec![vk::PipelineStageFlags::ALL_COMMANDS; waits.len()];
        let si = vk::SubmitInfo {
            wait_semaphore_count: waits.len() as u32,
            p_wait_semaphores: waits.as_ptr(),
            p_wait_dst_stage_mask: stages.as_ptr(),
            command_buffer_count: 1,
            p_command_buffers: &cb,
            ..Default::default()
        };
        check(vkfn!(t, ID_VK_QUEUE_SUBMIT, c"vkQueueSubmit", vk::PFN_vkQueueSubmit)(vk::Queue::from_raw(queue), 1, &si, fence))?;
    }
    drop(devices);
    // On its way: the region says which generation it will have, and the worker lands it.
    let mut g = [0u8; 8];
    let _ = shm.read_at(&mut g, CONTENT_GENERATION_AT);
    let pending = u64::from_le_bytes(g).wrapping_add(1);
    // Which generation the share image will hold: this one, or none (not copied this time).
    if share.is_some() || has_share {
        super::share::write_generation(&shm, if share.is_some() { pending } else { 0 });
    }
    let _ = shm.write_at(&pending.to_le_bytes(), PENDING_GENERATION_AT);
    *in_flight.busy.lock() = true;
    let landing = Landing { table: Arc::clone(&t), device, fence, invalidate, staging_memory, mapped, bytes, shm, pixels_at, in_flight: Arc::clone(&in_flight), direct: direct.is_some() };
    // `OMNI_ASYNC_RELEASE=0`: landed here, on the app's thread, as before (for comparison).
    static ASYNC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ASYNC.get_or_init(|| std::env::var("OMNI_ASYNC_RELEASE").as_deref() != Ok("0")) {
        landing.land();
    } else if let Err(std::sync::mpsc::SendError(landing)) = landings().lock().send(landing) {
        landing.land();
    }
    if a[4] != 0 {
        p.mem.write(a[4], &(-1i32).to_le_bytes()).map_err(|_| CallError::Args)?;
    }
    Ok(0)
}

#[cfg(test)]
#[path = "native_wait_tests.rs"]
mod wait_tests;
