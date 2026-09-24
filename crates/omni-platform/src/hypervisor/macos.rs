//! Hypervisor.framework on Apple silicon. See the module above for the contract; this file is the
//! FFI and the stage-2 mirror.
//!
//! # The mirror, and why one lock is enough
//!
//! [`MIRROR`] holds the attachments, the caller-owned private ranges and the overlays. Every change
//! to stage 2 is made under it, by exactly one of: [`attach`], [`detach`], [`overlay`],
//! [`remove_overlay`], and the hook [`host_mapping_changed`], which `vm/macos.rs` calls **while
//! holding its own registry lock**. So the lock order is *vm registry, then mirror*, and nothing
//! here takes the vm registry: the initial population and an overlay's restore read the host's
//! protection from the kernel (`mach_vm_region`), not from the registry.
//!
//! A host change and the hook that mirrors it are not atomic with respect to a vCPU running on
//! another thread: for the moment between them the guest can see the old stage 2. That is the same
//! window a Linux guest has between one thread's `munmap` and another thread's access -- a race in
//! the guest -- and the thread that made the change cannot return to its caller until the mirror
//! has caught up.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use super::{HostLimits, HvError, HvResult, Reg, Stage2, Stage2Stats, SysReg, VcpuExit};

const HV_SUCCESS: u32 = 0;
const HV_DENIED: u32 = 0xfae9_4007;

const HV_MEMORY_READ: u64 = 1 << 0;
const HV_MEMORY_WRITE: u64 = 1 << 1;
const HV_MEMORY_EXEC: u64 = 1 << 2;

const HV_EXIT_REASON_CANCELED: u32 = 0;
const HV_EXIT_REASON_EXCEPTION: u32 = 1;
const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;

/// `hv_reg_t`: `X0`..`X30` are 0..30, then these.
const HV_REG_PC: u32 = 31;
const HV_REG_FPCR: u32 = 32;
const HV_REG_FPSR: u32 = 33;
const HV_REG_CPSR: u32 = 34;

/// `VM_REGION_BASIC_INFO_64` and its count in `natural_t`s (`<mach/vm_region.h>`).
const VM_REGION_BASIC_INFO_64: i32 = 9;
const VM_REGION_BASIC_INFO_COUNT_64: u32 = 9;
const KERN_SUCCESS: i32 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
struct HvExitException {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

/// `hv_vcpu_exit_t`.
#[repr(C)]
#[derive(Clone, Copy)]
struct HvVcpuExit {
    reason: u32,
    exception: HvExitException,
}

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

/// `hv_simd_fp_uchar16_t` in memory: sixteen bytes, 16-byte aligned. Only ever passed by pointer.
#[repr(C, align(16))]
struct Q([u8; 16]);

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    fn hv_vm_create(config: *mut c_void) -> u32;
    fn hv_vm_get_max_vcpu_count(max: *mut u32) -> u32;
    fn hv_vm_config_get_default_ipa_size(bits: *mut u32) -> u32;
    fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> u32;
    fn hv_vm_unmap(ipa: u64, size: usize) -> u32;
    fn hv_vcpu_create(vcpu: *mut u64, exit: *mut *const HvVcpuExit, config: *mut c_void) -> u32;
    fn hv_vcpu_destroy(vcpu: u64) -> u32;
    fn hv_vcpu_run(vcpu: u64) -> u32;
    fn hv_vcpu_get_reg(vcpu: u64, reg: u32, value: *mut u64) -> u32;
    fn hv_vcpu_set_reg(vcpu: u64, reg: u32, value: u64) -> u32;
    fn hv_vcpu_get_sys_reg(vcpu: u64, reg: u16, value: *mut u64) -> u32;
    fn hv_vcpu_set_sys_reg(vcpu: u64, reg: u16, value: u64) -> u32;
    fn hv_vcpu_get_simd_fp_reg(vcpu: u64, reg: u32, value: *mut Q) -> u32;
    /// Really `hv_return_t (hv_vcpu_t, hv_simd_fp_reg_t, hv_simd_fp_uchar16_t)`, the vector **by
    /// value in `Q0`**. Stable Rust cannot pass a SIMD type through FFI (`simd_ffi` is unstable), so
    /// this is declared without arguments, only to take its address, and called by
    /// [`set_simd_raw`].
    fn hv_vcpu_set_simd_fp_reg();
    fn hv_vcpu_set_vtimer_mask(vcpu: u64, masked: bool) -> u32;
    fn hv_vcpu_get_vtimer_offset(vcpu: u64, offset: *mut u64) -> u32;
}

extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
    fn sys_icache_invalidate(start: *mut c_void, len: usize);
    fn mach_vm_region(
        task: libc::mach_port_t,
        address: *mut u64,
        size: *mut u64,
        flavor: i32,
        info: *mut i32,
        count: *mut u32,
        object_name: *mut libc::mach_port_t,
    ) -> i32;
}

fn name(code: u32) -> &'static str {
    match code {
        0xfae9_4001 => "HV_ERROR",
        0xfae9_4002 => "HV_BUSY",
        0xfae9_4003 => "HV_BAD_ARGUMENT",
        0xfae9_4005 => "HV_NO_RESOURCES",
        0xfae9_4006 => "HV_NO_DEVICE",
        0xfae9_4007 => "HV_DENIED",
        0xfae9_4008 => "HV_FAULT",
        0xfae9_400f => "HV_UNSUPPORTED",
        _ => "an undocumented code",
    }
}

fn check(operation: &'static str, code: u32) -> HvResult<()> {
    match code {
        HV_SUCCESS => Ok(()),
        HV_DENIED => Err(HvError::Denied { operation, code }),
        _ => Err(HvError::Failed { operation, code, name: name(code), detail: String::new() }),
    }
}

fn check_at(operation: &'static str, code: u32, start: u64, len: usize) -> HvResult<()> {
    match code {
        HV_SUCCESS => Ok(()),
        HV_DENIED => Err(HvError::Denied { operation, code }),
        _ => Err(HvError::Failed {
            operation,
            code,
            name: name(code),
            detail: format!(" at {start:#x}..{:#x}", start + len as u64),
        }),
    }
}

// ------------------------------------------------------------------------------------------ the VM

static LIMITS: OnceLock<HvResult<HostLimits>> = OnceLock::new();

pub(super) fn vm_init() -> HvResult<()> {
    LIMITS
        .get_or_init(|| {
            // SAFETY: a null config selects the defaults; the VM is the process's own.
            check("hv_vm_create", unsafe { hv_vm_create(core::ptr::null_mut()) })?;
            let mut ipa_bits = 0u32;
            let mut max_vcpus = 0u32;
            // SAFETY: each writes one `u32` this frame owns.
            check("hv_vm_config_get_default_ipa_size", unsafe {
                hv_vm_config_get_default_ipa_size(&mut ipa_bits)
            })?;
            // SAFETY: as above.
            check("hv_vm_get_max_vcpu_count", unsafe { hv_vm_get_max_vcpu_count(&mut max_vcpus) })?;
            Ok(HostLimits { ipa_bits, max_vcpus })
        })
        .clone()
        .map(|_| ())
}

pub(super) fn limits() -> HostLimits {
    match LIMITS.get() {
        Some(Ok(limits)) => *limits,
        // `Vm::limits` is only reachable through a `&Vm`, which only `Vm::get` hands out after a
        // successful `vm_init`.
        _ => HostLimits { ipa_bits: 0, max_vcpus: 0 },
    }
}

// ------------------------------------------------------------------------------ the stage-2 mirror

struct Mirror {
    /// `(base, len)` of every attached host range.
    attachments: Vec<(usize, usize)>,
    /// `(ipa, len)` of caller-owned memory mapped with `map_private`.
    private: Vec<(u64, usize)>,
    /// IPA page -> the host page and stage-2 flags pinned there.
    overlays: BTreeMap<u64, (usize, u64)>,
}

static MIRROR: Mutex<Mirror> =
    Mutex::new(Mirror { attachments: Vec::new(), private: Vec::new(), overlays: BTreeMap::new() });
/// Attachments live, read without the lock by the hook's fast path.
static ATTACHED: AtomicUsize = AtomicUsize::new(0);
static MIRRORED: AtomicU64 = AtomicU64::new(0);
static MAPS: AtomicU64 = AtomicU64::new(0);
static UNMAPS: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU64 = AtomicU64::new(0);

fn mirror() -> MutexGuard<'static, Mirror> {
    // A panic under this lock would have come from this file's own arithmetic; the map itself is
    // still consistent with stage 2 up to the call that panicked, so recovering is truer than
    // wedging every later mapping change in the process.
    MIRROR.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn page() -> usize {
    crate::vm::page_size()
}

fn flags_of_prot(prot: libc::c_int) -> u64 {
    let mut flags = 0;
    if prot & libc::PROT_READ != 0 {
        flags |= HV_MEMORY_READ;
    }
    if prot & libc::PROT_WRITE != 0 {
        flags |= HV_MEMORY_WRITE;
    }
    if prot & libc::PROT_EXEC != 0 {
        flags |= HV_MEMORY_EXEC;
    }
    flags
}

fn flags_of(permission: Stage2) -> u64 {
    (if permission.read { HV_MEMORY_READ } else { 0 })
        | (if permission.write { HV_MEMORY_WRITE } else { 0 })
        | (if permission.execute { HV_MEMORY_EXEC } else { 0 })
}

fn unmap(start: usize, len: usize) -> HvResult<()> {
    UNMAPS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: unmapping guest-physical space dereferences nothing on the host. Unmapping a range
    // that is partly or wholly unmapped succeeds (MEASURED), which is what makes an update
    // idempotent.
    let result = check_at("hv_vm_unmap", unsafe { hv_vm_unmap(start as u64, len) }, start as u64, len);
    if result.is_err() {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    result
}

fn map(host: usize, ipa: usize, len: usize, flags: u64) -> HvResult<()> {
    MAPS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `host..host + len` is memory of this process: an attached range (whose lifetime the
    // attachment's owner guarantees, and every change to which comes back through this mirror), an
    // overlay page, or `map_private` memory, both under their callers' contracts.
    let result = check_at(
        "hv_vm_map",
        unsafe { hv_vm_map(host as *mut c_void, ipa as u64, len, flags) },
        ipa as u64,
        len,
    );
    if result.is_err() {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// Re-establish stage 2 for `[start, end)` of an attachment, with host protection `flags`, leaving
/// overlaid pages alone.
fn apply(mirror: &Mirror, start: usize, end: usize, flags: u64) -> HvResult<()> {
    let mut first_error = None;
    let mut cursor = start;
    let mut segment = |from: usize, to: usize| {
        if from >= to {
            return;
        }
        let result = unmap(from, to - from).and_then(|()| {
            if flags == 0 {
                Ok(())
            } else {
                map(from, from, to - from, flags)
            }
        });
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    };
    for (&overlay, _) in mirror.overlays.range(start as u64..end as u64) {
        let overlay = overlay as usize;
        segment(cursor, overlay);
        cursor = overlay + page();
    }
    segment(cursor, end);
    first_error.map_or(Ok(()), Err)
}

/// The hook `vm/macos.rs` calls after every `mmap(MAP_FIXED)`, `mprotect` and `munmap` it makes, with
/// the protection the range now has (`PROT_NONE` for a range that was freed or decommitted).
///
/// Never panics and never fails outward: a stage-2 call that fails is counted in
/// [`Stage2Stats::failures`], which a CPU backend checks before trusting stage 2 again.
pub(crate) fn host_mapping_changed(address: usize, size: usize, prot: libc::c_int) {
    if ATTACHED.load(Ordering::SeqCst) == 0 {
        return;
    }
    let mirror = mirror();
    let end = address.saturating_add(size);
    let flags = flags_of_prot(prot);
    for &(base, len) in &mirror.attachments {
        let from = address.max(base);
        let to = end.min(base + len);
        if from < to {
            MIRRORED.fetch_add(1, Ordering::Relaxed);
            let _ = apply(&mirror, from, to, flags);
        }
    }
}

/// Walk the kernel's own record of `[start, end)` and mirror every region's current protection.
fn populate(mirror: &Mirror, start: usize, end: usize) -> HvResult<()> {
    let mut cursor = start as u64;
    while cursor < end as u64 {
        let mut address = cursor;
        let mut size = 0u64;
        let mut info = [0i32; VM_REGION_BASIC_INFO_COUNT_64 as usize];
        let mut count = VM_REGION_BASIC_INFO_COUNT_64;
        let mut object = 0;
        // SAFETY: every out-parameter is a local of the right size; `mach_task_self_` is set by
        // libSystem before any Rust code runs.
        let kr = unsafe {
            mach_vm_region(
                mach_task_self_,
                &mut address,
                &mut size,
                VM_REGION_BASIC_INFO_64,
                info.as_mut_ptr(),
                &mut count,
                &mut object,
            )
        };
        if kr != KERN_SUCCESS || address >= end as u64 {
            // Nothing allocated from `cursor` to `end`: it stays unmapped at stage 2.
            break;
        }
        let from = address.max(cursor) as usize;
        let to = (address + size).min(end as u64) as usize;
        // `vm_region_basic_info_64.protection` is the first field.
        apply(mirror, from, to, flags_of_prot(info[0]))?;
        cursor = address + size;
    }
    Ok(())
}

fn overlaps(a: u64, a_len: u64, b: u64, b_len: u64) -> bool {
    a < b + b_len && b < a + a_len
}

pub(super) fn attach(base: usize, len: usize) -> HvResult<()> {
    let limits = limits();
    let refuse = |reason| HvError::Range {
        operation: "attach",
        start: base as u64,
        end: base as u64 + len as u64,
        reason,
    };
    if len == 0 || base % page() != 0 || len % page() != 0 {
        return Err(refuse("an attachment is a non-empty whole number of host pages"));
    }
    let Some(end) = base.checked_add(len) else {
        return Err(refuse("the range wraps"));
    };
    if limits.ipa_bits == 0 || (end as u64) > (1u64 << limits.ipa_bits) {
        return Err(refuse(
            "it reaches past this host's guest-physical address size (hv_vm_config_get_default_ipa_size), \
             and IPA == VA means a guest address must be a valid IPA",
        ));
    }
    let mut mirror = mirror();
    if mirror.attachments.iter().any(|&(b, l)| overlaps(b as u64, l as u64, base as u64, len as u64))
        || mirror.private.iter().any(|&(i, l)| overlaps(i, l as u64, base as u64, len as u64))
    {
        return Err(refuse("it overlaps a range already mapped at stage 2"));
    }
    mirror.attachments.push((base, len));
    ATTACHED.fetch_add(1, Ordering::SeqCst);
    // A clean slate, then the host's truth. Anything a previous attachment of this range left is gone.
    let populated = unmap(base, len).and_then(|()| populate(&mirror, base, end));
    if let Err(error) = populated {
        mirror.attachments.retain(|&a| a != (base, len));
        ATTACHED.fetch_sub(1, Ordering::SeqCst);
        let _ = unmap(base, len);
        return Err(error);
    }
    Ok(())
}

pub(super) fn detach(base: usize, len: usize) {
    let mut mirror = mirror();
    mirror.attachments.retain(|&a| a != (base, len));
    let inside: Vec<u64> =
        mirror.overlays.range(base as u64..(base + len) as u64).map(|(&k, _)| k).collect();
    for key in inside {
        mirror.overlays.remove(&key);
    }
    let _ = unmap(base, len);
    ATTACHED.fetch_sub(1, Ordering::SeqCst);
}

pub(super) unsafe fn map_private(host: *mut u8, ipa: u64, len: usize, permission: Stage2) -> HvResult<()> {
    let refuse = |reason| HvError::Range { operation: "map_private", start: ipa, end: ipa + len as u64, reason };
    if len == 0 || ipa % page() as u64 != 0 || len % page() != 0 || (host as usize) % page() != 0 {
        return Err(refuse("a private mapping is a non-empty whole number of pages, page-aligned on both sides"));
    }
    let mut mirror = mirror();
    if mirror.attachments.iter().any(|&(b, l)| overlaps(b as u64, l as u64, ipa, len as u64))
        || mirror.private.iter().any(|&(i, l)| overlaps(i, l as u64, ipa, len as u64))
    {
        return Err(refuse("it overlaps a range already mapped at stage 2"));
    }
    map(host as usize, ipa as usize, len, flags_of(permission))?;
    mirror.private.push((ipa, len));
    Ok(())
}

pub(super) unsafe fn overlay(ipa_page: u64, host: *mut u8, permission: Stage2) -> HvResult<()> {
    let refuse = |reason| HvError::Range {
        operation: "overlay",
        start: ipa_page,
        end: ipa_page + page() as u64,
        reason,
    };
    if ipa_page % page() as u64 != 0 || (host as usize) % page() != 0 {
        return Err(refuse("an overlay is one page, page-aligned on both sides"));
    }
    let mut mirror = mirror();
    if !mirror.attachments.iter().any(|&(b, l)| ipa_page >= b as u64 && ipa_page < (b + l) as u64) {
        return Err(refuse("an overlay replaces a page of an attachment, and this is not one"));
    }
    let flags = flags_of(permission);
    unmap(ipa_page as usize, page())?;
    map(host as usize, ipa_page as usize, page(), flags)?;
    mirror.overlays.insert(ipa_page, (host as usize, flags));
    Ok(())
}

pub(super) fn remove_overlay(ipa_page: u64) -> HvResult<bool> {
    let mut mirror = mirror();
    if mirror.overlays.remove(&ipa_page).is_none() {
        return Ok(false);
    }
    let start = ipa_page as usize;
    unmap(start, page())?;
    populate(&mirror, start, start + page())?;
    Ok(true)
}

pub(super) fn stage2_stats() -> Stage2Stats {
    Stage2Stats {
        mirrored_changes: MIRRORED.load(Ordering::Relaxed),
        maps: MAPS.load(Ordering::Relaxed),
        unmaps: UNMAPS.load(Ordering::Relaxed),
        failures: FAILURES.load(Ordering::Relaxed),
    }
}

// ------------------------------------------------------------------------------------------- vCPUs

static LIVE_VCPUS: AtomicU32 = AtomicU32::new(0);

pub(super) fn live_vcpus() -> u32 {
    LIVE_VCPUS.load(Ordering::Relaxed)
}

pub(super) struct VcpuInner {
    id: u64,
    exit: *const HvVcpuExit,
}

pub(super) fn vcpu_create() -> HvResult<VcpuInner> {
    let max = limits().max_vcpus;
    let live = LIVE_VCPUS.fetch_add(1, Ordering::SeqCst);
    if live >= max {
        LIVE_VCPUS.fetch_sub(1, Ordering::SeqCst);
        return Err(HvError::VcpuLimit { live, max });
    }
    let mut id = 0u64;
    let mut exit: *const HvVcpuExit = core::ptr::null();
    // SAFETY: both out-parameters are locals; a null config selects the defaults.
    let code = unsafe { hv_vcpu_create(&mut id, &mut exit, core::ptr::null_mut()) };
    if let Err(error) = check("hv_vcpu_create", code) {
        LIVE_VCPUS.fetch_sub(1, Ordering::SeqCst);
        return Err(error);
    }
    Ok(VcpuInner { id, exit })
}

fn x_index(reg: Reg) -> HvResult<u32> {
    match reg {
        Reg::X(n) if n <= 30 => Ok(u32::from(n)),
        Reg::X(n) => Err(HvError::Range {
            operation: "a general-purpose register",
            start: u64::from(n),
            end: u64::from(n) + 1,
            reason: "X0-X30 are indices 0-30",
        }),
        Reg::Pc => Ok(HV_REG_PC),
        Reg::Fpcr => Ok(HV_REG_FPCR),
        Reg::Fpsr => Ok(HV_REG_FPSR),
        Reg::Cpsr => Ok(HV_REG_CPSR),
    }
}

fn simd_index(index: u8) -> HvResult<u32> {
    if index > 31 {
        return Err(HvError::Range {
            operation: "a SIMD register",
            start: u64::from(index),
            end: u64::from(index) + 1,
            reason: "Q0-Q31 are indices 0-31",
        });
    }
    Ok(u32::from(index))
}

impl VcpuInner {
    pub(super) fn run(&mut self) -> HvResult<VcpuExit> {
        // SAFETY: `self.id` is this thread's vCPU (the type is not `Send`).
        check("hv_vcpu_run", unsafe { hv_vcpu_run(self.id) })?;
        // SAFETY: the framework owns `exit` for the vCPU's lifetime and fills it before `run`
        // returns; it is read on the owning thread, after the run.
        let exit = unsafe { *self.exit };
        Ok(match exit.reason {
            HV_EXIT_REASON_CANCELED => VcpuExit::Canceled,
            HV_EXIT_REASON_EXCEPTION => VcpuExit::Exception {
                syndrome: exit.exception.syndrome,
                virtual_address: exit.exception.virtual_address,
                physical_address: exit.exception.physical_address,
            },
            HV_EXIT_REASON_VTIMER_ACTIVATED => VcpuExit::VtimerActivated,
            other => VcpuExit::Unknown(other),
        })
    }

    pub(super) fn reg(&self, reg: Reg) -> HvResult<u64> {
        let index = x_index(reg)?;
        let mut value = 0;
        // SAFETY: the owning thread; `value` is a local.
        check("hv_vcpu_get_reg", unsafe { hv_vcpu_get_reg(self.id, index, &mut value) })?;
        Ok(value)
    }

    pub(super) fn set_reg(&mut self, reg: Reg, value: u64) -> HvResult<()> {
        let index = x_index(reg)?;
        // SAFETY: the owning thread.
        check("hv_vcpu_set_reg", unsafe { hv_vcpu_set_reg(self.id, index, value) })
    }

    pub(super) fn sys_reg(&self, reg: SysReg) -> HvResult<u64> {
        let mut value = 0;
        // SAFETY: the owning thread; `value` is a local.
        check("hv_vcpu_get_sys_reg", unsafe {
            hv_vcpu_get_sys_reg(self.id, reg.encoding(), &mut value)
        })?;
        Ok(value)
    }

    pub(super) fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> HvResult<()> {
        // SAFETY: the owning thread.
        check("hv_vcpu_set_sys_reg", unsafe { hv_vcpu_set_sys_reg(self.id, reg.encoding(), value) })
    }

    pub(super) fn simd(&self, index: u8) -> HvResult<u128> {
        let index = simd_index(index)?;
        let mut value = Q([0; 16]);
        // SAFETY: the owning thread; `value` is sixteen aligned bytes, the framework's vector type.
        check("hv_vcpu_get_simd_fp_reg", unsafe { hv_vcpu_get_simd_fp_reg(self.id, index, &mut value) })?;
        // Lane 0 is the least significant byte, as it is in `Q{n}`.
        Ok(u128::from_le_bytes(value.0))
    }

    pub(super) fn set_simd(&mut self, index: u8, value: u128) -> HvResult<()> {
        let index = simd_index(index)?;
        // SAFETY: the owning thread.
        check("hv_vcpu_set_simd_fp_reg", unsafe { set_simd_raw(self.id, index, value) })
    }

    pub(super) fn set_vtimer_mask(&mut self, masked: bool) -> HvResult<()> {
        // SAFETY: the owning thread.
        check("hv_vcpu_set_vtimer_mask", unsafe { hv_vcpu_set_vtimer_mask(self.id, masked) })
    }

    pub(super) fn vtimer_offset(&self) -> HvResult<u64> {
        let mut offset = 0;
        // SAFETY: the owning thread; `offset` is a local.
        check("hv_vcpu_get_vtimer_offset", unsafe { hv_vcpu_get_vtimer_offset(self.id, &mut offset) })?;
        Ok(offset)
    }
}

/// `hv_vcpu_set_simd_fp_reg(vcpu, reg, value)` with `value` in `Q0`, per AAPCS64 for a 16-byte short
/// vector, which stable Rust cannot express as an FFI signature.
///
/// # Safety
///
/// `vcpu` must be the calling thread's vCPU.
unsafe fn set_simd_raw(vcpu: u64, reg: u32, value: u128) -> u32 {
    let bytes = value.to_le_bytes();
    let function = hv_vcpu_set_simd_fp_reg as unsafe extern "C" fn();
    let result: u64;
    // SAFETY: a call to a C function with the C ABI's argument registers loaded as its prototype
    // says -- `X0` the vCPU, `W1` the register, `Q0` the vector (lane 0 is the lowest byte, as in
    // `to_le_bytes`) -- and every caller-saved register declared clobbered. No `nostack`, so the
    // compiler keeps nothing below `SP` across the call.
    unsafe {
        core::arch::asm!(
            "ldr q0, [{bytes}]",
            "blr {function}",
            bytes = in(reg) bytes.as_ptr(),
            function = in(reg) function,
            inlateout("x0") vcpu => result,
            in("x1") u64::from(reg),
            clobber_abi("C"),
        );
    }
    result as u32
}

impl Drop for VcpuInner {
    fn drop(&mut self) {
        // SAFETY: the owning thread (the type is not `Send`), destroying the vCPU exactly once.
        unsafe { hv_vcpu_destroy(self.id) };
        LIVE_VCPUS.fetch_sub(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------------------------------ misc

pub(super) fn counter_now() -> u64 {
    // SAFETY: no arguments, no side effects.
    unsafe { mach_absolute_time() }
}

pub(super) fn counter_frequency() -> u64 {
    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
        // SAFETY: writes the struct this frame owns.
        unsafe { mach_timebase_info(&mut info) };
        if info.numer == 0 {
            return 0;
        }
        // ticks * numer / denom = nanoseconds.
        1_000_000_000u64 * u64::from(info.denom) / u64::from(info.numer)
    })
}

pub(super) unsafe fn icache_invalidate(address: *mut u8, len: usize) {
    // SAFETY: the caller guarantees the range is mapped readable.
    unsafe { sys_icache_invalidate(address.cast(), len) }
}
