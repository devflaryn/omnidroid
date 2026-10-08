//! What one forwarded Vulkan command costs on the host's side of `/dev/omni-gpu`, measured: the
//! same cheap command (`vkCmdSetViewport` into a recording command buffer) called N times straight
//! on the host driver, then N times as the guest's driver sends it -- `ioctl(OMNI_GPU_CALL)` through
//! the syscall table, the descriptor lookup, the argument copies, the handle lookup, the result
//! write -- and N times batched (`OMNI_VK_ID_BATCH`, one ioctl per many records). The difference is
//! the forwarding's own cost (what a render thread pays per command besides the driver).
//!
//! Not in it: the guest's half (its stub, bionic's `ioctl`, dynarmic's `svc` thunk); see
//! `d3a_gpu.rs`'s `vkbench` for the whole path.
//!
//! `cargo test --release -p omni-linux --test gpu_call_cost -- --nocapture` prints the numbers;
//! it needs a host Vulkan GPU (it passes, saying so, without one).
use omni_linux::fd::Output;
use omni_linux::gpu::{command_id, OMNI_GPU_CALL};
use omni_linux::process::{Process, Task};
use omni_linux::syscall::nr;
use omni_linux::{manifest, vfs::{Sysroot, Vfs}};
use std::sync::Arc;
use std::time::Instant;

/// The forwarded-call ABI as the guest's driver speaks it: a 32-byte request at `call`, the
/// arguments at `args`.
struct Guest {
    p: Arc<Process>,
    t: Task,
    fd: u64,
    call: u64,
    args: u64,
}

impl Guest {
    fn call(&mut self, name: &str, args: &[u64]) -> (i64, u64) {
        let id = command_id(name).unwrap_or_else(|| panic!("{name} is forwarded"));
        self.raw(id, args)
    }

    fn raw(&mut self, id: u32, args: &[u64]) -> (i64, u64) {
        let bytes: Vec<u8> = args.iter().flat_map(|a| a.to_le_bytes()).collect();
        self.p.mem.write(self.args, &bytes).unwrap();
        let mut c = Vec::with_capacity(32);
        c.extend_from_slice(&id.to_le_bytes());
        c.extend_from_slice(&(args.len() as u32).to_le_bytes());
        c.extend_from_slice(&self.args.to_le_bytes());
        c.extend_from_slice(&[0u8; 16]);
        self.p.mem.write(self.call, &c).unwrap();
        let r = self.p.syscall(&mut self.t, nr::IOCTL, [self.fd, OMNI_GPU_CALL, self.call, 0, 0, 0]) as i64;
        (r, self.p.mem.read_u64(self.call + 16).unwrap())
    }

    fn ok(&mut self, name: &str, args: &[u64]) -> u64 {
        let (r, v) = self.call(name, args);
        assert_eq!(r, 0, "{name}: the ioctl");
        v
    }
}

/// Guest memory laid out by hand: `at(n)` is byte `n` of the scratch area.
struct Layout(u64);
impl Layout {
    fn at(&self, n: u64) -> u64 {
        self.0 + n
    }
}

fn wr32(p: &Process, at: u64, v: u32) {
    p.mem.write(at, &v.to_le_bytes()).unwrap();
}
fn wr64(p: &Process, at: u64, v: u64) {
    p.mem.write(at, &v.to_le_bytes()).unwrap();
}

#[test]
fn a_forwarded_command_costs_little_more_than_the_driver() {
    let m = manifest::parse("d\t755\t/\n").unwrap();
    let vfs = Vfs::new(Sysroot::from_manifest(&std::env::temp_dir(), m), vec![], b"/x".to_vec());
    let p = Process::for_tests(vfs, Output::Capture(Default::default()));
    let t = p.test_task();
    let s = Layout(p.scratch());
    // The host driver reads and writes guest memory straight: commit the pages first.
    p.mem.write(s.at(0), &vec![0u8; 256 * 1024]).unwrap();
    p.mem.write(s.at(0), b"/dev/omni-gpu\0").unwrap();
    let mut t = t;
    let fd = p.syscall(&mut t, nr::OPENAT, [(-100i64) as u64, s.at(0), 2, 0, 0, 0]);
    assert!((fd as i64) >= 0, "open /dev/omni-gpu");
    let mut g = Guest { p: Arc::clone(&p), t, fd, call: s.at(0x100), args: s.at(0x200) };

    // vkCreateInstance(&ici, NULL, &host): a bare VkInstanceCreateInfo (sType 1).
    let (ici, out) = (s.at(0x1000), s.at(0x1100));
    wr32(&p, ici, 1);
    let (r, v) = g.call("vkCreateInstance", &[ici, 0, out]);
    if r != 0 || v != 0 {
        eprintln!("no host Vulkan here ({r}, {v}): nothing measured");
        return;
    }
    let instance = p.mem.read_u64(out).unwrap();
    // Wrappers as the guest's driver makes them: { dispatch, host }.
    let wrap = |at: u64, host: u64| {
        wr64(&p, at, 0);
        wr64(&p, at + 8, host);
        at
    };
    let instance_w = wrap(s.at(0x2000), instance);
    wr32(&p, s.at(0x1200), 1);
    assert_eq!(g.ok("vkEnumeratePhysicalDevices", &[instance_w, s.at(0x1200), s.at(0x1208)]) as i32, 0);
    let pd = p.mem.read_u64(s.at(0x1208)).unwrap();
    let pd_w = wrap(s.at(0x2010), pd);

    // vkCreateDevice: one queue of family 0 (graphics on every desktop GPU).
    let (dci, qci, prio) = (s.at(0x3000), s.at(0x3100), s.at(0x3200));
    p.mem.write(prio, &1.0f32.to_le_bytes()).unwrap();
    wr32(&p, qci, 2);
    wr32(&p, qci + 20, 0);
    wr32(&p, qci + 24, 1);
    wr64(&p, qci + 32, prio);
    wr32(&p, dci, 3);
    wr32(&p, dci + 20, 1);
    wr64(&p, dci + 24, qci);
    assert_eq!(g.ok("vkCreateDevice", &[pd_w, dci, 0, s.at(0x1300)]) as i32, 0);
    let device = p.mem.read_u64(s.at(0x1300)).unwrap();
    let device_w = wrap(s.at(0x2020), device);

    // A pool whose buffers reset one by one, and one primary command buffer.
    let cpci = s.at(0x3300);
    wr32(&p, cpci, 39);
    wr32(&p, cpci + 16, 2);
    assert_eq!(g.ok("vkCreateCommandPool", &[device_w, cpci, 0, s.at(0x1400)]) as i32, 0);
    let pool = p.mem.read_u64(s.at(0x1400)).unwrap();
    let cbai = s.at(0x3400);
    wr32(&p, cbai, 40);
    wr64(&p, cbai + 16, pool);
    wr32(&p, cbai + 28, 1);
    assert_eq!(g.ok("vkAllocateCommandBuffers", &[device_w, cbai, s.at(0x1500)]) as i32, 0);
    let cb = p.mem.read_u64(s.at(0x1500)).unwrap();
    let cb_w = wrap(s.at(0x2030), cb);
    let begin = s.at(0x3500);
    wr32(&p, begin, 42);
    wr32(&p, begin + 16, 1);

    // One viewport.
    let vp = s.at(0x3600);
    let floats: Vec<u8> = [0.0f32, 0.0, 64.0, 64.0, 0.0, 1.0].iter().flat_map(|f| f.to_le_bytes()).collect();
    p.mem.write(vp, &floats).unwrap();

    const N: u32 = 200_000;
    const PER_BUFFER: u32 = 5_000;

    // 1. The host driver, straight.
    let entry = unsafe { ash::Entry::load() }.expect("the host's Vulkan loader");
    let gipa = entry.static_fn().get_instance_proc_addr;
    // SAFETY: Vulkan signatures; the handles are the live host objects made above.
    let gdpa: ash::vk::PFN_vkGetDeviceProcAddr = unsafe {
        std::mem::transmute(gipa(ash::vk::Handle::from_raw(instance), c"vkGetDeviceProcAddr".as_ptr()).expect("gdpa"))
    };
    let get = |name: &std::ffi::CStr| unsafe { gdpa(ash::vk::Handle::from_raw(device), name.as_ptr()).expect("entry point") };
    let set_viewport: unsafe extern "system" fn(u64, u32, u32, u64) = unsafe { std::mem::transmute(get(c"vkCmdSetViewport")) };
    let begin_cb: unsafe extern "system" fn(u64, u64) -> i32 = unsafe { std::mem::transmute(get(c"vkBeginCommandBuffer")) };
    let end_cb: unsafe extern "system" fn(u64) -> i32 = unsafe { std::mem::transmute(get(c"vkEndCommandBuffer")) };
    let mut direct = std::time::Duration::ZERO;
    for _ in 0..N / PER_BUFFER {
        unsafe { begin_cb(cb, begin) };
        let t0 = Instant::now();
        for _ in 0..PER_BUFFER {
            unsafe { set_viewport(cb, 0, 1, vp) };
        }
        direct += t0.elapsed();
        unsafe { end_cb(cb) };
    }

    let ns = |d: std::time::Duration| d.as_nanos() as f64 / f64::from(N);

    // 2. Forwarded, one ioctl each (the request and arguments written once: the guest's stub
    //    rewrites the same stack slots, which costs it nothing measurable here).
    let id = command_id("vkCmdSetViewport").unwrap();
    let bytes: Vec<u8> = [cb_w, 0, 1, vp].iter().flat_map(|a| a.to_le_bytes()).collect();
    let forwarded = |g: &mut Guest| {
        let mut took = std::time::Duration::ZERO;
        for _ in 0..N / PER_BUFFER {
            assert_eq!(g.ok("vkBeginCommandBuffer", &[cb_w, begin]) as i32, 0);
            g.p.mem.write(g.args, &bytes).unwrap();
            let mut c = Vec::new();
            c.extend_from_slice(&id.to_le_bytes());
            c.extend_from_slice(&4u32.to_le_bytes());
            c.extend_from_slice(&g.args.to_le_bytes());
            c.extend_from_slice(&[0u8; 16]);
            g.p.mem.write(g.call, &c).unwrap();
            let t0 = Instant::now();
            for _ in 0..PER_BUFFER {
                let r = g.p.syscall(&mut g.t, nr::IOCTL, [fd, OMNI_GPU_CALL, g.call, 0, 0, 0]);
                debug_assert_eq!(r, 0);
            }
            took += t0.elapsed();
            assert_eq!(g.ok("vkEndCommandBuffer", &[cb_w]) as i32, 0);
        }
        took
    };
    // 2b. The same, with the arguments right after the request (`OMNI_GPU_CALL_INLINE`).
    let inline = |g: &mut Guest| {
        let mut took = std::time::Duration::ZERO;
        let at = g.call + 0x400;
        let mut c = Vec::new();
        c.extend_from_slice(&id.to_le_bytes());
        c.extend_from_slice(&4u32.to_le_bytes());
        c.extend_from_slice(&(at + 32).to_le_bytes());
        c.extend_from_slice(&[0u8; 16]);
        c.extend_from_slice(&bytes);
        let cmd = omni_linux::gpu::OMNI_GPU_CALL_INLINE | ((c.len() as u64) << 16);
        for _ in 0..N / PER_BUFFER {
            assert_eq!(g.ok("vkBeginCommandBuffer", &[cb_w, begin]) as i32, 0);
            g.p.mem.write(at, &c).unwrap();
            let t0 = Instant::now();
            for _ in 0..PER_BUFFER {
                let r = g.p.syscall(&mut g.t, nr::IOCTL, [fd, cmd, at, 0, 0, 0]);
                debug_assert_eq!(r, 0);
            }
            took += t0.elapsed();
            assert_eq!(g.ok("vkEndCommandBuffer", &[cb_w]) as i32, 0);
        }
        took
    };
    // 3. One checked copy out of guest memory (the plain forwarding makes three and writes one).
    let copy = |g: &Guest| {
        let t0 = Instant::now();
        for _ in 0..N {
            std::hint::black_box(g.p.mem.read(g.call, 32).unwrap());
        }
        t0.elapsed()
    };
    // 4. A null system call through the same table: the dispatch alone.
    let t0 = Instant::now();
    for _ in 0..N {
        p.syscall(&mut g.t, nr::GETPID, [0; 6]);
    }
    let null = t0.elapsed();
    eprintln!("[gpu-call-cost] vkCmdSetViewport x{N}: host driver {:.1} ns/call, getpid {:.1} ns", ns(direct), ns(null));

    // The scratch area as a thread's stack is: a lazy mapping partly committed (each checked copy
    // asks the space whether its pages are), then all of it committed.
    for committed in ["part", "all"] {
        if committed == "all" {
            p.mem.write(s.at(256 * 1024), &vec![0u8; 768 * 1024]).unwrap();
        }
        for (fast, handles) in [(false, false), (true, false), (true, true)] {
            omni_linux::gpu::set_fast(fast);
            omni_linux::gpu::set_handle_cache(handles);
            let f = ns(forwarded(&mut g));
            eprintln!(
                "[gpu-call-cost] scratch {committed} committed, vk_fast={} vk_handles={}: forwarded {f:.1} ns/call (crossing {:.1} ns), a 32-byte guest read {:.1} ns",
                u8::from(fast),
                u8::from(handles),
                f - ns(direct),
                ns(copy(&g))
            );
        }
        omni_linux::gpu::set_handle_cache(false);
        omni_linux::gpu::set_fast(true);
        let f = ns(inline(&mut g));
        eprintln!("[gpu-call-cost] scratch {committed} committed, vk_fast=1, inline: forwarded {f:.1} ns/call (crossing {:.1} ns)", f - ns(direct));
        batched(&mut g, cb_w, begin, vp, N, PER_BUFFER, ns(direct));
    }
    omni_linux::gpu::set_fast(false);
    refused_batches(&mut g, cb_w, device_w, begin);

    g.ok("vkDestroyCommandPool", &[device_w, pool, 0]);
    g.ok("vkDestroyDevice", &[device_w, 0]);
    g.ok("vkDestroyInstance", &[instance_w, 0]);
}

/// A batch the host cannot replay as it is answers `EINVAL` and makes nothing after the bad record:
/// a record on another command buffer, a command that is not batched, a size that runs past the
/// end, a count that disagrees, a batch on what is not a command buffer. The config query answers
/// the lever.
fn refused_batches(g: &mut Guest, cb_w: u64, device_w: u64, begin: u64) {
    const EINVAL: i64 = -22;
    const EBADF: i64 = -9;
    let at = g.args + 0x3_0000;
    let record = |id: u32, args: &[u64], size: u32| {
        let mut r = Vec::new();
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&(args.len() as u32).to_le_bytes());
        r.extend_from_slice(&size.to_le_bytes());
        r.extend_from_slice(&0u32.to_le_bytes());
        for a in args {
            r.extend_from_slice(&a.to_le_bytes());
        }
        r
    };
    let line_width = command_id("vkCmdSetLineWidth").unwrap();
    let good = record(line_width, &[cb_w, u64::from(1.0f32.to_bits())], 32);
    let send = |g: &mut Guest, wrapper: u64, bytes: &[u8], count: u64| {
        g.p.mem.write(at, bytes).unwrap();
        g.raw(omni_linux::gpu::BATCH_COMMAND, &[wrapper, at, bytes.len() as u64, count]).0
    };
    assert_eq!(g.ok("vkBeginCommandBuffer", &[cb_w, begin]) as i32, 0);
    assert_eq!(send(g, cb_w, &good, 1), 0, "a good batch");
    let other_cb = record(line_width, &[device_w, u64::from(1.0f32.to_bits())], 32);
    assert_eq!(send(g, cb_w, &[good.clone(), other_cb].concat(), 2), EINVAL, "another command buffer's record");
    let not_batched = record(command_id("vkCmdBeginRenderPass").unwrap(), &[cb_w, 0, 0], 40);
    assert_eq!(send(g, cb_w, &not_batched, 1), EINVAL, "a command that is not batched");
    assert_eq!(send(g, cb_w, &record(line_width, &[cb_w, 0], 40), 1), EINVAL, "a size past the end");
    assert_eq!(send(g, cb_w, &good, 2), EINVAL, "a count that disagrees");
    assert_eq!(send(g, device_w, &good, 1), EBADF, "a device is not a command buffer");
    assert_eq!(g.ok("vkEndCommandBuffer", &[cb_w]) as i32, 0);
    for on in [true, false] {
        omni_linux::gpu::set_batch(on);
        assert_eq!(g.raw(omni_linux::gpu::CONFIG_COMMAND, &[]), (0, u64::from(on)), "the config query");
    }
}

/// The batched form: records as the guest's driver writes them (`device/src/vk/driver.c`) --
/// `{u32 id, u32 argc, u32 size, u32 0, u64 args[argc], copies}` with the viewport copied in after
/// the arguments and the pointer argument at the copy -- `PER_BATCH` of them in one
/// `OMNI_VK_ID_BATCH` ioctl.
fn batched(g: &mut Guest, cb_w: u64, begin: u64, vp: u64, n: u32, per_buffer: u32, direct_ns: f64) {
    let batch_id = omni_linux::gpu::BATCH_COMMAND;
    let id = command_id("vkCmdSetViewport").unwrap();
    const PER_BATCH: u32 = 256;
    let at = g.args + 0x2_0000;
    let viewport = g.p.mem.read(vp, 24).unwrap();
    let mut records = Vec::new();
    for _ in 0..PER_BATCH {
        let start = records.len() as u64;
        records.extend_from_slice(&id.to_le_bytes());
        records.extend_from_slice(&4u32.to_le_bytes());
        records.extend_from_slice(&(16u32 + 32 + 24).to_le_bytes());
        records.extend_from_slice(&0u32.to_le_bytes());
        for a in [cb_w, 0, 1, at + start + 48] {
            records.extend_from_slice(&a.to_le_bytes());
        }
        records.extend_from_slice(&viewport);
    }
    g.p.mem.write(at, &records).unwrap();
    let mut took = std::time::Duration::ZERO;
    let mut calls = 0u32;
    while calls < n {
        assert_eq!(g.ok("vkBeginCommandBuffer", &[cb_w, begin]) as i32, 0);
        let mut c = Vec::new();
        c.extend_from_slice(&batch_id.to_le_bytes());
        c.extend_from_slice(&4u32.to_le_bytes());
        c.extend_from_slice(&g.args.to_le_bytes());
        c.extend_from_slice(&[0u8; 16]);
        let a: Vec<u8> = [cb_w, at, records.len() as u64, u64::from(PER_BATCH)].iter().flat_map(|a| a.to_le_bytes()).collect();
        g.p.mem.write(g.args, &a).unwrap();
        g.p.mem.write(g.call, &c).unwrap();
        let t0 = Instant::now();
        for _ in 0..per_buffer / PER_BATCH {
            let r = g.p.syscall(&mut g.t, nr::IOCTL, [g.fd, OMNI_GPU_CALL, g.call, 0, 0, 0]);
            assert_eq!(r, 0, "the batch");
        }
        took += t0.elapsed();
        calls += per_buffer / PER_BATCH * PER_BATCH;
        assert_eq!(g.ok("vkEndCommandBuffer", &[cb_w]) as i32, 0);
    }
    let ns = took.as_nanos() as f64 / f64::from(calls);
    eprintln!("[gpu-call-cost] batched x{calls} ({PER_BATCH} a batch): {ns:.1} ns/call (crossing {:.1} ns)", ns - direct_ns);
}
