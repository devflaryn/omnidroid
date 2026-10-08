//! **Which Vulkan calls make the host driver commit write-combined memory** (protect `0x404`,
//! `PAGE_READWRITE | PAGE_WRITECOMBINE`): a measurement on this host's GPU, straight through the
//! host's loader, no guest. The game's host process held ~401 MiB of it in ~19 blocks of ~30 MiB
//! (`wsscan.ps1`, PS99 in-world, 2026-10-09), ~83 MiB resident. Each step below prints how much
//! private committed memory of each kind it added. `cargo test --release -p omni-linux --test
//! vk_wc_census -- --ignored --nocapture`.
#![cfg(target_os = "windows")]

use ash::vk;
use std::ffi::c_void;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Mbi {
    base: usize,
    alloc_base: usize,
    alloc_protect: u32,
    partition: u16,
    size: usize,
    state: u32,
    protect: u32,
    kind: u32,
}

extern "system" {
    fn VirtualQuery(address: *const c_void, info: *mut Mbi, len: usize) -> usize;
}

/// (write-combined committed private bytes, their regions, other committed private bytes).
fn census() -> (usize, usize, usize) {
    let (mut wc, mut wc_regions, mut other) = (0, 0, 0);
    let mut at = 0usize;
    loop {
        let mut m = Mbi::default();
        // SAFETY: `m` is writable and its size passed; nothing at `at` is read.
        if unsafe { VirtualQuery(at as *const c_void, &mut m, std::mem::size_of::<Mbi>()) } == 0 {
            break;
        }
        if m.state == 0x1000 && m.kind == 0x2_0000 {
            if m.protect & 0x400 != 0 {
                wc += m.size;
                wc_regions += 1;
            } else {
                other += m.size;
            }
        }
        let next = m.base + m.size;
        if next <= at {
            break;
        }
        at = next;
    }
    (wc, wc_regions, other)
}

struct Step((usize, usize, usize));

impl Step {
    fn new() -> Self {
        Self(census())
    }
    fn say(&mut self, what: &str) {
        let now = census();
        let mib = |b: usize| b as f64 / (1 << 20) as f64;
        eprintln!(
            "{what:<58} WC {:+8.2} MiB ({:+3} regions; {:7.2} MiB in {:3})  other {:+8.2} MiB",
            mib(now.0) - mib(self.0 .0),
            now.1 as i64 - self.0 .1 as i64,
            mib(now.0),
            now.1,
            mib(now.2) - mib(self.0 .2)
        );
        self.0 = now;
    }
}

#[test]
#[ignore = "a measurement of the host GPU driver, not a test"]
fn which_calls_commit_write_combined_memory() {
    let mut s = Step::new();
    // SAFETY: loading the system's Vulkan loader.
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }.expect("instance");
    s.say("vkCreateInstance");
    let pd = unsafe { instance.enumerate_physical_devices() }.expect("devices")[0];
    let props = unsafe { instance.get_physical_device_properties(pd) };
    eprintln!("device: {}", props.device_name_as_c_str().unwrap_or_default().to_string_lossy());
    let memory = unsafe { instance.get_physical_device_memory_properties(pd) };
    for i in 0..memory.memory_type_count as usize {
        let t = memory.memory_types[i];
        eprintln!("  memory type {i}: heap {} flags {:#x}", t.heap_index, t.property_flags.as_raw());
    }
    let family = unsafe { instance.get_physical_device_queue_family_properties(pd) }
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .expect("a graphics queue") as u32;
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
    let device = unsafe { instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&qci), None) }.expect("device");
    s.say("vkCreateDevice (one graphics queue)");
    let queue = unsafe { device.get_device_queue(family, 0) };

    // Command pools and buffers, as an engine records them: a pool, buffers, many commands.
    let mut pools = Vec::new();
    let mut record = |device: &ash::Device, n_buffers: u32, commands: u32, flags: vk::CommandPoolCreateFlags| {
        let pool = unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family).flags(flags), None) }.unwrap();
        let bufs = unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(n_buffers)) }.unwrap();
        for &b in &bufs {
            unsafe { device.begin_command_buffer(b, &vk::CommandBufferBeginInfo::default()) }.unwrap();
            for i in 0..commands {
                let vp = [vk::Viewport { x: 0.0, y: 0.0, width: 64.0 + (i % 7) as f32, height: 64.0, min_depth: 0.0, max_depth: 1.0 }];
                unsafe { device.cmd_set_viewport(b, 0, &vp) };
            }
            unsafe { device.end_command_buffer(b) }.unwrap();
        }
        pools.push(pool);
        bufs
    };
    let _ = record(&device, 1, 1, vk::CommandPoolCreateFlags::empty());
    s.say("1 pool, 1 buffer, 1 command");
    let _ = record(&device, 1, 100_000, vk::CommandPoolCreateFlags::empty());
    s.say("1 pool, 1 buffer, 100k commands");
    for _ in 0..8 {
        let _ = record(&device, 3, 20_000, vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
    }
    s.say("8 more pools, 3 buffers x 20k commands each");
    let big = *pools.last().unwrap();
    unsafe { device.reset_command_pool(big, vk::CommandPoolResetFlags::empty()) }.unwrap();
    s.say("vkResetCommandPool (no flags) of the last");
    unsafe { device.reset_command_pool(big, vk::CommandPoolResetFlags::RELEASE_RESOURCES) }.unwrap();
    s.say("vkResetCommandPool(RELEASE_RESOURCES) of the last");
    for &p in &pools {
        unsafe { device.trim_command_pool(p, vk::CommandPoolTrimFlags::empty()) };
    }
    s.say("vkTrimCommandPool of all (buffers still allocated)");
    for &p in &pools {
        unsafe { device.reset_command_pool(p, vk::CommandPoolResetFlags::RELEASE_RESOURCES) }.unwrap();
    }
    s.say("vkResetCommandPool(RELEASE_RESOURCES) of all");
    for p in pools.drain(..) {
        unsafe { device.destroy_command_pool(p, None) };
    }
    s.say("vkDestroyCommandPool of all");

    // A submission.
    let pool = unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family), None) }.unwrap();
    let cb = unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)) }.unwrap();
    unsafe { device.begin_command_buffer(cb[0], &vk::CommandBufferBeginInfo::default()) }.unwrap();
    unsafe { device.end_command_buffer(cb[0]) }.unwrap();
    let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }.unwrap();
    unsafe { device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cb)], fence) }.unwrap();
    unsafe { device.wait_for_fences(&[fence], true, u64::MAX) }.unwrap();
    s.say("first vkQueueSubmit + wait");

    // Descriptor pools.
    let sizes = [
        vk::DescriptorPoolSize { ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER, descriptor_count: 4096 },
        vk::DescriptorPoolSize { ty: vk::DescriptorType::UNIFORM_BUFFER, descriptor_count: 4096 },
        vk::DescriptorPoolSize { ty: vk::DescriptorType::UNIFORM_BUFFER_DYNAMIC, descriptor_count: 4096 },
    ];
    let dp = unsafe { device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(4096).pool_sizes(&sizes), None) }.unwrap();
    s.say("vkCreateDescriptorPool (4096 sets, 3x4096 descriptors)");
    let mut dps = Vec::new();
    for _ in 0..16 {
        dps.push(unsafe { device.create_descriptor_pool(&vk::DescriptorPoolCreateInfo::default().max_sets(1024).pool_sizes(&sizes[..1]), None) }.unwrap());
    }
    s.say("16 more descriptor pools (1024 sets each)");

    // Memory of each host-visible type, mapped.
    for i in 0..memory.memory_type_count as usize {
        let f = memory.memory_types[i].property_flags;
        if !f.contains(vk::MemoryPropertyFlags::HOST_VISIBLE) {
            continue;
        }
        for size in [64u64 << 10, 4 << 20] {
            let m = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(size).memory_type_index(i as u32), None) };
            let Ok(m) = m else { continue };
            s.say(&format!("vkAllocateMemory {} KiB, type {i}", size >> 10));
            let p = unsafe { device.map_memory(m, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }.unwrap();
            s.say(&format!("  vkMapMemory it (flags {:#x})", f.as_raw()));
            let _ = p;
        }
    }
    // Device-local memory.
    let local = (0..memory.memory_type_count as usize).find(|&i| memory.memory_types[i].property_flags == vk::MemoryPropertyFlags::DEVICE_LOCAL).unwrap() as u32;
    let _m = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(64 << 20).memory_type_index(local), None) }.unwrap();
    s.say("vkAllocateMemory 64 MiB DEVICE_LOCAL");
    let pc = unsafe { device.create_pipeline_cache(&vk::PipelineCacheCreateInfo::default(), None) }.unwrap();
    s.say("vkCreatePipelineCache");
    let _ = (dp, pc);
    unsafe { device.device_wait_idle() }.unwrap();
}

extern "system" {
    fn K32QueryWorkingSetEx(process: *mut c_void, info: *mut [usize; 2], len: u32) -> i32;
    fn GetCurrentProcess() -> *mut c_void;
}

/// Resident bytes of the write-combined committed private regions.
fn wc_resident() -> usize {
    let mut pages = Vec::new();
    let mut at = 0usize;
    loop {
        let mut m = Mbi::default();
        // SAFETY: as in `census`.
        if unsafe { VirtualQuery(at as *const c_void, &mut m, std::mem::size_of::<Mbi>()) } == 0 {
            break;
        }
        if m.state == 0x1000 && m.kind == 0x2_0000 && m.protect & 0x400 != 0 {
            pages.extend((0..m.size / 4096).map(|i| [m.base + i * 4096, 0usize]));
        }
        let next = m.base + m.size;
        if next <= at {
            break;
        }
        at = next;
    }
    if pages.is_empty() {
        return 0;
    }
    // SAFETY: `pages` is writable; the call only queries.
    unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), pages.as_mut_ptr(), (pages.len() * 16) as u32) };
    pages.iter().filter(|e| e[1] & 1 != 0).count() * 4096
}

/// Device-local memory: what each way of allocating it commits and keeps resident in WC memory.
#[test]
#[ignore = "a measurement of the host GPU driver, not a test"]
fn device_local_memory_and_its_write_combined_shadow() {
    let entry = unsafe { ash::Entry::load() }.expect("a Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_1);
    let instance = unsafe { entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None) }.expect("instance");
    let pd = unsafe { instance.enumerate_physical_devices() }.expect("devices")[0];
    let memory = unsafe { instance.get_physical_device_memory_properties(pd) };
    let exts: Vec<String> = unsafe { instance.enumerate_device_extension_properties(pd) }
        .unwrap()
        .iter()
        .map(|e| e.extension_name_as_c_str().unwrap().to_string_lossy().into_owned())
        .collect();
    let pageable = exts.iter().any(|e| e == "VK_EXT_pageable_device_local_memory") && exts.iter().any(|e| e == "VK_EXT_memory_priority");
    eprintln!("pageable_device_local_memory + memory_priority: {pageable}");
    let family = unsafe { instance.get_physical_device_queue_family_properties(pd) }
        .iter()
        .position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS))
        .unwrap() as u32;
    let prio = [1.0f32];
    let qci = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
    let names = [c"VK_EXT_memory_priority".as_ptr(), c"VK_EXT_pageable_device_local_memory".as_ptr()];
    let mut pf = vk::PhysicalDevicePageableDeviceLocalMemoryFeaturesEXT::default().pageable_device_local_memory(true);
    let mut dci = vk::DeviceCreateInfo::default().queue_create_infos(&qci);
    if pageable {
        dci = dci.enabled_extension_names(&names).push_next(&mut pf);
    }
    let device = unsafe { instance.create_device(pd, &dci, None) }.expect("device");
    let queue = unsafe { device.get_device_queue(family, 0) };
    let local = (0..memory.memory_type_count as usize).find(|&i| memory.memory_types[i].property_flags == vk::MemoryPropertyFlags::DEVICE_LOCAL).unwrap() as u32;
    let mut s = Step::new();
    let say = |s: &mut Step, what: &str| {
        s.say(what);
        eprintln!("{:<58} WC resident {:7.2} MiB", "", wc_resident() as f64 / (1 << 20) as f64);
    };
    say(&mut s, "device made");
    let mut keep = Vec::new();
    for _ in 0..16 {
        keep.push(unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(1 << 20).memory_type_index(local), None) }.unwrap());
    }
    say(&mut s, "16 x vkAllocateMemory 1 MiB DEVICE_LOCAL");
    let big = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(64 << 20).memory_type_index(local), None) }.unwrap();
    say(&mut s, "vkAllocateMemory 64 MiB DEVICE_LOCAL");
    // Written by the GPU.
    let buf = unsafe {
        device.create_buffer(&vk::BufferCreateInfo::default().size(64 << 20).usage(vk::BufferUsageFlags::TRANSFER_DST), None)
    }
    .unwrap();
    unsafe { device.bind_buffer_memory(buf, big, 0) }.unwrap();
    let pool = unsafe { device.create_command_pool(&vk::CommandPoolCreateInfo::default().queue_family_index(family), None) }.unwrap();
    let cb = unsafe { device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)) }.unwrap();
    unsafe {
        device.begin_command_buffer(cb[0], &vk::CommandBufferBeginInfo::default()).unwrap();
        device.cmd_fill_buffer(cb[0], buf, 0, vk::WHOLE_SIZE, 0x5555_5555);
        device.end_command_buffer(cb[0]).unwrap();
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).unwrap();
        device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cb)], fence).unwrap();
        device.wait_for_fences(&[fence], true, u64::MAX).unwrap();
    }
    say(&mut s, "  filled by the GPU (vkCmdFillBuffer 64 MiB)");
    // A dedicated image.
    let ici = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
        .extent(vk::Extent3D { width: 2048, height: 2048, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST);
    let image = unsafe { device.create_image(&ici, None) }.unwrap();
    let req = unsafe { device.get_image_memory_requirements(image) };
    let mut ded = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let im = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(req.size).memory_type_index(local).push_next(&mut ded), None) }.unwrap();
    unsafe { device.bind_image_memory(image, im, 0) }.unwrap();
    say(&mut s, &format!("dedicated image 2048x2048 RGBA ({} MiB)", req.size >> 20));
    if pageable {
        let mut p = vk::MemoryPriorityAllocateInfoEXT::default().priority(0.0);
        let low = unsafe { device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(64 << 20).memory_type_index(local).push_next(&mut p), None) }.unwrap();
        say(&mut s, "vkAllocateMemory 64 MiB DEVICE_LOCAL, priority 0 (pageable device)");
        keep.push(low);
    }
    for m in keep.drain(..) {
        unsafe { device.free_memory(m, None) };
    }
    say(&mut s, "freed the 1 MiB ones (and the low-priority one)");
    unsafe {
        device.destroy_buffer(buf, None);
        device.free_memory(big, None);
    }
    say(&mut s, "freed the 64 MiB one");
}
