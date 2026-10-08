//! **How a host thread should wait for the GPU** (`release_wait`), measured on the host GPU: the
//! same submitted work (a few milliseconds of buffer copies, about what a release's fence covers in
//! a world) waited for each way, interleaved, the process's CPU time per wait and the wall time from
//! the submit to the waiter's return. A measurement, not a check:
//!
//! ```text
//! cargo test --release -p omni-linux --lib -- --ignored --nocapture release_wait_cost
//! ```
use ash::vk;

// `WaitForSingleObject` -- only for the probe of whether an exported fence handle is waitable;
// the runtime goes through `omni_platform` (Global Constraint 4).
#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
    fn CloseHandle(handle: isize) -> i32;
    fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
    fn GetCurrentThread() -> isize;
}

/// This thread's CPU cycles so far (exact, unlike `GetThreadTimes`' 15.6 ms ticks).
#[cfg(windows)]
fn thread_cycles() -> u64 {
    let mut c = 0u64;
    // SAFETY: the pseudo-handle of this thread; writes one u64.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}
#[cfg(not(windows))]
fn thread_cycles() -> u64 {
    0
}

/// Cycles a millisecond of this thread running flat out.
fn cycles_per_ms() -> f64 {
    let (t, c) = (std::time::Instant::now(), thread_cycles());
    while t.elapsed() < std::time::Duration::from_millis(100) {
        std::hint::spin_loop();
    }
    (thread_cycles() - c) as f64 / (t.elapsed().as_secs_f64() * 1000.0)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum How {
    Spin,
    Poll(u64),
    Timeline,
    Event,
}

#[test]
#[ignore = "asks the host GPU"]
#[allow(clippy::too_many_lines)]
fn release_wait_cost() {
    let entry = super::super::entry().expect("a Vulkan loader");
    let app = vk::ApplicationInfo::default().api_version(vk::make_api_version(0, 1, 2, 0));
    // SAFETY (the test): every handle is made here, used on this thread, destroyed at the end.
    unsafe {
        let instance = entry.create_instance(&vk::InstanceCreateInfo::default().application_info(&app), None).expect("instance");
        let pd = instance
            .enumerate_physical_devices()
            .expect("devices")
            .into_iter()
            .max_by_key(|&pd| u8::from(instance.get_physical_device_properties(pd).device_type == vk::PhysicalDeviceType::DISCRETE_GPU))
            .expect("a device");
        let family = instance.get_physical_device_queue_family_properties(pd).iter().position(|f| f.queue_flags.contains(vk::QueueFlags::GRAPHICS)).expect("a queue") as u32;
        let offered = instance.enumerate_device_extension_properties(pd).expect("extensions");
        let has = |n: &std::ffi::CStr| offered.iter().any(|e| e.extension_name_as_c_str().is_ok_and(|x| x == n));
        let event_ok = cfg!(windows) && has(ash::khr::external_fence_win32::NAME);
        let mut exts = Vec::new();
        if event_ok {
            exts.push(ash::khr::external_fence_win32::NAME.as_ptr());
        }
        let prio = [1.0f32];
        let queues = [vk::DeviceQueueCreateInfo::default().queue_family_index(family).queue_priorities(&prio)];
        let mut v12 = vk::PhysicalDeviceVulkan12Features::default().timeline_semaphore(true);
        let device = instance.create_device(pd, &vk::DeviceCreateInfo::default().queue_create_infos(&queues).enabled_extension_names(&exts).push_next(&mut v12), None).expect("device");
        let queue = device.get_device_queue(family, 0);

        // The work: copies between two device-local buffers, sized for ~3 ms on this GPU.
        let mem = instance.get_physical_device_memory_properties(pd);
        let size: u64 = 64 << 20;
        let make = || {
            let b = device.create_buffer(&vk::BufferCreateInfo::default().size(size).usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST), None).expect("buffer");
            let need = device.get_buffer_memory_requirements(b);
            let kind = (0..mem.memory_type_count).find(|&i| need.memory_type_bits & (1 << i) != 0 && mem.memory_types[i as usize].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)).expect("device-local");
            let m = device.allocate_memory(&vk::MemoryAllocateInfo::default().allocation_size(need.size).memory_type_index(kind), None).expect("memory");
            device.bind_buffer_memory(b, m, 0).expect("bind");
            (b, m)
        };
        let ((a, am), (b, bm)) = (make(), make());
        let pool = device.create_command_pool(&vk::CommandPoolCreateInfo::default().flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER).queue_family_index(family), None).expect("pool");
        let cb = device.allocate_command_buffers(&vk::CommandBufferAllocateInfo::default().command_pool(pool).command_buffer_count(1)).expect("cb")[0];
        let record = |copies: u32| {
            device.begin_command_buffer(cb, &vk::CommandBufferBeginInfo::default()).expect("begin");
            let barrier = vk::MemoryBarrier::default().src_access_mask(vk::AccessFlags::TRANSFER_WRITE).dst_access_mask(vk::AccessFlags::TRANSFER_READ | vk::AccessFlags::TRANSFER_WRITE);
            for i in 0..copies {
                let (src, dst) = if i % 2 == 0 { (a, b) } else { (b, a) };
                device.cmd_copy_buffer(cb, src, dst, &[vk::BufferCopy { src_offset: 0, dst_offset: 0, size }]);
                device.cmd_pipeline_barrier(cb, vk::PipelineStageFlags::TRANSFER, vk::PipelineStageFlags::TRANSFER, vk::DependencyFlags::empty(), &[barrier], &[], &[]);
            }
            device.end_command_buffer(cb).expect("end");
        };
        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None).expect("fence");
        let mut export = vk::ExportFenceCreateInfo::default().handle_types(vk::ExternalFenceHandleTypeFlags::OPAQUE_WIN32);
        let event_fence = if event_ok { device.create_fence(&vk::FenceCreateInfo::default().push_next(&mut export), None).ok() } else { None };
        let mut kind = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(0);
        let timeline = device.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut kind), None).expect("timeline");
        let mut value = 0u64;
        let win32 = event_fence.map(|_| ash::khr::external_fence_win32::Device::new(&instance, &device));
        let cbs = [cb];

        // One wait: submit, wait the way asked, answer (wall ms) -- or None when that way failed.
        let mut once = |how: How| -> Option<f64> {
            let t = std::time::Instant::now();
            match how {
                How::Spin | How::Poll(_) => {
                    device.reset_fences(&[fence]).expect("reset");
                    device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], fence).expect("submit");
                    if let How::Poll(us) = how {
                        while device.get_fence_status(fence) == Ok(false) {
                            std::thread::sleep(std::time::Duration::from_micros(us));
                        }
                    }
                    device.wait_for_fences(&[fence], true, u64::MAX).expect("wait");
                }
                How::Timeline => {
                    value += 1;
                    let values = [value];
                    let sems = [timeline];
                    let mut tl = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&values);
                    device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs).signal_semaphores(&sems).push_next(&mut tl)], vk::Fence::null()).expect("submit");
                    device.wait_semaphores(&vk::SemaphoreWaitInfo::default().semaphores(&sems).values(&values), u64::MAX).expect("wait");
                }
                How::Event => {
                    let f = event_fence?;
                    device.reset_fences(&[f]).expect("reset");
                    device.queue_submit(queue, &[vk::SubmitInfo::default().command_buffers(&cbs)], f).expect("submit");
                    #[cfg(windows)]
                    {
                        let info = vk::FenceGetWin32HandleInfoKHR::default().fence(f).handle_type(vk::ExternalFenceHandleTypeFlags::OPAQUE_WIN32);
                        let handle = win32.as_ref()?.get_fence_win32_handle(&info).ok()?;
                        let r = WaitForSingleObject(handle as isize, 10_000);
                        CloseHandle(handle as isize);
                        if r != 0 {
                            eprintln!("release_wait_cost: WaitForSingleObject on an exported fence answered {r:#x}");
                            device.wait_for_fences(&[f], true, u64::MAX).expect("wait");
                            return None;
                        }
                        // Signalled by the handle: the fence must agree, or the handle is not the
                        // fence's event (MEASURED on NVIDIA 591.86: it is not -- signalled at once).
                        if device.get_fence_status(f) != Ok(true) {
                            eprintln!("release_wait_cost: an exported fence's handle was signalled before its fence: not a wait for the GPU");
                            device.wait_for_fences(&[f], true, u64::MAX).expect("wait");
                            return None;
                        }
                    }
                }
            }
            Some(t.elapsed().as_secs_f64() * 1000.0)
        };

        // Calibrate the work to ~3 ms of GPU time (warm first: the first submits are slow).
        let mut copies = 2;
        record(copies);
        for _ in 0..10 {
            once(How::Spin);
        }
        for _ in 0..8 {
            let mut v: Vec<f64> = (0..5).map(|_| once(How::Spin).expect("spin")).collect();
            v.sort_by(f64::total_cmp);
            if v[2] > 2.5 {
                break;
            }
            copies *= 2;
            record(copies);
        }
        let per_ms = cycles_per_ms();
        let ways = [How::Spin, How::Poll(50), How::Poll(100), How::Poll(250), How::Poll(500), How::Timeline, How::Event];
        let (mut cpu, mut wall, mut thread): (Vec<Vec<f64>>, Vec<Vec<f64>>, Vec<Vec<f64>>) = (vec![Vec::new(); ways.len()], vec![Vec::new(); ways.len()], vec![Vec::new(); ways.len()]);
        let mut failed = vec![false; ways.len()];
        const PER: u32 = 100;
        for round in 0..6 {
            for k in 0..ways.len() {
                let w = (round + k) % ways.len();
                if failed[w] {
                    continue;
                }
                let c0 = omni_platform::process::cpu_time().expect("cpu");
                let k0 = thread_cycles();
                let mut walls = Vec::new();
                for _ in 0..PER {
                    match once(ways[w]) {
                        Some(ms) => walls.push(ms),
                        None => {
                            failed[w] = true;
                            break;
                        }
                    }
                }
                if failed[w] {
                    continue;
                }
                let used = omni_platform::process::cpu_time().expect("cpu") - c0;
                thread[w].push((thread_cycles() - k0) as f64 / per_ms / f64::from(PER));
                cpu[w].push(used.as_secs_f64() * 1000.0 / f64::from(PER));
                walls.sort_by(f64::total_cmp);
                wall[w].push(walls[walls.len() / 2]);
            }
        }
        let med = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v.get(v.len() / 2).copied().unwrap_or(f64::NAN)
        };
        let spin_wall = med(&wall[0]);
        eprintln!("release_wait_cost: {copies} copies of 64 MiB a wait; event (exported fence) {}", if event_ok { "offered" } else { "not offered" });
        for (i, w) in ways.iter().enumerate() {
            if failed[i] {
                eprintln!("release_wait_cost {w:?}: not usable here");
                continue;
            }
            eprintln!(
                "release_wait_cost {w:?}: waiting thread CPU {:.3} ms a wait; process CPU {:.2} ms a wait; wall {:.2} ms (+{:.2} over spin)",
                med(&thread[i]),
                med(&cpu[i]),
                med(&wall[i]),
                med(&wall[i]) - spin_wall
            );
        }

        device.destroy_semaphore(timeline, None);
        if let Some(f) = event_fence {
            device.destroy_fence(f, None);
        }
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        for (bb, m) in [(a, am), (b, bm)] {
            device.destroy_buffer(bb, None);
            device.free_memory(m, None);
        }
        device.destroy_device(None);
        instance.destroy_instance(None);
    }
}
