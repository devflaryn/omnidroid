//! `present_zero` on the host GPU: share images made and exported by name on one Vulkan device (as
//! the app's host process makes them, `native::export_share` + the release's copy and hand-over),
//! opened by name and composed on another (the system host's, [`super::WindowPresenter`]), read
//! back and compared with the CPU's composition of the same layers (`hal::compose`). Then the cost
//! of a frame each way. Asks the host GPU, opens no window:
//!
//! ```text
//! cargo test --release -p omni-linux --lib -- --ignored --nocapture zero_
//! ```
#![cfg(windows)]

use ash::vk;

use super::super::share::{self, ShareBlend, ShareDesc, ShareLayer};
use crate::hal::compose::{self, Blend, Layer, Source};

/// The app's side: a device that exports share images.
struct App {
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
    uuids: ([u8; 16], [u8; 16]),
    made: Vec<(vk::Image, vk::DeviceMemory, isize)>,
}

impl App {
    fn new() -> Self {
        let entry = super::super::entry().expect("a Vulkan loader");
        let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 1, 0));
        // SAFETY (the test): handles made here and destroyed in `drop`.
        unsafe {
            let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).expect("instance");
            let pd = instance.enumerate_physical_devices().expect("devices").into_iter().max_by_key(|&pd| u8::from(instance.get_physical_device_properties(pd).device_type == vk::PhysicalDeviceType::DISCRETE_GPU)).expect("a device");
            let family = instance.get_physical_device_queue_family_properties(pd).iter().position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS)).expect("a queue") as u32;
            let exts = [ash::khr::external_memory_win32::NAME.as_ptr()];
            let prio = [1.0f32];
            let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
            let device = instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&exts), None).expect("an exporting device");
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            instance.get_physical_device_properties2(pd, &mut props);
            let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family), None).expect("pool");
            let memory = instance.get_physical_device_memory_properties(pd);
            Self { queue: device.get_device_queue(family, 0), instance, device, pool, family, memory, uuids: (id.device_uuid, id.driver_uuid), made: Vec::new() }
        }
    }

    fn kind(&self, bits: u32, want: vk::MemoryPropertyFlags) -> u32 {
        (0..self.memory.memory_type_count).find(|&i| bits & (1 << i) != 0 && self.memory.memory_types[i as usize].property_flags.contains(want)).expect("a memory type")
    }

    /// A share image of these bytes (in `format`'s own order), exported by a new name and handed
    /// over as the release hands it (`GENERAL`, to `QUEUE_FAMILY_EXTERNAL`).
    fn share(&mut self, format: vk::Format, width: u32, height: u32, bytes: &[u8]) -> ShareDesc {
        let name = share::next_name();
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        let d = &self.device;
        // SAFETY (the test).
        unsafe {
            let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(format)
                .extent(vk::Extent3D { width, height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(share::USAGE)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external);
            let image = d.create_image(&info, None).expect("image");
            let need = d.get_image_memory_requirements(image);
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32);
            let mut named = vk::ExportMemoryWin32HandleInfoKHR::default().dw_access(0x1000_0000).name(wide.as_ptr());
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let ai = vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(self.kind(need.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)).push_next(&mut export).push_next(&mut named).push_next(&mut dedicated);
            let memory = d.allocate_memory(&ai, None).expect("exportable memory");
            d.bind_image_memory(image, memory, 0).expect("bind");
            let win32 = ash::khr::external_memory_win32::Device::new(&self.instance, d);
            let handle = win32.get_memory_win32_handle(&vk::MemoryGetWin32HandleInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_WIN32)).expect("a named handle");
            // The pixels, through a staging buffer.
            let staging = d.create_buffer(&vk::BufferCreateInfo::default().size(bytes.len() as u64).usage(vk::BufferUsageFlags::TRANSFER_SRC), None).expect("buffer");
            let sneed = d.get_buffer_memory_requirements(staging);
            let smem = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(sneed.size).memory_type_index(self.kind(sneed.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT)), None).expect("memory");
            d.bind_buffer_memory(staging, smem, 0).expect("bind");
            let mapped = d.map_memory(smem, 0, bytes.len() as u64, vk::MemoryMapFlags::empty()).expect("map").cast::<u8>();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), mapped, bytes.len());
            let cb = d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(self.pool).command_buffer_count(1)).expect("cb")[0];
            d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
            let range = super::range();
            let to_dst = vk::ImageMemoryBarrier::default().old_layout(vk::ImageLayout::UNDEFINED).new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).dst_access_mask(vk::AccessFlags::TRANSFER_WRITE).src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED).image(image).subresource_range(range);
            d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[to_dst]);
            let copy = vk::BufferImageCopy::default().image_subresource(super::layers()).image_extent(vk::Extent3D { width, height, depth: 1 });
            d.cmd_copy_buffer_to_image(cb, staging, image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[copy]);
            let out = vk::ImageMemoryBarrier::default().old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL).new_layout(vk::ImageLayout::GENERAL).src_access_mask(vk::AccessFlags::TRANSFER_WRITE).src_queue_family_index(self.family).dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL).image(image).subresource_range(range);
            d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::BOTTOM_OF_PIPE, vk::DependencyFlags::empty(), &[], &[], &[out]);
            d.end_command_buffer(cb).expect("end");
            let fence = d.create_fence(&vk::FenceCreateInfo::default(), None).expect("fence");
            let cbs = [cb];
            d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
            d.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.pool, &cbs);
            d.destroy_buffer(staging, None);
            d.free_memory(smem, None);
            self.made.push((image, memory, handle));
            ShareDesc { format, width, height, size: need.size, device_uuid: self.uuids.0, driver_uuid: self.uuids.1, name }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // SAFETY (the test): idle; made here.
        unsafe {
            let _ = self.device.device_wait_idle();
            for (image, memory, handle) in self.made.drain(..) {
                self.device.destroy_image(image, None);
                self.device.free_memory(memory, None);
                use std::os::windows::io::FromRawHandle as _;
                drop(std::os::windows::io::OwnedHandle::from_raw_handle(handle as *mut std::ffi::c_void));
            }
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

/// An in-world frame: an opaque SurfaceView (RGBA) under the app's window (RGBA, premultiplied:
/// transparent but for a 60-row opaque bar and a translucent panel), and a small BGRA coverage
/// layer cropped and placed off the corner.
fn scene(w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let surface: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, ((i / w) % 241) as u8, (i % 239) as u8, 255]).collect();
    let window: Vec<u8> = (0..w * h)
        .flat_map(|i| {
            let (x, y) = (i % w, i / w);
            if y < 60 {
                [30, 40, 50, 255]
            } else if (40..340).contains(&x) && (600..800).contains(&y) {
                [60, 50, 40, 128]
            } else {
                [0, 0, 0, 0]
            }
        })
        .collect();
    // 64x48 BGRA, alpha varying.
    let badge: Vec<u8> = (0..64 * 48).flat_map(|i| [(i % 200) as u8, 100, 220, (i % 256) as u8]).collect();
    (surface, window, badge)
}

/// **More share images opened within ~4 s than the descriptor pool holds** (a window resized a
/// few times, each resize new buffers): every one is still shown. The pool holds 64 sets and an
/// image was let go only after `KEEP_IMPORTED` frames undrawn, so the 65th open failed -- and
/// every frame's after it, with nothing composed for anything to age by: present_zero on the CPU
/// for good. Each image's own colour comes back, so the right one was drawn.
#[test]
#[ignore = "asks the host GPU"]
fn zero_more_share_images_than_the_descriptor_pool_holds() {
    let (w, h) = (16u32, 16u32);
    let mut app = App::new();
    let mut presenter = super::WindowPresenter::new_headless().expect("a presenter");
    for k in 0..100u32 {
        let colour = [(k * 7 % 256) as u8, (k * 13 % 256) as u8, (k * 29 % 256) as u8, 255];
        let bytes: Vec<u8> = (0..w * h).flat_map(|_| colour).collect();
        let desc = app.share(vk::Format::B8G8R8A8_UNORM, w, h, &bytes);
        let layer = ShareLayer { desc, opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::None, generation: 1 };
        let frame = presenter.compose_offscreen(&[layer], (w, h)).unwrap_or_else(|e| panic!("share image {k}: {e}"));
        let at = ((h / 2 * w + w / 2) * 4) as usize;
        assert_eq!(&frame[at..at + 3], &colour[..3], "share image {k}'s own pixels");
    }
}

/// **A frame given up after its layers were opened takes nothing over**: the composition acquires a
/// share image from the app's process (`QUEUE_FAMILY_EXTERNAL`) once per generation, and recorded
/// the generation as acquired when the layer was opened -- so a frame that then failed (another
/// layer that would not open; in a window, an out-of-date swapchain) left the next frame of the same
/// generation sampling an image it never acquired.
#[test]
#[ignore = "asks the host GPU"]
fn zero_a_frame_given_up_takes_nothing_over() {
    let (w, h) = (16u32, 16u32);
    let mut app = App::new();
    let good = app.share(vk::Format::B8G8R8A8_UNORM, w, h, &vec![200u8; (w * h * 4) as usize]);
    let mut gone = good.clone();
    gone.name = format!("{}-never-made", gone.name);
    let layer = |desc: &ShareDesc| ShareLayer { desc: desc.clone(), opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::None, generation: 1 };
    let mut presenter = super::WindowPresenter::new_headless().expect("a presenter");
    assert!(presenter.compose_offscreen(&[layer(&good), layer(&gone)], (w, h)).is_err(), "a share image that is not there");
    let acquired = |p: &super::WindowPresenter| p.imported.get(&(good.name.clone(), false)).map(|i| i.acquired);
    assert_eq!(acquired(&presenter), Some(0), "opened, and not taken over: the frame was not drawn");
    presenter.compose_offscreen(&[layer(&good)], (w, h)).expect("the good layer alone");
    assert_eq!(acquired(&presenter), Some(1), "taken over by the frame drawn");
}

#[test]
#[ignore = "asks the host GPU"]
fn zero_the_gpu_composes_share_images_as_the_cpu_does() {
    let (w, h) = (1575u32, 890u32);
    let (surface, window, badge) = scene(w as usize, h as usize);
    let mut app = App::new();
    let s = app.share(vk::Format::R8G8B8A8_UNORM, w, h, &surface);
    let o = app.share(vk::Format::R8G8B8A8_UNORM, w, h, &window);
    let b = app.share(vk::Format::B8G8R8A8_UNORM, 64, 48, &badge);
    let layers = vec![
        ShareLayer { desc: s, opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::Premultiplied, generation: 1 },
        ShareLayer { desc: o, opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::Premultiplied, generation: 1 },
        ShareLayer { desc: b, opaque: false, crop: (8.0, 4.0, 40.0, 36.0), frame: (1500, 860, 1532, 892), blend: ShareBlend::Coverage, generation: 1 },
    ];
    let mut presenter = super::WindowPresenter::new_headless().expect("a presenter");
    assert!(presenter.can_show(&layers[0].desc), "same GPU and driver");
    let gpu = presenter.compose_offscreen(&layers, (w, h)).expect("the GPU's composition");

    let cpu_layers = [
        Layer { source: Source::Pixels { data: &surface, stride: w as usize, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
        Layer { source: Source::Pixels { data: &window, stride: w as usize, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
        Layer { source: Source::Mapped { data: &badge, stride: 64, rows: 48, opaque: false, bgra: true, crop: (8.0, 4.0, 40.0, 36.0), transform: 0 }, frame: (1500, 860, 1532, 892), blend: Blend::Coverage, alpha: 1.0 },
    ];
    let mut cpu = vec![0u8; (w * h * 4) as usize];
    compose::compose_into(&mut cpu, w as usize, h as usize, &cpu_layers, compose::Opts { fast: false, runs: false, bgra: true });
    // Colour channels: exact where nothing blends; where a layer blends, within 2 -- the GPU's float
    // blend rounds, the CPU's integer one truncates twice for a coverage layer (premultiplying,
    // then blending). MEASURED: 92,245 of 4,205,250 values differ, by at most 2. Screenshots are
    // the CPU's (composed on demand), so they are exact. (Alpha is the window's: not compared.)
    let (mut off, mut worst) = (0usize, 0i32);
    for (g, c) in gpu.chunks_exact(4).zip(cpu.chunks_exact(4)) {
        for k in 0..3 {
            let d = (i32::from(g[k]) - i32::from(c[k])).abs();
            worst = worst.max(d);
            off += usize::from(d > 0);
        }
    }
    eprintln!("zero composition 1575x890, 3 layers: {off} of {} colour values differ, by at most {worst}", w * h * 3);
    assert!(worst <= 2, "the GPU's frame is the CPU's within rounding: worst {worst}");
    // The opaque bar and the plain SurfaceView are exact.
    let px = |v: &[u8], x: u32, y: u32| v[((y * w + x) * 4) as usize..((y * w + x) * 4 + 3) as usize].to_vec();
    for (x, y) in [(10, 10), (800, 30), (800, 400), (1574, 889), (0, 889)] {
        assert_eq!(px(&gpu, x, y), px(&cpu, x, y), "pixel {x},{y}");
    }
}

impl App {
    /// GPU milliseconds of the copy `present_zero` adds to a release: a 1575x890 image into its
    /// share image (median of 20).
    fn share_copy_ms(&self, w: u32, h: u32) -> f64 {
        let d = &self.device;
        // SAFETY (the test).
        unsafe {
            let make = |usage| {
                let info = vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(usage)
                    .initial_layout(vk::ImageLayout::UNDEFINED);
                let i = d.create_image(&info, None).expect("image");
                let need = d.get_image_memory_requirements(i);
                let m = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(self.kind(need.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)), None).expect("memory");
                d.bind_image_memory(i, m, 0).expect("bind");
                (i, m)
            };
            let ((src, sm), (dst, dm)) = (make(vk::ImageUsageFlags::TRANSFER_SRC), make(vk::ImageUsageFlags::TRANSFER_DST));
            let cb = d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(self.pool).command_buffer_count(1)).expect("cb")[0];
            let fence = d.create_fence(&vk::FenceCreateInfo::default(), None).expect("fence");
            let range = super::range();
            let mut ms = Vec::new();
            for _ in 0..20 {
                d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
                let b = |image, new| vk::ImageMemoryBarrier::default().old_layout(vk::ImageLayout::UNDEFINED).new_layout(new).dst_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::TRANSFER_READ).src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED).image(image).subresource_range(range);
                d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[b(src, vk::ImageLayout::TRANSFER_SRC_OPTIMAL), b(dst, vk::ImageLayout::TRANSFER_DST_OPTIMAL)]);
                let region = vk::ImageCopy { src_subresource: super::layers(), src_offset: vk::Offset3D::default(), dst_subresource: super::layers(), dst_offset: vk::Offset3D::default(), extent: vk::Extent3D { width: w, height: h, depth: 1 } };
                d.cmd_copy_image(cb, src, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, dst, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region]);
                d.end_command_buffer(cb).expect("end");
                let cbs = [cb];
                let t = std::time::Instant::now();
                d.queue_submit(self.queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
                d.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
                d.reset_fences(&[fence]).expect("reset");
                d.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty()).expect("reset");
            }
            d.destroy_fence(fence, None);
            d.free_command_buffers(self.pool, &[cb]);
            for (i, m) in [(src, sm), (dst, dm)] {
                d.destroy_image(i, None);
                d.free_memory(m, None);
            }
            ms.sort_by(f64::total_cmp);
            ms[ms.len() / 2]
        }
    }
}

/// **What showing an in-world frame costs the system host process**, per path: a real 1575x890
/// window at 60 frames a second, the process's CPU time per frame (every thread: the composing one,
/// the window's `WM_PAINT`, the driver's), rounds interleaved, median of four. The layers are the
/// in-world frame (`scene`): the CPU paths read them from two shared-memory regions as the composer
/// does; `present_zero` from two share images another device exported by name.
///
/// - **default**: regions read into buffers, composed (`compose_fast`), copied into the
///   framebuffer, swizzled into the window's canvas, `StretchDIBits`;
/// - **compose_zero + present_bgra**: composed from the regions into the framebuffer as BGRA,
///   the canvas takes it shared, `StretchDIBits`;
/// - **present_zero**: the window's GPU composes the share images and presents; nothing per frame
///   on the CPU but the commands.
///
/// ```text
/// OMNI_GFX_WINDOW_TESTS=1 cargo test --release -p omni-linux --lib -- --ignored --nocapture zero_present_cost
/// ```
#[test]
#[ignore = "a measurement that opens a window: OMNI_GFX_WINDOW_TESTS=1"]
#[allow(clippy::too_many_lines)]
fn zero_present_cost() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use omni_platform::window::{Window, WindowDesc};

    use crate::hal::framebuffer::Framebuffer;
    use crate::shm::Shm;

    assert!(std::env::var("OMNI_GFX_WINDOW_TESTS").as_deref() == Ok("1"), "needs a desktop session: OMNI_GFX_WINDOW_TESTS=1");
    let mut window = Window::new(&WindowDesc::new("omni zero_present_cost", 1575, 890)).expect("a window");
    window.show();
    let settle = |window: &mut Window| {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            window.poll_events().for_each(drop);
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    settle(&mut window);
    let (w, h) = (1575u32, 890u32);
    let client = window.client_size().expect("its size");
    let (surface, overlay, _) = scene(w as usize, h as usize);
    let mut app = App::new();
    eprintln!("zero_present_cost: the share copy a release adds: {:.3} ms of GPU", app.share_copy_ms(w, h));
    let layers = vec![
        ShareLayer { desc: app.share(vk::Format::R8G8B8A8_UNORM, w, h, &surface), opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::Premultiplied, generation: 1 },
        ShareLayer { desc: app.share(vk::Format::R8G8B8A8_UNORM, w, h, &overlay), opaque: false, crop: (0.0, 0.0, w as f32, h as f32), frame: (0, 0, w as i32, h as i32), blend: ShareBlend::Premultiplied, generation: 1 },
    ];
    let region = |pixels: &[u8]| {
        let shm = Shm::create("zero_present_cost").expect("region");
        shm.set_len(pixels.len() as u64).expect("size");
        shm.as_graphics_buffer();
        shm.write_at(pixels, 0).expect("write");
        shm
    };
    let (rs, ro) = (region(&surface), region(&overlay));
    let need = (w * h * 4) as usize;
    let fb = Framebuffer::new(w, h);
    let presenter = window.presenter();
    let raw = window.raw();
    let (mut sa, mut so, mut out) = (Vec::new(), Vec::new(), Vec::new());
    let cpu_layers = |a: &[u8], b: &[u8], f: &mut dyn FnMut(&[Layer<'_>])| {
        let l = [
            Layer { source: Source::Pixels { data: a, stride: w as usize, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
            Layer { source: Source::Pixels { data: b, stride: w as usize, opaque: false, crop_x: 0, crop_y: 0 }, frame: (0, 0, w as i32, h as i32), blend: Blend::Premultiplied, alpha: 1.0 },
        ];
        f(&l);
    };
    const N: u32 = 180;
    let period = Duration::from_micros(16_667);
    let names = ["default", "compose_zero=1 present_bgra=1", "present_zero=1"];
    let (mut cpu, mut call): ([Vec<f64>; 3], [Vec<f64>; 3]) = Default::default();
    for round in 0..4usize {
        for k in 0..3usize {
            let path = (round + k) % 3;
            let mut gpu = (path == 2).then(|| {
                presenter.clear();
                super::WindowPresenter::new(raw).expect("a swapchain")
            });
            let mut one = |window: &mut Window| {
                let t = Instant::now();
                match path {
                    0 => {
                        sa.resize(need, 0);
                        so.resize(need, 0);
                        rs.read_at(&mut sa, 0).expect("read");
                        ro.read_at(&mut so, 0).expect("read");
                        out.resize(need, 0);
                        cpu_layers(&sa, &so, &mut |l| compose::compose_with(&mut out, w as usize, h as usize, l, true));
                        fb.present_frame(&out, w, h, w);
                        let (_, _, _, px) = fb.frame();
                        presenter.present_rgba(&px, w, h).expect("present");
                    }
                    1 => {
                        let (a, b) = (rs.bytes(0, need).expect("view"), ro.bytes(0, need).expect("view"));
                        cpu_layers(&a, &b, &mut |l| fb.present_with(w, h, true, |o| compose::compose_into(o, w as usize, h as usize, l, compose::Opts { fast: true, runs: true, bgra: true })));
                        let (_, _, _, px, _) = fb.frame_raw();
                        presenter.present_bgra(px, w, h).expect("present");
                    }
                    _ => {
                        gpu.as_mut().expect("made").present_layers(&layers, (w, h), client).expect("present_zero");
                    }
                }
                let spent = t.elapsed();
                window.poll_events().for_each(drop);
                spent
            };
            for _ in 0..10 {
                one(&mut window);
                std::thread::sleep(period);
            }
            let c0 = omni_platform::process::cpu_time().expect("cpu");
            let (start, mut calls) = (Instant::now(), Duration::ZERO);
            for i in 0..N {
                calls += one(&mut window);
                if let Some(wait) = (start + period * (i + 1)).checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
            let used = omni_platform::process::cpu_time().expect("cpu") - c0;
            cpu[path].push(used.as_secs_f64() * 1000.0 / f64::from(N));
            call[path].push(calls.as_secs_f64() * 1000.0 / f64::from(N));
            drop(gpu);
        }
    }
    let _ = Arc::new(());
    let med = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    for p in 0..3 {
        eprintln!("zero_present_cost 1575x890 2 layers, {}: process CPU {:.2} ms/frame (rounds {:.2?}); the frame's call {:.2} ms", names[p], med(&cpu[p]), cpu[p], med(&call[p]));
    }
}

/// A gralloc region laid out as the allocator lays one out (`hal::gralloc`): the metadata page,
/// then `stride` x `h` pixels, holding `desc`'s frame of `generation` only in its share image.
fn stale_region(desc: &ShareDesc, stride: u32, h: u32, generation: u64) -> std::sync::Arc<crate::shm::Shm> {
    use crate::gpu::native::{CONTENT_GENERATION_AT, PENDING_GENERATION_AT};
    let shm = crate::shm::Shm::create("region_lazy").expect("region");
    shm.set_len(crate::hal::gralloc::PIXELS_AT + u64::from(stride) * u64::from(h) * 4).expect("size");
    shm.as_graphics_buffer();
    desc.write(&shm);
    share::write_generation(&shm, generation);
    shm.write_at(&generation.to_le_bytes(), CONTENT_GENERATION_AT).expect("generation");
    shm.write_at(&generation.to_le_bytes(), PENDING_GENERATION_AT).expect("generation");
    share::set_stale(&shm, generation);
    shm
}

/// **`region_lazy`'s on-demand fill**: a region its release left stale is filled from its share
/// image (opened by name here, read back on the GPU at the region's stride), the mark cleared; one
/// whose share image holds another frame is not, and its buffer is copied again from the next
/// release on. Then **what the release worker spends a frame**, with the region copy and without:
/// the copy submitted (into staging and the share image, or the share image alone), then the
/// worker's wait for it and its copy into the region -- the waiting thread's CPU and the wall time.
#[test]
#[ignore = "asks the host GPU"]
#[allow(clippy::too_many_lines)]
fn zero_region_lazy() {
    let (w, h, stride) = (1575u32, 890u32, 1600u32);
    let (surface, _, _) = scene(w as usize, h as usize);
    let mut app = App::new();
    let desc = app.share(vk::Format::R8G8B8A8_UNORM, w, h, &surface);
    let share_image = app.made.last().expect("made").0;

    let shm = stale_region(&desc, stride, h, 7);
    let t = std::time::Instant::now();
    assert!(share::ensure_region(&shm, u64::from(stride) * 4), "filled");
    let first = t.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(share::stale(&shm), 0, "the mark cleared");
    let mut back = vec![0u8; (stride * h * 4) as usize];
    shm.read_at(&mut back, crate::hal::gralloc::PIXELS_AT).expect("read");
    for y in [0usize, 1, 445, 889] {
        let row = &back[y * stride as usize * 4..][..w as usize * 4];
        assert!(row == &surface[y * w as usize * 4..][..w as usize * 4], "row {y} is the share image's");
    }
    // Again (the share image already open here): what a screenshot of a lazy frame costs.
    share::set_stale(&shm, 7);
    let t = std::time::Instant::now();
    assert!(share::ensure_region(&shm, u64::from(stride) * 4));
    let again = t.elapsed().as_secs_f64() * 1000.0;
    eprintln!("zero_region_lazy: a stale 1575x890 region filled on demand in {first:.2} ms (the share image opened), {again:.2} ms after");
    {
        let mut p = super::WindowPresenter::new_headless().expect("presenter");
        for i in 0..4 {
            let t = std::time::Instant::now();
            let px = p.read_share(&desc, 7 + i, u64::from(stride) * 4).expect("read");
            let read = t.elapsed().as_secs_f64() * 1000.0;
            let t = std::time::Instant::now();
            shm.write_at(&px, crate::hal::gralloc::PIXELS_AT).expect("write");
            eprintln!("zero_region_lazy: read_share {read:.2} ms, write_at {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    // A share image holding another frame: not filled, and the composer's mark withdrawn.
    let other = stale_region(&desc, stride, h, 9);
    share::write_generation(&other, 8);
    share::mark_taken(&other, 9);
    assert!(!share::ensure_region(&other, u64::from(stride) * 4));
    assert_eq!(share::taken(&other), 0, "copied again from the next release");

    // The release worker's frame, each way.
    let d = &app.device;
    // SAFETY (the test).
    unsafe {
        let make_image = || {
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(vk::Format::R8G8B8A8_UNORM)
                .extent(vk::Extent3D { width: w, height: h, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::TRANSFER_SRC)
                .initial_layout(vk::ImageLayout::UNDEFINED);
            let i = d.create_image(&info, None).expect("image");
            let need = d.get_image_memory_requirements(i);
            let m = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(app.kind(need.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)), None).expect("memory");
            d.bind_image_memory(i, m, 0).expect("bind");
            (i, m)
        };
        let (frame, frame_mem) = make_image();
        let bytes = u64::from(stride) * u64::from(h) * 4;
        let staging = d.create_buffer(&vk::BufferCreateInfo::default().size(bytes).usage(vk::BufferUsageFlags::TRANSFER_DST), None).expect("buffer");
        let need = d.get_buffer_memory_requirements(staging);
        let staging_mem = d.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(app.kind(need.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED)), None).expect("memory");
        d.bind_buffer_memory(staging, staging_mem, 0).expect("bind");
        let mapped = d.map_memory(staging_mem, 0, bytes, vk::MemoryMapFlags::empty()).expect("map").cast::<u8>();
        let cb = d.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(app.pool).command_buffer_count(1)).expect("cb")[0];
        let fence = d.create_fence(&vk::FenceCreateInfo::default(), None).expect("fence");
        let range = super::range();
        let me = omni_platform::sampler::HostThread::current().expect("this thread");
        let per_ms = {
            let (t, c) = (std::time::Instant::now(), me.cycles().expect("cycles"));
            while t.elapsed() < std::time::Duration::from_millis(100) {
                std::hint::spin_loop();
            }
            (me.cycles().expect("cycles") - c) as f64 / (t.elapsed().as_secs_f64() * 1000.0)
        };
        let region = stale_region(&desc, stride, h, 1);
        let once = |lazy: bool| -> (f64, f64) {
            d.reset_command_buffer(cb, vk::CommandBufferResetFlags::empty()).expect("reset");
            d.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
            let b = |image, old, new| vk::ImageMemoryBarrier::default().old_layout(old).new_layout(new).dst_access_mask(vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE).src_queue_family_index(vk::QUEUE_FAMILY_IGNORED).dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED).image(image).subresource_range(range);
            d.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[], &[], &[b(frame, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_SRC_OPTIMAL), b(share_image, vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL)]);
            if !lazy {
                let copy = vk::BufferImageCopy::default().buffer_row_length(stride).image_subresource(super::layers()).image_extent(vk::Extent3D { width: w, height: h, depth: 1 });
                d.cmd_copy_image_to_buffer(cb, frame, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, staging, &[copy]);
            }
            let region_copy = vk::ImageCopy { src_subresource: super::layers(), src_offset: vk::Offset3D::default(), dst_subresource: super::layers(), dst_offset: vk::Offset3D::default(), extent: vk::Extent3D { width: w, height: h, depth: 1 } };
            d.cmd_copy_image(cb, frame, vk::ImageLayout::TRANSFER_SRC_OPTIMAL, share_image, vk::ImageLayout::TRANSFER_DST_OPTIMAL, &[region_copy]);
            d.end_command_buffer(cb).expect("end");
            let cbs = [cb];
            d.reset_fences(&[fence]).expect("reset");
            let t = std::time::Instant::now();
            d.queue_submit(app.queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
            // The worker: the wait, and the copy into the region unless skipped.
            let c0 = me.cycles().expect("cycles");
            d.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
            if !lazy {
                region.write_at(std::slice::from_raw_parts(mapped, bytes as usize), crate::hal::gralloc::PIXELS_AT).expect("land");
            }
            let cpu = (me.cycles().expect("cycles") - c0) as f64 / per_ms;
            (cpu, t.elapsed().as_secs_f64() * 1000.0)
        };
        let (mut cpu, mut wall): ([Vec<f64>; 2], [Vec<f64>; 2]) = Default::default();
        for i in 0..80 {
            for lazy in if i % 2 == 0 { [false, true] } else { [true, false] } {
                let (c, t) = once(lazy);
                if i >= 5 {
                    cpu[usize::from(lazy)].push(c);
                    wall[usize::from(lazy)].push(t);
                }
            }
        }
        let med = |v: &mut Vec<f64>| {
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        eprintln!(
            "zero_region_lazy release worker, 1575x890: region copied -- CPU {:.3} ms, submit to landed {:.2} ms; region_lazy -- CPU {:.3} ms, {:.2} ms",
            med(&mut cpu[0]),
            med(&mut wall[0]),
            med(&mut cpu[1]),
            med(&mut wall[1])
        );
        d.destroy_fence(fence, None);
        d.free_command_buffers(app.pool, &[cb]);
        d.unmap_memory(staging_mem);
        d.destroy_buffer(staging, None);
        d.free_memory(staging_mem, None);
        d.destroy_image(frame, None);
        d.free_memory(frame_mem, None);
    }
}
