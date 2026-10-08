//! **The display window presented by the GPU** (`present_gpu=0|1`, `OMNI_PRESENT_GPU=1`; **off
//! by default**): each framebuffer frame goes to the window through a Vulkan swapchain of the
//! system host process's own, instead of GDI.
//!
//! The GDI path costs the system process, every frame: a swizzled 5.6 MB copy into the window's
//! canvas (at 1575x890; none under `present_bgra`), and `StretchDIBits` from `WM_PAINT` -- GDI
//! stretching the frame on the CPU into the window's redirection surface. Here the frame is copied
//! once into host-visible memory the GPU reads (a `memcpy`, RGBA or BGRA as the framebuffer holds
//! it: the source image takes either format, so there is never a swizzle), copied on the GPU into
//! an image and blitted -- scaled, when the window and the display differ -- into the swapchain
//! image. The window's thread paints nothing (its canvas is emptied, `Presenter::clear`).
//!
//! Its own instance and device, not the guest's forwarded ones: this is the host's window. Two
//! frames in flight, each with its own staging buffer and source image, so the copy into one never
//! races the GPU reading the other. The swapchain follows the window's client size; it is rebuilt
//! as `omni-gfx`'s renderer learned to on this host -- the new one created naming the old, the old
//! destroyed only after (destroying it first crashed the NVIDIA driver on the first resize).
//!
//! Presents MAILBOX when the surface offers it (never blocks; a newer frame replaces a waiting one),
//! else FIFO. A window system with no surface here (anything but Win32 and Xlib) answers an error,
//! and the window stays on GDI.
//!
//! **MEASURED: no CPU saved on its own** (`tests/present_cost.rs`, Windows, RTX 4060, 1575x890
//! frames at 60/s, process CPU per frame, E-cores at low priority, two runs): at 1:1 GDI 2.4-3.1 ms
//! (RGBA), **1.3-1.5 ms with `present_bgra`**, this path 2.1 ms; stretched to a 2400x1300 window
//! 3.5-3.7 / 1.6-1.8 / 1.5-1.8 ms. Its present call is 1.1-1.2 ms -- nearly all the copy into
//! host-visible memory -- and the driver's present ~0.9 ms more; GDI's `StretchDIBits` of a BGRA
//! frame costs about as much as that copy alone. What it is for: the GPU end of the path that takes
//! the frame from the app's GPU to the window without the CPU
//! (`docs/superpowers/specs/2026-10-09-gpu-present-design.md`), where nothing is copied in.
use std::sync::atomic::{AtomicBool, Ordering};

use ash::{khr, vk};
use omni_platform::window::RawWindow;

/// The lever (`present_gpu`, read live by the display window's present thread).
pub static ON: AtomicBool = AtomicBool::new(false);

/// Whether the GPU present is asked for: the lever, or `OMNI_PRESENT_GPU=1` from the start.
#[must_use]
pub fn on() -> bool {
    static FROM_ENV: std::sync::Once = std::sync::Once::new();
    FROM_ENV.call_once(|| {
        if std::env::var("OMNI_PRESENT_GPU").as_deref() == Ok("1") {
            ON.store(true, Ordering::Relaxed);
        }
    });
    ON.load(Ordering::Relaxed)
}

/// Frames recorded before the oldest must have finished.
const FRAMES: usize = 2;

/// One frame in flight: its commands, the fence that says the GPU is done with them, the semaphore
/// its swapchain image is acquired with, and the frame's pixels on their way.
struct Slot {
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    acquired: vk::Semaphore,
    source: Option<Source>,
}

/// A frame's pixels: a host-visible buffer the CPU copies into, mapped for good, and the image the
/// GPU copies it to and blits from.
struct Source {
    width: u32,
    height: u32,
    format: vk::Format,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    mapped: *mut u8,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
}

struct Chain {
    handle: vk::SwapchainKHR,
    extent: vk::Extent2D,
    images: Vec<vk::Image>,
    /// Signalled when an image's blit is done, waited on by its present: one per image, since the
    /// presentation engine holds it until it is done with that image.
    done: Vec<vk::Semaphore>,
    /// The fence of the frame last drawn into each image.
    in_flight: Vec<vk::Fence>,
}

/// A window presented through a Vulkan swapchain. Not `Send`: it is made and used on one thread
/// (the display window's present thread), and dropped before the window is.
pub struct WindowPresenter {
    instance: ash::Instance,
    surface_fn: khr::surface::Instance,
    surface: vk::SurfaceKHR,
    physical: vk::PhysicalDevice,
    memory: vk::PhysicalDeviceMemoryProperties,
    device: Option<ash::Device>,
    swapchain_fn: Option<khr::swapchain::Device>,
    queue: vk::Queue,
    pool: vk::CommandPool,
    slots: Vec<Slot>,
    chain: Option<Chain>,
    mode: vk::PresentModeKHR,
    next: usize,
    dirty: bool,
    /// The window's client size, last told.
    target: (u32, u32),
    frames: u64,
    name: String,
}

fn vkerr(what: &'static str) -> impl Fn(vk::Result) -> String {
    move |r| format!("{what}: {r}")
}

impl WindowPresenter {
    /// A presenter for `window` (its surface, a device that can present to it, its frames).
    ///
    /// # Errors
    /// No Vulkan, no surface for this window system, no device that presents to it, or anything the
    /// driver refused; nothing is left behind.
    pub fn new(window: RawWindow) -> Result<Self, String> {
        let entry = super::entry().map_err(|_| "no host Vulkan loader".to_string())?;
        let platform: &std::ffi::CStr = match window {
            RawWindow::Win32 { .. } => khr::win32_surface::NAME,
            RawWindow::Xlib { .. } => khr::xlib_surface::NAME,
            other => return Err(format!("no Vulkan surface here for a {} window", other.system_name())),
        };
        let extensions = [khr::surface::NAME.as_ptr(), platform.as_ptr()];
        let app = vk::ApplicationInfo::default().application_name(c"omnidroid-window").api_version(vk::make_api_version(0, 1, 0, 0));
        let info = vk::InstanceCreateInfo::default().application_info(&app).enabled_extension_names(&extensions);
        // SAFETY: `info` and what it points to outlive the call.
        let instance = unsafe { entry.create_instance(&info, None) }.map_err(vkerr("vkCreateInstance"))?;
        let surface_fn = khr::surface::Instance::new(entry, &instance);
        let mut me = Self {
            instance,
            surface_fn,
            surface: vk::SurfaceKHR::null(),
            physical: vk::PhysicalDevice::null(),
            memory: vk::PhysicalDeviceMemoryProperties::default(),
            device: None,
            swapchain_fn: None,
            queue: vk::Queue::null(),
            pool: vk::CommandPool::null(),
            slots: Vec::new(),
            chain: None,
            mode: vk::PresentModeKHR::FIFO,
            next: 0,
            dirty: true,
            target: (0, 0),
            frames: 0,
            name: String::new(),
        };
        // From here on an error drops `me`, which destroys what was made.
        me.surface = match window {
            RawWindow::Win32 { hwnd, hinstance } => {
                let info = vk::Win32SurfaceCreateInfoKHR::default().hinstance(hinstance).hwnd(hwnd);
                // SAFETY: a live window's handles (the caller's window outlives this presenter).
                unsafe { khr::win32_surface::Instance::new(entry, &me.instance).create_win32_surface(&info, None) }.map_err(vkerr("vkCreateWin32SurfaceKHR"))?
            }
            RawWindow::Xlib { display, window } => {
                let info = vk::XlibSurfaceCreateInfoKHR::default().dpy(display as *mut vk::Display).window(window as vk::Window);
                // SAFETY: as above.
                unsafe { khr::xlib_surface::Instance::new(entry, &me.instance).create_xlib_surface(&info, None) }.map_err(vkerr("vkCreateXlibSurfaceKHR"))?
            }
            _ => unreachable!("refused above"),
        };
        let (physical, family, name) = me.pick()?;
        me.physical = physical;
        me.name = name;
        // SAFETY: a live physical device of this instance.
        me.memory = unsafe { me.instance.get_physical_device_memory_properties(physical) };
        let priorities = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&priorities)];
        let device_extensions = [khr::swapchain::NAME.as_ptr()];
        let info = vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&device_extensions);
        // SAFETY: `info` outlives the call; the family was read from this device.
        let device = unsafe { me.instance.create_device(physical, &info, None) }.map_err(vkerr("vkCreateDevice"))?;
        // SAFETY: one queue of `family` was asked for.
        me.queue = unsafe { device.get_device_queue(family, 0) };
        me.swapchain_fn = Some(khr::swapchain::Device::new(&me.instance, &device));
        let pool_info = vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family);
        // SAFETY: a live device.
        let pool = unsafe { device.create_command_pool(&pool_info, None) };
        me.device = Some(device);
        let device = me.device.as_ref().expect("just set");
        me.pool = pool.map_err(vkerr("vkCreateCommandPool"))?;
        let alloc = vk::CommandBufferAllocateInfo::default().command_pool(me.pool).level(vk::CommandBufferLevel::PRIMARY).command_buffer_count(FRAMES as u32);
        // SAFETY: a live pool of this device.
        let cmds = unsafe { device.allocate_command_buffers(&alloc) }.map_err(vkerr("vkAllocateCommandBuffers"))?;
        for cmd in cmds {
            // SAFETY: a live device; each object is kept in `slots` and destroyed by `drop`.
            let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default().flags(vk::FenceCreateFlags::SIGNALED), None) }.map_err(vkerr("vkCreateFence"))?;
            let acquired = match unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) } {
                Ok(s) => s,
                Err(e) => {
                    // SAFETY: made just above, used by nothing.
                    unsafe { device.destroy_fence(fence, None) };
                    return Err(vkerr("vkCreateSemaphore")(e));
                }
            };
            me.slots.push(Slot { cmd, fence, acquired, source: None });
        }
        // SAFETY: live physical device and surface.
        let modes = unsafe { me.surface_fn.get_physical_device_surface_present_modes(physical, me.surface) }.map_err(vkerr("vkGetPhysicalDeviceSurfacePresentModesKHR"))?;
        me.mode = if modes.contains(&vk::PresentModeKHR::MAILBOX) { vk::PresentModeKHR::MAILBOX } else { vk::PresentModeKHR::FIFO };
        Ok(me)
    }

    /// The device it presents with, and its present mode, for the log.
    #[must_use]
    pub fn describe(&self) -> String {
        let mode = match self.mode {
            vk::PresentModeKHR::MAILBOX => "mailbox",
            vk::PresentModeKHR::FIFO => "fifo",
            _ => "other",
        };
        format!("{}, {mode}", self.name)
    }

    /// Frames presented so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// A device that can present to the surface (a discrete GPU first) and a queue family that can
    /// both blit and present.
    fn pick(&self) -> Result<(vk::PhysicalDevice, u32, String), String> {
        // SAFETY: a live instance.
        let devices = unsafe { self.instance.enumerate_physical_devices() }.map_err(vkerr("vkEnumeratePhysicalDevices"))?;
        let mut best: Option<(u8, vk::PhysicalDevice, u32, String)> = None;
        for pd in devices {
            // SAFETY: a live physical device of this instance.
            let (props, families, exts) = unsafe {
                (self.instance.get_physical_device_properties(pd), self.instance.get_physical_device_queue_family_properties(pd), self.instance.enumerate_device_extension_properties(pd).unwrap_or_default())
            };
            if !exts.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|n| n == khr::swapchain::NAME)) {
                continue;
            }
            let family = families.iter().enumerate().position(|(i, f)| {
                // SAFETY: as above; the surface is live.
                f.queue_flags.contains(vk::QueueFlags::GRAPHICS) && unsafe { self.surface_fn.get_physical_device_surface_support(pd, i as u32, self.surface) }.unwrap_or(false)
            });
            let Some(family) = family else { continue };
            let rank = match props.device_type {
                vk::PhysicalDeviceType::DISCRETE_GPU => 3,
                vk::PhysicalDeviceType::INTEGRATED_GPU => 2,
                _ => 1,
            };
            let name = props.device_name_as_c_str().map_or_else(|_| "?".into(), |n| n.to_string_lossy().into_owned());
            if best.as_ref().is_none_or(|b| rank > b.0) {
                best = Some((rank, pd, family as u32, name));
            }
        }
        best.map(|(_, pd, f, n)| (pd, f, n)).ok_or_else(|| "no device presents to this window".to_string())
    }

    fn device(&self) -> &ash::Device {
        self.device.as_ref().expect("made in new")
    }

    /// **Present a frame**: `pixels`, `width` x `height`, RGBA or (`bgra`) BGRA rows of `width`,
    /// scaled to the window, whose client area is `client` now. `Ok(false)`: nothing shown (a
    /// minimised window, a swapchain being rebuilt).
    ///
    /// # Errors
    /// What the driver refused (the device lost, memory exhausted): the caller goes back to GDI.
    pub fn present(&mut self, pixels: &[u8], width: u32, height: u32, bgra: bool, client: (u32, u32)) -> Result<bool, String> {
        let bytes = width as usize * height as usize * 4;
        if width == 0 || height == 0 || pixels.len() < bytes {
            return Ok(false);
        }
        if client != self.target {
            self.target = client;
            self.dirty = true;
        }
        if self.dirty || self.chain.is_none() {
            self.rebuild()?;
            if self.chain.is_none() {
                return Ok(false);
            }
        }
        let slot = self.next;
        let (fence, acquired, cmd) = (self.slots[slot].fence, self.slots[slot].acquired, self.slots[slot].cmd);
        // SAFETY: this slot's fence, made signalled; once it is, nothing of the slot is in use.
        unsafe { self.device().wait_for_fences(&[fence], true, u64::MAX) }.map_err(vkerr("vkWaitForFences"))?;
        let format = if bgra { vk::Format::B8G8R8A8_UNORM } else { vk::Format::R8G8B8A8_UNORM };
        if self.slots[slot].source.as_ref().is_none_or(|s| (s.width, s.height, s.format) != (width, height, format)) {
            if let Some(old) = self.slots[slot].source.take() {
                self.destroy_source(old);
            }
            self.slots[slot].source = Some(self.make_source(width, height, format)?);
        }
        let source = self.slots[slot].source.as_ref().expect("just made");
        // SAFETY: `mapped` is this slot's buffer, `bytes` long, coherent; the GPU is done with it
        // (the fence above), and `pixels` holds `bytes` bytes.
        unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), source.mapped, bytes) };
        let (src_image, src_buffer) = (source.image, source.buffer);
        let chain = self.chain.as_ref().expect("checked");
        let swapchain_fn = self.swapchain_fn.as_ref().expect("made in new");
        // SAFETY: a live swapchain and an unsignalled semaphore of this slot (its last wait is done).
        let (index, suboptimal) = match unsafe { swapchain_fn.acquire_next_image(chain.handle, u64::MAX, acquired, vk::Fence::null()) } {
            Ok(v) => v,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                self.dirty = true;
                return Ok(false);
            }
            Err(e) => return Err(vkerr("vkAcquireNextImageKHR")(e)),
        };
        let i = index as usize;
        let device = self.device.as_ref().expect("made in new");
        let earlier = chain.in_flight[i];
        if earlier != vk::Fence::null() && earlier != fence {
            // SAFETY: a live fence of another slot.
            unsafe { device.wait_for_fences(&[earlier], true, u64::MAX) }.map_err(vkerr("vkWaitForFences"))?;
        }
        let (target, extent, done) = (chain.images[i], chain.extent, chain.done[i]);
        let src_extent = (width, height);
        let filter = if src_extent == (extent.width, extent.height) {
            vk::Filter::NEAREST
        } else {
            // SAFETY: a live physical device.
            let features = unsafe { self.instance.get_physical_device_format_properties(self.physical, format) }.optimal_tiling_features;
            if features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR) { vk::Filter::LINEAR } else { vk::Filter::NEAREST }
        };
        // SAFETY: the slot's command buffer is not in use (its fence); every handle is live.
        unsafe {
            device.reset_fences(&[fence]).map_err(vkerr("vkResetFences"))?;
            device.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT)).map_err(vkerr("vkBeginCommandBuffer"))?;
            barrier(device, cmd, src_image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TOP_OF_PIPE);
            let copy = vk::BufferImageCopy::default().image_subresource(layers()).image_extent(vk::Extent3D { width, height, depth: 1 });
            device.cmd_copy_buffer_to_image(cmd, src_buffer, src_image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[copy]);
            barrier(device, cmd, src_image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, vk::AccessFlags::TRANSFER_WRITE, vk::AccessFlags::TRANSFER_READ, vk::PipelineStageFlags::TRANSFER);
            barrier(device, cmd, target, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::AccessFlags::empty(), vk::AccessFlags::TRANSFER_WRITE, vk::PipelineStageFlags::TRANSFER);
            let blit = vk::ImageBlit::default()
                .src_subresource(layers())
                .src_offsets([vk::Offset3D::default(), vk::Offset3D { x: width as i32, y: height as i32, z: 1 }])
                .dst_subresource(layers())
                .dst_offsets([vk::Offset3D::default(), vk::Offset3D { x: extent.width as i32, y: extent.height as i32, z: 1 }]);
            device.cmd_blit_image(cmd, src_image, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, target, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[blit], filter);
            let to_present = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(target)
                .subresource_range(range());
            device.cmd_pipeline_barrier(cmd, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[], &[to_present]);
            device.end_command_buffer(cmd).map_err(vkerr("vkEndCommandBuffer"))?;
            let (waits, stages, cmds, signals) = ([acquired], [vk::PipelineStageFlags::TRANSFER], [cmd], [done]);
            let submit = vk::SubmitInfo::default().wait_semaphores(&waits).wait_dst_stage_mask(&stages).command_buffers(&cmds).signal_semaphores(&signals);
            device.queue_submit(self.queue, &[submit], fence).map_err(vkerr("vkQueueSubmit"))?;
        }
        if let Some(c) = self.chain.as_mut() {
            c.in_flight[i] = fence;
        }
        let chain = self.chain.as_ref().expect("checked");
        let (swapchains, indices, waits) = ([chain.handle], [index], [done]);
        let info = vk::PresentInfoKHR::default().wait_semaphores(&waits).swapchains(&swapchains).image_indices(&indices);
        // SAFETY: the image was acquired above and its blit submitted, signalling `done`.
        match unsafe { swapchain_fn.queue_present(self.queue, &info) } {
            Ok(late) => self.dirty |= suboptimal | late,
            Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => self.dirty = true,
            Err(e) => return Err(vkerr("vkQueuePresentKHR")(e)),
        }
        self.next = (self.next + 1) % FRAMES;
        self.frames += 1;
        Ok(true)
    }

    /// Make the swapchain anew for the window's size, naming the old one; none for a size of zero.
    fn rebuild(&mut self) -> Result<(), String> {
        let device = self.device.as_ref().expect("made in new");
        // SAFETY: a live device; idle, nothing uses the old swapchain's images or semaphores.
        unsafe { device.device_wait_idle() }.map_err(vkerr("vkDeviceWaitIdle"))?;
        self.dirty = false;
        // SAFETY: live physical device and surface.
        let caps = unsafe { self.surface_fn.get_physical_device_surface_capabilities(self.physical, self.surface) }.map_err(vkerr("vkGetPhysicalDeviceSurfaceCapabilitiesKHR"))?;
        let extent = if caps.current_extent.width == u32::MAX {
            vk::Extent2D {
                width: self.target.0.clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: self.target.1.clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        } else {
            caps.current_extent
        };
        if extent.width == 0 || extent.height == 0 || self.target.0 == 0 || self.target.1 == 0 {
            self.destroy_chain();
            return Ok(());
        }
        if !caps.supported_usage_flags.contains(vk::ImageUsageFlags::TRANSFER_DST) {
            return Err("the window's swapchain images cannot be blitted to".into());
        }
        // SAFETY: as above.
        let formats = unsafe { self.surface_fn.get_physical_device_surface_formats(self.physical, self.surface) }.map_err(vkerr("vkGetPhysicalDeviceSurfaceFormatsKHR"))?;
        // A UNORM format: an `_SRGB` one would encode the frame's (already encoded) bytes again.
        let blittable = |f: &vk::SurfaceFormatKHR| {
            // SAFETY: a live physical device.
            let props = unsafe { self.instance.get_physical_device_format_properties(self.physical, f.format) };
            props.optimal_tiling_features.contains(vk::FormatFeatureFlags::BLIT_DST)
        };
        let format = [vk::Format::B8G8R8A8_UNORM, vk::Format::R8G8B8A8_UNORM]
            .iter()
            .find_map(|want| formats.iter().find(|f| f.format == *want && blittable(f)))
            .copied()
            .ok_or_else(|| format!("no UNORM 8-bit surface format to blit to among {:?}", formats.iter().map(|f| f.format.as_raw()).collect::<Vec<_>>()))?;
        let mut count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            count = count.min(caps.max_image_count);
        }
        let alpha = [vk::CompositeAlphaFlagsKHR::OPAQUE, vk::CompositeAlphaFlagsKHR::INHERIT, vk::CompositeAlphaFlagsKHR::PRE_MULTIPLIED, vk::CompositeAlphaFlagsKHR::POST_MULTIPLIED]
            .into_iter()
            .find(|a| caps.supported_composite_alpha.contains(*a))
            .unwrap_or(vk::CompositeAlphaFlagsKHR::OPAQUE);
        let old = self.chain.as_ref().map_or(vk::SwapchainKHR::null(), |c| c.handle);
        let info = vk::SwapchainCreateInfoKHR::default()
            .surface(self.surface)
            .min_image_count(count)
            .image_format(format.format)
            .image_color_space(format.color_space)
            .image_extent(extent)
            .image_array_layers(1)
            .image_usage(vk::ImageUsageFlags::TRANSFER_DST)
            .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
            .pre_transform(caps.current_transform)
            .composite_alpha(alpha)
            .present_mode(self.mode)
            .clipped(true)
            .old_swapchain(old);
        let swapchain_fn = self.swapchain_fn.as_ref().expect("made in new");
        // SAFETY: every handle in `info` is live, the old swapchain too (destroyed only after).
        let handle = unsafe { swapchain_fn.create_swapchain(&info, None) }.map_err(vkerr("vkCreateSwapchainKHR"))?;
        self.destroy_chain();
        let swapchain_fn = self.swapchain_fn.as_ref().expect("made in new");
        let device = self.device.as_ref().expect("made in new");
        // SAFETY: the swapchain just made.
        let images = match unsafe { swapchain_fn.get_swapchain_images(handle) } {
            Ok(images) => images,
            Err(e) => {
                // SAFETY: made above, used by nothing.
                unsafe { swapchain_fn.destroy_swapchain(handle, None) };
                return Err(vkerr("vkGetSwapchainImagesKHR")(e));
            }
        };
        let mut done = Vec::new();
        for _ in &images {
            // SAFETY: a live device.
            match unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) } {
                Ok(s) => done.push(s),
                Err(e) => {
                    // SAFETY: made here, used by nothing.
                    unsafe {
                        for s in done {
                            device.destroy_semaphore(s, None);
                        }
                        swapchain_fn.destroy_swapchain(handle, None);
                    }
                    return Err(vkerr("vkCreateSemaphore")(e));
                }
            }
        }
        self.chain = Some(Chain { handle, extent, in_flight: vec![vk::Fence::null(); images.len()], images, done });
        Ok(())
    }

    /// The swapchain and its semaphores, if any. The device is idle or a new swapchain names it.
    fn destroy_chain(&mut self) {
        let Some(chain) = self.chain.take() else { return };
        let (device, swapchain_fn) = (self.device.as_ref().expect("made in new"), self.swapchain_fn.as_ref().expect("made in new"));
        // SAFETY: see above: nothing uses them any more.
        unsafe {
            for s in chain.done {
                device.destroy_semaphore(s, None);
            }
            swapchain_fn.destroy_swapchain(chain.handle, None);
        }
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.memory.memory_type_count).find(|&i| bits & (1 << i) != 0 && self.memory.memory_types[i as usize].property_flags.contains(want))
    }

    /// The staging buffer's memory: host-visible and coherent, in the host's RAM rather than the
    /// GPU's (a write through the PCIe BAR is the slow way to copy 5.6 MB), cached first unless
    /// `OMNI_PRESENT_GPU_MEM=wc` asks for write-combined.
    fn staging_type(&self, bits: u32) -> Option<u32> {
        use vk::MemoryPropertyFlags as F;
        let wc = std::env::var("OMNI_PRESENT_GPU_MEM").as_deref() == Ok("wc");
        let host = |i: u32, cached: Option<bool>| {
            let f = self.memory.memory_types[i as usize].property_flags;
            bits & (1 << i) != 0 && f.contains(F::HOST_VISIBLE | F::HOST_COHERENT) && !f.contains(F::DEVICE_LOCAL) && cached.is_none_or(|c| f.contains(F::HOST_CACHED) == c)
        };
        let n = self.memory.memory_type_count;
        (0..n)
            .find(|&i| host(i, Some(!wc)))
            .or_else(|| (0..n).find(|&i| host(i, None)))
            .or_else(|| self.memory_type(bits, F::HOST_VISIBLE | F::HOST_COHERENT))
    }

    /// A slot's staging buffer (mapped) and source image for frames of this size and format.
    fn make_source(&self, width: u32, height: u32, format: vk::Format) -> Result<Source, String> {
        let device = self.device();
        let size = u64::from(width) * u64::from(height) * 4;
        let mut s = Source {
            width,
            height,
            format,
            buffer: vk::Buffer::null(),
            buffer_memory: vk::DeviceMemory::null(),
            mapped: std::ptr::null_mut(),
            image: vk::Image::null(),
            image_memory: vk::DeviceMemory::null(),
        };
        let made = (|| -> Result<(), String> {
            // SAFETY (the block): a live device; each object is recorded in `s` as soon as it is
            // made, so a failure part way is undone by `destroy_source`.
            unsafe {
                s.buffer = device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_SRC).sharing_mode(vk::SharingMode::EXCLUSIVE), None).map_err(vkerr("vkCreateBuffer"))?;
                let need = device.get_buffer_memory_requirements(s.buffer);
                let kind = self.staging_type(need.memory_type_bits).ok_or("no host-visible memory")?;
                s.buffer_memory = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(kind), None).map_err(vkerr("vkAllocateMemory"))?;
                device.bind_buffer_memory(s.buffer, s.buffer_memory, 0).map_err(vkerr("vkBindBufferMemory"))?;
                s.mapped = device.map_memory(s.buffer_memory, 0, size, vk::MemoryMapFlags::empty()).map_err(vkerr("vkMapMemory"))?.cast();
                let info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(format)
                    .extent(vk::Extent3D { width, height, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .initial_layout(vk::ImageLayout::UNDEFINED);
                s.image = device.create_image(&info, None).map_err(vkerr("vkCreateImage"))?;
                let need = device.get_image_memory_requirements(s.image);
                let kind = self.memory_type(need.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL).or_else(|| self.memory_type(need.memory_type_bits, vk::MemoryPropertyFlags::empty())).ok_or("no memory for the image")?;
                s.image_memory = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(kind), None).map_err(vkerr("vkAllocateMemory"))?;
                device.bind_image_memory(s.image, s.image_memory, 0).map_err(vkerr("vkBindImageMemory"))?;
            }
            Ok(())
        })();
        match made {
            Ok(()) => Ok(s),
            Err(e) => {
                self.destroy_source(s);
                Err(e)
            }
        }
    }

    /// A source no frame in flight uses.
    fn destroy_source(&self, s: Source) {
        let device = self.device();
        // SAFETY: unused (its slot's fence was waited on, or the device is idle); null handles are
        // ignored by Vulkan's destroy and free calls.
        unsafe {
            if !s.mapped.is_null() {
                device.unmap_memory(s.buffer_memory);
            }
            device.destroy_buffer(s.buffer, None);
            device.free_memory(s.buffer_memory, None);
            device.destroy_image(s.image, None);
            device.free_memory(s.image_memory, None);
        }
    }
}

impl Drop for WindowPresenter {
    fn drop(&mut self) {
        if let Some(device) = self.device.as_ref() {
            // SAFETY: a live device; once idle nothing below is in use.
            unsafe {
                let _ = device.device_wait_idle();
            }
            self.destroy_chain();
            let slots = std::mem::take(&mut self.slots);
            for slot in slots {
                if let Some(s) = slot.source {
                    self.destroy_source(s);
                }
                let device = self.device();
                // SAFETY: idle; made by this presenter.
                unsafe {
                    device.destroy_fence(slot.fence, None);
                    device.destroy_semaphore(slot.acquired, None);
                }
            }
            let device = self.device.take().expect("checked");
            // SAFETY: idle; the pool's buffers go with it, and the device is destroyed last.
            unsafe {
                device.destroy_command_pool(self.pool, None);
                device.destroy_device(None);
            }
        }
        // SAFETY: the surface (null when never made) and the instance, after everything made from
        // them.
        unsafe {
            self.surface_fn.destroy_surface(self.surface, None);
            self.instance.destroy_instance(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use ash::vk;

    /// **The feasibility probe for writing a frame straight into its gralloc region** (the design
    /// in `docs/superpowers/specs/2026-10-09-gpu-present-design.md`, step A0): a graphics region's
    /// host view, imported as Vulkan memory (`VK_EXT_external_memory_host`), is written by the GPU
    /// and read back through the region as any reader (the composer, another host process mapping
    /// the same file) reads it. Asks the host GPU, opens no window (`cargo test --release -p
    /// omni-linux --lib -- --ignored gralloc_region_imports`).
    #[test]
    #[ignore = "asks the host GPU"]
    fn a_gralloc_region_imports_as_gpu_memory() {
        let entry = super::super::entry().expect("a Vulkan loader");
        let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));
        // SAFETY (the test): every handle is made here, used on this thread, destroyed at the end.
        unsafe {
            let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).expect("instance");
            let pd = instance.enumerate_physical_devices().expect("devices").into_iter().max_by_key(|&pd| u8::from(instance.get_physical_device_properties(pd).device_type == vk::PhysicalDeviceType::DISCRETE_GPU)).expect("a device");
            let family = instance.get_physical_device_queue_family_properties(pd).iter().position(|f| f.queue_flags.contains(vk::QueueFlags::TRANSFER) || f.queue_flags.contains(vk::QueueFlags::GRAPHICS)).expect("a queue") as u32;
            let mut host_props = vk::PhysicalDeviceExternalMemoryHostPropertiesEXT::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut host_props);
            instance.get_physical_device_properties2(pd, &mut props2);
            let align = host_props.min_imported_host_pointer_alignment as usize;
            eprintln!("minImportedHostPointerAlignment {align:#x}");
            let exts = [ash::ext::external_memory_host::NAME.as_ptr()];
            let prio = [1.0f32];
            let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
            let device = instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&exts), None).expect("a device with VK_EXT_external_memory_host");
            let host = ash::ext::external_memory_host::DeviceFn::load(|name| std::mem::transmute(instance.get_device_proc_addr(device.handle(), name.as_ptr())));

            // A 1575x890 frame at stride 1600 after gralloc's metadata page, as a region is laid out.
            let bytes: usize = 1600 * 890 * 4;
            let size = bytes.div_ceil(align) * align;
            let shm = crate::shm::Shm::create("a0-probe").expect("region");
            shm.set_len((crate::hal::gralloc::PIXELS_AT as usize + bytes) as u64).expect("size");
            shm.as_graphics_buffer();
            shm.write_at(&vec![0x11u8; bytes], crate::hal::gralloc::PIXELS_AT).expect("write");
            let ptr = shm.bytes(crate::hal::gralloc::PIXELS_AT, bytes).expect("a view").as_ptr();
            assert_eq!(ptr as usize % align, 0, "the pixels are page-aligned in the view");

            let mut pointer_props = vk::MemoryHostPointerPropertiesEXT::default();
            (host.get_memory_host_pointer_properties_ext)(device.handle(), vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT, ptr.cast(), &mut pointer_props).result().expect("vkGetMemoryHostPointerPropertiesEXT on a region's view");
            eprintln!("memory types for the view: {:#b}", pointer_props.memory_type_bits);
            let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
            let buffer = device.create_buffer(&vk::BufferCreateInfo::default().size(size as u64).usage(vk::BufferUsageFlags::TRANSFER_DST).push_next(&mut ext_info), None).expect("buffer");
            let need = device.get_buffer_memory_requirements(buffer);
            let kind = (0..32).find(|i| pointer_props.memory_type_bits & need.memory_type_bits & (1 << i) != 0).expect("a memory type for both");
            let mut import = vk::ImportMemoryHostPointerInfoEXT::default().handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT).host_pointer(ptr as *mut _);
            let memory = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(size as u64).memory_type_index(kind).push_next(&mut import), None).expect("the view imported");
            device.bind_buffer_memory(buffer, memory, 0).expect("bind");

            // The GPU writes the frame; the region shows it.
            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family), None).expect("pool");
            let cb = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)).expect("cb")[0];
            device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
            device.cmd_fill_buffer(cb, buffer, 0, bytes as u64, 0xA0B0_C0D0);
            device.end_command_buffer(cb).expect("end");
            let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).expect("fence");
            let cbs = [cb];
            device.queue_submit(device.get_device_queue(family, 0), &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
            device.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
            let mut back = vec![0u8; bytes];
            shm.read_at(&mut back, crate::hal::gralloc::PIXELS_AT).expect("read");
            assert!(back.chunks_exact(4).all(|p| p == 0xA0B0_C0D0u32.to_le_bytes()), "the GPU's write is in the region");

            // **What A0 takes away**, timed: today a frame is copied by the GPU from a device-local
            // image into a host-cached staging buffer, then by the release worker into the region
            // (`Landing::land`'s `write_at`); with A0 the GPU copies into the region and the worker
            // copies nothing. A device-local buffer stands in for the image.
            let mem = instance.get_physical_device_memory_properties(pd);
            let find = |bits: u32, want: vk::MemoryPropertyFlags| (0..mem.memory_type_count).find(|&i| bits & (1 << i) != 0 && mem.memory_types[i as usize].property_flags.contains(want)).expect("a memory type");
            let make = |usage: vk::BufferUsageFlags, want: vk::MemoryPropertyFlags| {
                let b = device.create_buffer(&vk::BufferCreateInfo::default().size(bytes as u64).usage(usage), None).expect("buffer");
                let need = device.get_buffer_memory_requirements(b);
                let m = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(find(need.memory_type_bits, want)), None).expect("memory");
                device.bind_buffer_memory(b, m, 0).expect("bind");
                (b, m)
            };
            let (frame, frame_mem) = make(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST, vk::MemoryPropertyFlags::DEVICE_LOCAL);
            let (staging, staging_mem) = make(vk::BufferUsageFlags::TRANSFER_DST, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED);
            let mapped = device.map_memory(staging_mem, 0, bytes as u64, vk::MemoryMapFlags::empty()).expect("map").cast::<u8>();
            let queue = device.get_device_queue(family, 0);
            let copy_to = |dst: vk::Buffer| {
                device.reset_fences(&[fence]).expect("reset");
                device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
                device.cmd_copy_buffer(cb, frame, dst, &[vk::BufferCopy { src_offset: 0, dst_offset: 0, size: bytes as u64 }]);
                device.end_command_buffer(cb).expect("end");
                let t = std::time::Instant::now();
                device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
                device.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
                t.elapsed().as_secs_f64() * 1000.0
            };
            let (mut gpu_old, mut cpu_old, mut gpu_new) = (Vec::new(), Vec::new(), Vec::new());
            for _ in 0..20 {
                gpu_old.push(copy_to(staging));
                let t = std::time::Instant::now();
                shm.write_at(std::slice::from_raw_parts(mapped, bytes), crate::hal::gralloc::PIXELS_AT).expect("land");
                cpu_old.push(t.elapsed().as_secs_f64() * 1000.0);
                gpu_new.push(copy_to(buffer));
            }
            let med = |v: &mut Vec<f64>| {
                v.sort_by(f64::total_cmp);
                v[v.len() / 2]
            };
            eprintln!(
                "A0 1600x890 frame: today GPU copy to staging {:.2} ms + the worker's CPU copy into the region {:.2} ms; A0 GPU copy into the region {:.2} ms, no CPU copy",
                med(&mut gpu_old),
                med(&mut cpu_old),
                med(&mut gpu_new)
            );
            device.unmap_memory(staging_mem);
            for (b, m) in [(frame, frame_mem), (staging, staging_mem)] {
                device.destroy_buffer(b, None);
                device.free_memory(m, None);
            }

            device.destroy_fence(fence, None);
            device.destroy_command_pool(pool, None);
            device.destroy_buffer(buffer, None);
            device.free_memory(memory, None);
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
    }
}

fn range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default().aspect_mask(vk::ImageAspectFlags::COLOR).level_count(1).layer_count(1)
}

fn layers() -> vk::ImageSubresourceLayers {
    vk::ImageSubresourceLayers::default().aspect_mask(vk::ImageAspectFlags::COLOR).layer_count(1)
}

/// An image layout transition, from `src_stage`'s `src` access to the transfer stage's `dst`.
#[allow(clippy::too_many_arguments)]
unsafe fn barrier(device: &ash::Device, cmd: vk::CommandBuffer, image: vk::Image, old: vk::ImageLayout, new: vk::ImageLayout, src: vk::AccessFlags, dst: vk::AccessFlags, src_stage: vk::PipelineStageFlags) {
    let b = vk::ImageMemoryBarrier::default()
        .old_layout(old)
        .new_layout(new)
        .src_access_mask(src)
        .dst_access_mask(dst)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(range());
    // SAFETY: the caller's recording command buffer and live image.
    unsafe { device.cmd_pipeline_barrier(cmd, src_stage, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[b]) };
}
