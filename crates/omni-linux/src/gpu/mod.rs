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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::errno::{Errno, SysResult, EBADF, EINVAL, EIO, ENODEV, ENOSYS, ENOTTY};
use crate::process::{Process, Task};

pub(crate) mod ahb;
pub(crate) mod generated;
pub(crate) mod native;
pub(crate) mod special;

/// `_IOWR('G', 1, struct omni_gpu_call)`, a 32-byte argument.
pub const OMNI_GPU_CALL: u64 = 0xc020_4701;
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

/// The host's Vulkan loader, loaded once per host process.
pub(crate) fn entry() -> Result<&'static ash::Entry, CallError> {
    static ENTRY: OnceLock<Option<ash::Entry>> = OnceLock::new();
    // SAFETY: loading the system's Vulkan loader runs its initializers, which is what loading it
    // is for; it is loaded once and never unloaded.
    ENTRY.get_or_init(|| unsafe { ash::Entry::load() }.ok()).as_ref().ok_or(CallError::NoHost)
}

/// The host driver's entry points for one host `VkInstance` or `VkDevice`, each resolved once.
pub(crate) struct Table {
    get_proc: GetProcAddr,
    /// The instance or device the entry points are asked of (0 for the global commands).
    owner: u64,
    fns: Vec<AtomicUsize>,
}

impl Table {
    pub(crate) fn new(get_proc: GetProcAddr, owner: u64) -> Arc<Self> {
        Arc::new(Self { get_proc, owner, fns: (0..generated::COMMANDS.len()).map(|_| AtomicUsize::new(0)).collect() })
    }

    /// The instance-level table: `vkGetInstanceProcAddr` of `instance` (0: the global commands).
    pub(crate) fn instance(instance: u64) -> Result<Arc<Self>, CallError> {
        // SAFETY: ash resolved `vkGetInstanceProcAddr` from the loader; its signature is Vulkan's.
        let gipa: GetProcAddr = unsafe { std::mem::transmute(entry()?.static_fn().get_instance_proc_addr) };
        Ok(Self::new(gipa, instance))
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

/// One open of `/dev/omni-gpu`: the host objects made through it.
#[derive(Default)]
pub struct Gpu {
    /// Dispatchable objects, by host handle.
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
        Arc::default()
    }

    /// The host handle behind a guest dispatchable wrapper (`{ dispatch, host }`), and the table
    /// its commands resolve in.
    pub(crate) fn dispatchable(&self, p: &Process, wrapper: u64) -> Result<(u64, Arc<Table>), CallError> {
        let host = p.mem.read_u64(wrapper.checked_add(8).ok_or(CallError::Handle(wrapper))?).map_err(|_| CallError::Handle(wrapper))?;
        let objects = self.objects.lock();
        let o = objects.get(&host).ok_or(CallError::Handle(wrapper))?;
        Ok((host, Arc::clone(&o.table)))
    }

    /// The host handle behind a guest wrapper that must be a `kind`.
    pub(crate) fn dispatchable_of(&self, p: &Process, wrapper: u64, kind: Kind) -> Result<u64, CallError> {
        let host = p.mem.read_u64(wrapper.checked_add(8).ok_or(CallError::Handle(wrapper))?).map_err(|_| CallError::Handle(wrapper))?;
        match self.objects.lock().get(&host) {
            Some(o) if o.kind == kind => Ok(host),
            _ => Err(CallError::Handle(wrapper)),
        }
    }

    pub(crate) fn add(&self, host: u64, kind: Kind, parent: u64, table: Arc<Table>) {
        self.objects.lock().insert(host, Object { kind, table, parent });
    }

    pub(crate) fn remove(&self, host: u64) {
        self.objects.lock().remove(&host);
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

/// `ioctl` on `/dev/omni-gpu`.
pub fn ioctl(p: &Process, t: &mut Task, gpu: &Arc<Gpu>, cmd: u64, arg: u64) -> SysResult {
    if cmd != OMNI_GPU_CALL {
        return Err(ENOTTY);
    }
    let call = p.mem.read(arg, 32)?;
    let id = u32::from_le_bytes(call[0..4].try_into().expect("4"));
    let argc = u32::from_le_bytes(call[4..8].try_into().expect("4"));
    let args_at = u64::from_le_bytes(call[8..16].try_into().expect("8"));
    if argc > MAX_ARGS {
        return Err(EINVAL);
    }
    let args: Vec<u64> = p.mem.read(args_at, argc as usize * 8)?.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().expect("8"))).collect();
    // OMNI_GPU_TRACE=1: each command named before the host driver runs it.
    static TRACE: OnceLock<bool> = OnceLock::new();
    if *TRACE.get_or_init(|| std::env::var("OMNI_GPU_TRACE").as_deref() == Ok("1")) {
        let name = generated::COMMANDS.get(id as usize).map_or("(extra)", |c| c.0);
        eprintln!("[gpu] {}:{} {name} {args:x?}", p.sys.pid, t.tid);
    }
    let answer = if id >= special::ID_GRALLOC_USAGE { special::extra(gpu, p, id, &args) } else { generated::dispatch(gpu, p, id, &args) };
    if *TRACE.get().unwrap_or(&false) {
        eprintln!("[gpu] {}:{}   -> {answer:?}", p.sys.pid, t.tid);
    }
    let result = answer.map_err(CallError::errno)?;
    p.mem.write(arg + 16, &result.to_le_bytes())?;
    Ok(0)
}
