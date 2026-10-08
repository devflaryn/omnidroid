//! `/dev/omni-gpu`: the host's GPU, as a paravirtual device (D3a design,
//! `docs/superpowers/specs/2026-09-27-d3a-guest-vulkan-design.md`).
//!
//! The guest's Vulkan driver (`device/src/vk/`, `/vendor/lib64/hw/vulkan.omni.so`) forwards each
//! Vulkan command as one `ioctl(OMNI_GPU_CALL)`: a command id and its arguments as 64-bit values.
//! Guest and host share one address space and Vulkan's structs have one layout on both, so the
//! host's driver reads the guest's structs where they lie; what is translated here is only what
//! a host driver cannot take from a guest -- the dispatchable handles the guest's loader owns the
//! first word of, and the Android extensions no host driver has ([`special`]).
//!
//! Most commands are generated from `vk.xml` ([`generated`], `tools/gen_vk_forward.py`).
use std::collections::HashMap;
use std::ffi::{c_char, CStr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EBADF, EINVAL, EIO, ENODEV, ENOSYS, ENOTTY};
use crate::process::{Process, Task};

pub(crate) mod ahb;
pub mod backend;
pub(crate) mod generated;
pub mod gl;
pub(crate) mod native;
pub(crate) mod special;
pub mod window_present;

/// `_IOWR('G', 1, struct omni_gpu_call)`, a 32-byte argument.
pub const OMNI_GPU_CALL: u64 = 0xc020_4701;
/// `_IOWR('G', 2, 32 + 8 * argc)`: a request with its arguments right after it (the `args`
/// pointer then names them there), read in one checked copy instead of two. The guest's driver
/// sends it while [`FAST`] is on (bit 1 of [`CONFIG_COMMAND`]'s answer). Its size bits vary.
pub const OMNI_GPU_CALL_INLINE: u64 = 0xc000_4702;
/// The size field of an ioctl number.
const INLINE_SIZE_MASK: u64 = 0x3fff << 16;
/// The most arguments a Vulkan command has, with room to spare.
const MAX_ARGS: u32 = 32;

/// Why a forwarded call could not be made. The guest's driver sees it as the ioctl's errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallError {
    /// An unknown command, a wrong argument count, or an argument that cannot be what it must be.
    Args,
    /// A dispatchable handle that is not one this device made.
    Handle(u64),
    /// The host driver has no such entry point.
    Missing(&'static str),
    /// The host has no Vulkan at all.
    NoHost,
    /// A host call made for the guest (not the guest's own) failed with this `VkResult`.
    Host(i32),
}

impl CallError {
    fn errno(self) -> Errno {
        match self {
            Self::Args => EINVAL,
            Self::Handle(_) => EBADF,
            Self::Missing(_) => ENOSYS,
            Self::NoHost => ENODEV,
            Self::Host(_) => EIO,
        }
    }
}

type GetProcAddr = unsafe extern "system" fn(u64, *const c_char) -> Option<unsafe extern "system" fn()>;

/// The host's Vulkan loader, loaded once per host process: where the platform says a loader may
/// be (`omni_platform::window::vulkan_loader_candidates`, as `omni-gfx` loads it), else the
/// system's default. On macOS the default finds nothing -- dyld does not search Homebrew's
/// `/opt/homebrew/lib` -- and every forwarded command answered `ENODEV` (SurfaceFlinger's ANGLE then
/// had no instance extensions and aborted: the first real-AOSP boot on the M1).
pub(crate) fn entry() -> Result<&'static ash::Entry, CallError> {
    static ENTRY: OnceLock<Option<ash::Entry>> = OnceLock::new();
    ENTRY
        .get_or_init(|| {
            let candidates = omni_platform::window::vulkan_loader_candidates();
            // SAFETY: loading the host's Vulkan loader runs its initializers, which is what loading
            // it is for; it is loaded once and never unloaded.
            let loaded = candidates.iter().find_map(|path| unsafe { ash::Entry::load_from(path) }.ok());
            let entry = loaded.or_else(|| unsafe { ash::Entry::load() }.ok());
            if entry.is_none() {
                eprintln!("[gpu] no host Vulkan loader: tried {candidates:?} and the system default");
            }
            entry
        })
        .as_ref()
        .ok_or(CallError::NoHost)
}

/// The host driver's entry points for one host `VkInstance` or `VkDevice`, each resolved once.
pub(crate) struct Table {
    get_proc: GetProcAddr,
    /// The instance or device the entry points are asked of (0 for the global commands).
    owner: u64,
    fns: Vec<AtomicUsize>,
}

impl Table {
    /// The host instance or device this table's entry points belong to.
    pub(crate) fn owner(&self) -> u64 {
        self.owner
    }

    pub(crate) fn new(get_proc: GetProcAddr, owner: u64) -> Arc<Self> {
        Arc::new(Self { get_proc, owner, fns: (0..generated::COMMANDS.len()).map(|_| AtomicUsize::new(0)).collect() })
    }

    /// The instance-level table: `vkGetInstanceProcAddr` of `instance` (0: the global commands).
    pub(crate) fn instance(instance: u64) -> Result<Arc<Self>, CallError> {
        // SAFETY: ash resolved `vkGetInstanceProcAddr` from the loader; its signature is Vulkan's.
        let gipa: GetProcAddr = unsafe { std::mem::transmute(entry()?.static_fn().get_instance_proc_addr) };
        Ok(Self::new(gipa, instance))
    }

    /// A host entry point the forwarding table has no id for (a host-only command the host itself
    /// calls, never the guest), asked for each time.
    pub(crate) fn lookup(&self, name: &CStr) -> Option<usize> {
        // SAFETY: `owner` is a live host instance or device and `name` is NUL-terminated.
        unsafe { (self.get_proc)(self.owner, name.as_ptr()) }.map(|f| f as usize)
    }

    /// The host driver's entry point `names[0]` (or an alias), for command `id`.
    pub(crate) fn get(&self, id: u32, names: &'static [&'static CStr]) -> Result<usize, CallError> {
        let slot = self.fns.get(id as usize).ok_or(CallError::Args)?;
        let f = slot.load(Ordering::Relaxed);
        if f != 0 {
            return Ok(f);
        }
        for name in names {
            // SAFETY: `owner` is a live host instance or device (or 0 for a global command), and
            // `name` is NUL-terminated.
            if let Some(f) = unsafe { (self.get_proc)(self.owner, name.as_ptr()) } {
                slot.store(f as usize, Ordering::Relaxed);
                return Ok(f as usize);
            }
        }
        Err(CallError::Missing(names.first().and_then(|n| n.to_str().ok()).unwrap_or("?")))
    }
}

/// What a dispatchable handle is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Instance,
    PhysicalDevice,
    Device,
    Queue,
    CommandBuffer,
}

pub(crate) struct Object {
    pub kind: Kind,
    /// The table its commands resolve in: its instance's, or its device's.
    pub table: Arc<Table>,
    /// The host instance or device it belongs to (itself, for an instance or device).
    pub parent: u64,
}

/// **The forwarding's fast path** (`omni_linux::lever`'s `vk_fast=0|1`, `OMNI_VK_FAST=1` from the
/// start; off by default): a dispatchable handle's host object and table from this thread's
/// [`HandleCache`] rather than the device's locked map, and no result written back when it is 0
/// (the guest's driver zeroes it). Measured with `tests/gpu_call_cost.rs`, on an E-core.
pub static FAST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// **Batching** (`vk_batch=0|1`, `OMNI_VK_BATCH=1`; off by default): what the guest's driver is told
/// at each `vkBeginCommandBuffer` (`OMNI_VK_ID_CONFIG`). On, the commands that only record into a
/// command buffer reach the host in batches ([`BATCH_COMMAND`]) instead of one system call each.
pub static BATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switch [`FAST`].
pub fn set_fast(on: bool) {
    FAST.store(on, Ordering::Relaxed);
}

/// Switch [`BATCH`].
pub fn set_batch(on: bool) {
    BATCH.store(on, Ordering::Relaxed);
}

/// `OMNI_VK_FAST` and `OMNI_VK_BATCH`, read once per host process (the first open of the device).
fn read_switches() {
    static READ: OnceLock<()> = OnceLock::new();
    READ.get_or_init(|| {
        let on = |name: &str| std::env::var(name).as_deref() == Ok("1");
        if on("OMNI_VK_FAST") {
            set_fast(true);
        }
        if on("OMNI_VK_BATCH") {
            set_batch(true);
        }
    });
}

/// The guest driver's batch of recorded commands (`device/src/vk/driver.c`): `(command buffer
/// wrapper, records, bytes, count)`, each record `{u32 id, u32 argc, u32 size, u32 0, u64
/// args[argc], what the arguments point at}`.
pub const BATCH_COMMAND: u32 = special::ID_BATCH;
/// What the host asks of the guest's driver: bit 0, batch ([`BATCH`]).
pub const CONFIG_COMMAND: u32 = special::ID_CONFIG;
/// The most bytes one batch may hold (the guest's are 64 KiB).
const BATCH_MAX: u64 = 1 << 20;

/// Every change to any device's handle map, counted: what [`HandleCache`] entries are checked
/// against. Objects come and go a few times a frame at most; commands are thousands.
static GENERATION: AtomicU64 = AtomicU64::new(1);
/// Each [`Gpu`]'s own number, for [`HandleCache`] (an address could be reused).
static NEXT_GPU: AtomicU64 = AtomicU64::new(1);

/// A guest wrapper's host handle, kind and table, as this thread last looked them up: the same
/// command buffer, device or queue again and again on a render thread. An entry holds while no
/// handle map has changed since ([`GENERATION`]) -- the guest's driver writes a wrapper's host
/// handle only when the host has just made the object (a change), and frees one only after the host
/// has destroyed it (another) -- so a hit is what the map would answer, without its lock, the
/// wrapper's read, or the hash.
struct HandleCache {
    entries: [Option<CachedHandle>; 4],
    next: usize,
}

struct CachedHandle {
    gpu: u64,
    generation: u64,
    wrapper: u64,
    host: u64,
    kind: Kind,
    table: Arc<Table>,
}

thread_local! {
    static HANDLES: std::cell::RefCell<HandleCache> = const { std::cell::RefCell::new(HandleCache { entries: [None, None, None, None], next: 0 }) };
}

/// A device number that is never reused.
pub(crate) struct GpuId(u64);

impl Default for GpuId {
    fn default() -> Self {
        Self(NEXT_GPU.fetch_add(1, Ordering::Relaxed))
    }
}

/// One open of `/dev/omni-gpu`: the host objects made through it.
#[derive(Default)]
pub struct Gpu {
    pub(crate) id: GpuId,
    /// Dispatchable objects, by host handle. Changed only through [`Gpu::add`], [`Gpu::remove`] and
    /// [`Gpu::retain_objects`], which count the change ([`GENERATION`]).
    pub(crate) objects: Mutex<HashMap<u64, Object>>,
    /// What the Android extensions need of each device, by host handle.
    pub(crate) devices: Mutex<HashMap<u64, native::DeviceInfo>>,
    /// Images on gralloc buffers (`VK_ANDROID_native_buffer`), by host handle.
    pub(crate) native: Mutex<HashMap<u64, native::NativeImage>>,
    /// Swapchain images made and not yet bound to their gralloc buffer: format and size.
    pub(crate) swapchain_images: Mutex<HashMap<u64, (ash::vk::Format, u32, u32)>>,
    /// Images made for imported gralloc buffers, and the imported memory (their mirrors).
    pub(crate) ahb_images: Mutex<HashMap<u64, ahb::AhbImage>>,
    pub(crate) ahb_memory: Mutex<HashMap<u64, ahb::AhbMemory>>,
}

impl Gpu {
    #[must_use]
    pub fn open() -> Arc<Self> {
        read_switches();
        Arc::default()
    }

    /// The host handle behind a guest dispatchable wrapper (`{ dispatch, host }`), and the table
    /// its commands resolve in.
    pub(crate) fn dispatchable(&self, p: &Process, wrapper: u64) -> Result<(u64, Arc<Table>), CallError> {
        self.lookup(p, wrapper).map(|(host, _, table)| (host, table))
    }

    /// The host handle behind a guest wrapper that must be a `kind`.
    pub(crate) fn dispatchable_of(&self, p: &Process, wrapper: u64, kind: Kind) -> Result<u64, CallError> {
        match self.lookup(p, wrapper) {
            Ok((host, k, _)) if k == kind => Ok(host),
            _ => Err(CallError::Handle(wrapper)),
        }
    }

    /// A wrapper's host handle, kind and table: from this thread's [`HandleCache`] under [`FAST`],
    /// else read from the wrapper and looked up in the map.
    fn lookup(&self, p: &Process, wrapper: u64) -> Result<(u64, Kind, Arc<Table>), CallError> {
        let fast = FAST.load(Ordering::Relaxed);
        // Read before the map is, so an entry made from a map a change has since passed is stale.
        let generation = GENERATION.load(Ordering::Acquire);
        if fast {
            let hit = HANDLES.with(|c| {
                c.borrow().entries.iter().flatten().find(|e| e.gpu == self.id.0 && e.wrapper == wrapper && e.generation == generation).map(|e| (e.host, e.kind, Arc::clone(&e.table)))
            });
            if let Some(hit) = hit {
                return Ok(hit);
            }
        }
        let at = wrapper.checked_add(8).ok_or(CallError::Handle(wrapper))?;
        let host = if fast {
            let mut host = [0u8; 8];
            p.mem.read_into(at, &mut host).map(|()| u64::from_le_bytes(host))
        } else {
            p.mem.read_u64(at)
        }
        .map_err(|_| CallError::Handle(wrapper))?;
        let (kind, table) = {
            let objects = self.objects.lock();
            let o = objects.get(&host).ok_or(CallError::Handle(wrapper))?;
            (o.kind, Arc::clone(&o.table))
        };
        if fast {
            HANDLES.with(|c| {
                let mut c = c.borrow_mut();
                let at = c.next;
                c.next = (at + 1) % c.entries.len();
                c.entries[at] = Some(CachedHandle { gpu: self.id.0, generation, wrapper, host, kind, table: Arc::clone(&table) });
            });
        }
        Ok((host, kind, table))
    }

    /// Count a change to the handle map; called with its lock held, after the change.
    fn changed() {
        GENERATION.fetch_add(1, Ordering::Release);
    }

    pub(crate) fn add(&self, host: u64, kind: Kind, parent: u64, table: Arc<Table>) {
        let mut objects = self.objects.lock();
        objects.insert(host, Object { kind, table, parent });
        Self::changed();
    }

    pub(crate) fn remove(&self, host: u64) {
        let mut objects = self.objects.lock();
        objects.remove(&host);
        Self::changed();
    }

    /// Keep only the objects `keep` says to (an instance's or device's children, at its end).
    pub(crate) fn retain_objects(&self, keep: impl FnMut(&u64, &mut Object) -> bool) {
        let mut objects = self.objects.lock();
        objects.retain(keep);
        Self::changed();
    }
}

/// A host entry point of `table`, as function type `F`.
///
/// # Safety
/// `F` must be the entry point's exact Vulkan signature.
pub(crate) unsafe fn entry_point<F: Copy>(table: &Table, id: u32, names: &'static [&'static CStr]) -> Result<F, CallError> {
    let p = table.get(id, names)?;
    debug_assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
    // SAFETY: `p` is a non-null function address; the caller names its type.
    Ok(unsafe { std::mem::transmute_copy(&p) })
}

/// The id of command `name` in the forwarding table (`vkCreateInstance`, ...).
#[must_use]
pub fn command_id(name: &str) -> Option<u32> {
    generated::COMMANDS.iter().position(|(n, _, _)| *n == name).map(|i| i as u32)
}

/// `OMNI_GPU_STATS=<seconds>`: per forwarded command, how many this host process made and the time
/// the host spent on them, every so often, most time first (`[gpu-stats]`) -- the forwarding's
/// cost per frame, and which command a frame waits in.
pub(crate) mod stats {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    use std::sync::OnceLock;
    use std::time::Duration;

    const SLOTS: usize = 512;
    static COUNT: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static NANOS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
    static MAX: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

    pub(super) fn on() -> bool {
        static ON: OnceLock<Option<u64>> = OnceLock::new();
        ON.get_or_init(|| {
            let every = std::env::var("OMNI_GPU_STATS").ok().and_then(|v| v.parse::<u64>().ok())?;
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_secs(every.max(1)));
                report(every.max(1));
            });
            Some(every)
        })
        .is_some()
    }

    /// Whether stats are being kept (for a command's own parts, [`add`] with a part's id).
    pub(crate) fn enabled() -> bool {
        on()
    }

    pub(crate) fn add(id: u32, took: Duration) {
        // The Android extension's commands (0x1000..) in the last slots.
        let i = if id >= super::special::ID_GRALLOC_USAGE { SLOTS - 8 + ((id - super::special::ID_GRALLOC_USAGE) as usize).min(7) } else { (id as usize).min(SLOTS - 9) };
        let ns = took.as_nanos() as u64;
        COUNT[i].fetch_add(1, Relaxed);
        NANOS[i].fetch_add(ns, Relaxed);
        MAX[i].fetch_max(ns, Relaxed);
    }

    fn report(every: u64) {
        let mut rows: Vec<(u64, u64, u64, usize)> = (0..SLOTS)
            .filter_map(|i| {
                let (c, ns, max) = (COUNT[i].swap(0, Relaxed), NANOS[i].swap(0, Relaxed), MAX[i].swap(0, Relaxed));
                (c > 0).then_some((ns, c, max, i))
            })
            .collect();
        if rows.is_empty() {
            return;
        }
        rows.sort_by(|a, b| b.0.cmp(&a.0));
        let (calls, ns): (u64, u64) = rows.iter().fold((0, 0), |(c, n), r| (c + r.1, n + r.0));
        const EXTRA: [&str; 8] = [
            "vkGetSwapchainGrallocUsageANDROID",
            "vkGetSwapchainGrallocUsage2ANDROID",
            "vkGetSwapchainGrallocUsage3ANDROID",
            "vkGetSwapchainGrallocUsage4ANDROID",
            "vkAcquireImageANDROID",
            "vkQueueSignalReleaseImageANDROID",
            "(release: GPU wait)",
            "(release: into the buffer)",
        ];
        let name = |i: usize| match i.checked_sub(SLOTS - 8) {
            Some(e) => EXTRA.get(e).map_or_else(|| format!("extra#{e}"), |n| (*n).to_string()),
            None => super::generated::COMMANDS.get(i).map_or_else(|| format!("#{i}"), |c| c.0.to_string()),
        };
        let top: Vec<String> = rows.iter().take(14).map(|(n, c, m, i)| format!("{} {c}x {:.1}ms (max {:.1})", name(*i), *n as f64 / 1e6, *m as f64 / 1e6)).collect();
        eprintln!(
            "[gpu-stats] host pid {} {every}s: {calls} calls, {:.1} ms in the host: {}",
            std::process::id(),
            ns as f64 / 1e6,
            top.join(", ")
        );
    }
}

/// **Microseconds between polls of a guest `vkWaitForFences`**, 0 for the host driver's own wait
/// (`omni_linux::lever`'s `fence_poll=`). NVIDIA's `vkWaitForFences` spins: a guest thread waiting
/// for its frame's fences burns its core for as long as the GPU takes. Polled -- the fences asked
/// with a zero timeout, a sleep between -- the thread gives the core up while it waits, at the cost
/// of up to one period of latency when the fences signal.
pub static FENCE_POLL_US: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// `vkWaitForFences(device, count, fences, waitAll, timeout)`, polled every `every_us`: the same
/// answer the host's wait gives -- `VK_SUCCESS`, an error, or `VK_TIMEOUT` once `timeout` ns passed.
fn wait_for_fences_polling(gpu: &Gpu, p: &Process, a: &[u64], every_us: u32) -> Result<u64, CallError> {
    const NAMES: &[&CStr] = &[c"vkWaitForFences"];
    const VK_TIMEOUT: i32 = 2;
    let (device, t) = gpu.dispatchable(p, a[0])?;
    // SAFETY: the Vulkan signature; the fence array is the guest's, readable for the call (D4).
    let wait: unsafe extern "system" fn(u64, u32, u64, u32, u64) -> i32 = unsafe { std::mem::transmute(t.get(generated::ID_VK_WAIT_FOR_FENCES, NAMES)?) };
    let started = std::time::Instant::now();
    let limit = std::time::Duration::from_nanos(a[4]);
    loop {
        let r = unsafe { wait(device, a[1] as u32, a[2], a[3] as u32, 0) };
        if r != VK_TIMEOUT {
            return Ok(u64::from(r as u32));
        }
        let waited = started.elapsed();
        if waited >= limit {
            return Ok(u64::from(VK_TIMEOUT as u32));
        }
        std::thread::sleep(std::time::Duration::from_micros(u64::from(every_us)).min(limit - waited));
    }
}

/// `ioctl` on `/dev/omni-gpu`.
pub fn ioctl(p: &Process, t: &mut Task, gpu: &Arc<Gpu>, cmd: u64, arg: u64) -> SysResult {
    // The GLES driver's commands (the GL backend) on the same node.
    if cmd == gl::OMNI_GL_CALL {
        return gl::ioctl(p, t, arg);
    }
    // The arguments right after the request, its size in the number (`OMNI_GPU_CALL_INLINE`).
    let inline = cmd & 0xffff_ffff & !INLINE_SIZE_MASK == OMNI_GPU_CALL_INLINE;
    if cmd != OMNI_GPU_CALL && !inline {
        return Err(ENOTTY);
    }
    // An inline request comes from a driver that zeroes the result, as the fast path assumes.
    let fast = inline || FAST.load(Ordering::Relaxed);
    // Fast: the request and its arguments into this stack, no allocation (a checked copy costs
    // ~110 ns on an E-core, two allocations of it a third). Inline: both in one checked copy.
    let mut call = [0u8; 32];
    let mut words = [0u64; MAX_ARGS as usize];
    let slow_args: Vec<u64>;
    let args: &[u64] = if inline {
        let size = ((cmd & INLINE_SIZE_MASK) >> 16) as usize;
        let mut bytes = [0u8; 32 + MAX_ARGS as usize * 8];
        if size < 32 || size > bytes.len() || size % 8 != 0 {
            return Err(EINVAL);
        }
        let bytes = &mut bytes[..size];
        p.mem.read_into(arg, bytes)?;
        call.copy_from_slice(&bytes[..32]);
        let argc = u32::from_le_bytes(call[4..8].try_into().expect("4")) as usize;
        if 32 + argc * 8 != size {
            return Err(EINVAL);
        }
        for (w, b) in words.iter_mut().zip(bytes[32..].chunks_exact(8)) {
            *w = u64::from_le_bytes(b.try_into().expect("8"));
        }
        &words[..argc]
    } else if fast {
        p.mem.read_into(arg, &mut call)?;
        let argc = u32::from_le_bytes(call[4..8].try_into().expect("4"));
        if argc > MAX_ARGS {
            return Err(EINVAL);
        }
        let mut bytes = [0u8; MAX_ARGS as usize * 8];
        let bytes = &mut bytes[..argc as usize * 8];
        p.mem.read_into(u64::from_le_bytes(call[8..16].try_into().expect("8")), bytes)?;
        for (w, b) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            *w = u64::from_le_bytes(b.try_into().expect("8"));
        }
        &words[..argc as usize]
    } else {
        call.copy_from_slice(&p.mem.read(arg, 32)?);
        let argc = u32::from_le_bytes(call[4..8].try_into().expect("4"));
        if argc > MAX_ARGS {
            return Err(EINVAL);
        }
        let args_at = u64::from_le_bytes(call[8..16].try_into().expect("8"));
        slow_args = p.mem.read(args_at, argc as usize * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8"))).collect();
        &slow_args
    };
    let id = u32::from_le_bytes(call[0..4].try_into().expect("4"));
    if id == CONFIG_COMMAND {
        let config = u64::from(BATCH.load(Ordering::Relaxed)) | u64::from(FAST.load(Ordering::Relaxed)) << 1;
        p.mem.write(arg + 16, &config.to_le_bytes())?;
        return Ok(0);
    }
    if id == BATCH_COMMAND {
        let result = replay_batch(gpu, p, args).map_err(CallError::errno)?;
        if result != 0 || !fast {
            p.mem.write(arg + 16, &result.to_le_bytes())?;
        }
        return Ok(0);
    }
    if trace_on() {
        let name = generated::COMMANDS.get(id as usize).map_or("(extra)", |c| c.0);
        eprintln!("[gpu] {}:{} {name} {args:x?}", p.sys.pid, t.tid);
    }
    let started = stats::on().then(std::time::Instant::now);
    let poll = FENCE_POLL_US.load(std::sync::atomic::Ordering::Relaxed);
    let answer = if id >= special::ID_GRALLOC_USAGE {
        special::extra(gpu, p, id, args)
    } else if id == generated::ID_VK_WAIT_FOR_FENCES && poll > 0 && args.len() >= 5 {
        wait_for_fences_polling(gpu, p, args, poll)
    } else if let Some(answer) = special::foreign_barrier(gpu, p, id, args) {
        answer
    } else {
        generated::dispatch(gpu, p, id, args)
    };
    if let Some(t0) = started {
        stats::add(id, t0.elapsed());
    }
    if trace_on() {
        eprintln!("[gpu] {}:{}   -> {answer:?}", p.sys.pid, t.tid);
    }
    let result = answer.map_err(CallError::errno)?;
    // The guest's driver sends the result zeroed: a 0 (every `void` command) needs no write.
    if result != 0 || !fast {
        p.mem.write(arg + 16, &result.to_le_bytes())?;
    }
    Ok(0)
}

/// `OMNI_GPU_TRACE=1`: each command named before the host driver runs it, and its answer.
fn trace_on() -> bool {
    static TRACE: OnceLock<bool> = OnceLock::new();
    *TRACE.get_or_init(|| std::env::var("OMNI_GPU_TRACE").as_deref() == Ok("1"))
}

/// One batch of commands recorded into one command buffer ([`BATCH_COMMAND`]): `(wrapper, records,
/// bytes, count)`. The records are read in one checked copy; the command buffer is unwrapped once;
/// each record must be a [`generated::batchable`] command on that same command buffer, whole, with
/// its argument count -- else nothing after it is made and the batch answers `EINVAL`. What a
/// record's arguments point at is the guest's copy inside the batch, read by the host driver where
/// it lies (one address space), unchanged while this call runs (its thread is here).
fn replay_batch(gpu: &Gpu, p: &Process, a: &[u64]) -> Result<u64, CallError> {
    let [wrapper, at, len, count] = *a else { return Err(CallError::Args) };
    if len > BATCH_MAX || len % 8 != 0 {
        return Err(CallError::Args);
    }
    let (h0, kind, table) = gpu.lookup(p, wrapper)?;
    if kind != Kind::CommandBuffer {
        return Err(CallError::Handle(wrapper));
    }
    let mut bytes = vec![0u8; len as usize];
    p.mem.read_into(at, &mut bytes).map_err(|_| CallError::Args)?;
    let word = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().expect("4"));
    let stats = stats::on();
    let trace = trace_on();
    let mut args = [0u64; MAX_ARGS as usize];
    let (mut off, mut made) = (0usize, 0u64);
    while off < bytes.len() {
        if bytes.len() - off < 16 {
            return Err(CallError::Args);
        }
        let (id, argc, size) = (word(off), word(off + 4), word(off + 8) as usize);
        let fits = argc <= MAX_ARGS && size >= 16 + argc as usize * 8 && size % 8 == 0 && size <= bytes.len() - off;
        if !fits || !generated::batchable(id) || generated::COMMANDS.get(id as usize).map(|c| c.1) != Some(argc) {
            return Err(CallError::Args);
        }
        let args = &mut args[..argc as usize];
        for (i, w) in args.iter_mut().enumerate() {
            let o = off + 16 + i * 8;
            *w = u64::from_le_bytes(bytes[o..o + 8].try_into().expect("8"));
        }
        if args[0] != wrapper {
            return Err(CallError::Args);
        }
        if trace {
            eprintln!("[gpu] {}   batched {} {args:x?}", p.sys.pid, generated::COMMANDS[id as usize].0);
        }
        let started = stats.then(std::time::Instant::now);
        generated::replay(&table, h0, id, args)?;
        if let Some(t0) = started {
            stats::add(id, t0.elapsed());
        }
        off += size;
        made += 1;
    }
    if made != count {
        return Err(CallError::Args);
    }
    Ok(0)
}
