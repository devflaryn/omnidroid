//! A hardware virtual machine for running **same-ISA** guest code natively: Hypervisor.framework on
//! Apple silicon, behind a shape a native `GuestCpu` backend can use and no other crate can reach
//! the OS through.
//!
//! Behind the non-default `hypervisor` feature. Nothing in the workspace enables it except
//! `omni-cpu`'s `native-hvf`, so a default build neither links the framework nor carries the
//! hook described below.
//!
//! # What is here
//!
//! * [`Vm`] — the process's one virtual machine (Hypervisor.framework allows one per process), its
//!   limits, and **stage 2**: which guest-physical addresses reach which host memory, with what
//!   permission.
//! * [`Attachment`] — a host address range whose stage-2 mapping **mirrors the host's own
//!   protection, at IPA == VA**, and keeps mirroring it. See below; this is the load-bearing part.
//! * [`Vcpu`] — one virtual CPU, bound to the host thread that created it. Registers, system
//!   registers, the virtual timer, and `run`.
//! * [`counter_now`] and [`counter_frequency`] — the host counter the virtual timer compares
//!   against.
//! * [`icache_invalidate`] — make host writes to guest code visible to guest instruction fetch.
//!
//! # Why stage 2 has to be kept in step by this crate, and nowhere else
//!
//! MEASURED on the M1 (C probes, `docs/ports/macos-hvf.md` section 1), two facts about
//! `hv_vm_map`:
//!
//! 1. **Host protection is not enforced at stage 2.** A range mapped read-write is read and written
//!    by the guest while the host has it `PROT_NONE`.
//! 2. **It binds the memory object present when it is called.** After the host replaces a range with
//!    `mmap(MAP_FIXED)` -- a file view over a reservation, or [`vm`](crate::vm)'s decommit, which is
//!    a fresh anonymous mapping -- the guest keeps seeing the *old* pages until the range is unmapped
//!    and mapped again.
//!
//! So a stage 2 set up once would hand the guest every lazily-committed page without asking the
//! demand pager, ignore every `mprotect`, keep decommitted memory alive, and show zeroes where a
//! library was mapped. Every one of those changes is made by [`vm`](crate::vm)'s macOS backend, so
//! that backend calls one hook after each `mmap(MAP_FIXED)`, `mprotect` and `munmap` it makes, and
//! the hook re-establishes stage 2 for whatever part of the range an [`Attachment`] covers, with the
//! protection the host just set. It runs on the thread that made the change, before the change
//! returns to its caller. A range nobody attached costs one atomic load.
//!
//! The consequence is that **stage 2 is the host's protection**: a `PROT_NONE` page -- a lazily
//! committed one, a guard page, a free range -- is unmapped, so a guest touch of it is a stage-2
//! fault the CPU backend can hand to the same policy the demand pager uses.
//!
//! # Scope
//!
//! Implemented on macOS on Apple silicon. Everywhere else every entry point returns
//! [`HvError::Unsupported`] naming what is missing. The process must be signed with the
//! `com.apple.security.hypervisor` entitlement; without it, creating the VM is refused with
//! [`HvError::Denied`], which says so.

use core::fmt;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
mod macos;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use macos as backend;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) use macos::host_mapping_changed;

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
mod unsupported;
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
use unsupported as backend;

/// Why a hypervisor operation did not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HvError {
    /// This host has no hypervisor this seam can drive.
    #[error("hypervisor: {operation} is not available here: {reason}")]
    Unsupported {
        /// What was asked.
        operation: &'static str,
        /// Why it cannot be done here.
        reason: &'static str,
    },
    /// The OS refused. On macOS this is `HV_DENIED`: the process is not signed with the
    /// `com.apple.security.hypervisor` entitlement.
    #[error(
        "hypervisor: {operation} was denied ({code:#010x}): this executable is not signed with the \
         com.apple.security.hypervisor entitlement (tools/hvf_run.sh signs a test binary with it)"
    )]
    Denied {
        /// What was asked.
        operation: &'static str,
        /// The framework's return code.
        code: u32,
    },
    /// The framework returned an error.
    #[error("hypervisor: {operation} failed with {code:#010x} ({name}){detail}")]
    Failed {
        /// What was asked.
        operation: &'static str,
        /// The framework's return code.
        code: u32,
        /// Its name in `hv_error.h`.
        name: &'static str,
        /// Where, when that helps.
        detail: String,
    },
    /// A guest-physical range the hardware cannot address, or one already claimed.
    #[error("hypervisor: {operation} of {start:#x}..{end:#x} refused: {reason}")]
    Range {
        /// What was asked.
        operation: &'static str,
        /// First address.
        start: u64,
        /// One past the last.
        end: u64,
        /// Why.
        reason: &'static str,
    },
    /// The per-VM vCPU limit is reached.
    #[error(
        "hypervisor: no vCPU could be created: {live} are live and this host allows {max} per VM \
         (one VM per process), and every host thread that runs guest code holds one"
    )]
    VcpuLimit {
        /// vCPUs live in this process.
        live: u32,
        /// `hv_vm_get_max_vcpu_count`.
        max: u32,
    },
}

/// Result alias for this module.
pub type HvResult<T> = Result<T, HvError>;

/// What this host's hypervisor can do, read from it rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLimits {
    /// Guest-physical address bits (`hv_vm_config_get_max_ipa_size`). Stage 2 cannot map an IPA at
    /// or above `1 << ipa_bits`, and with IPA == VA that is also the highest guest address.
    pub ipa_bits: u32,
    /// vCPUs per VM (`hv_vm_get_max_vcpu_count`).
    pub max_vcpus: u32,
}

/// Stage-2 permission for memory the caller owns outright (not an [`Attachment`], whose
/// permissions follow the host's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stage2 {
    /// Guest may read.
    pub read: bool,
    /// Guest may write.
    pub write: bool,
    /// Guest may execute.
    pub execute: bool,
}

impl Stage2 {
    /// Read and execute: code.
    pub const READ_EXECUTE: Stage2 = Stage2 { read: true, write: false, execute: true };
    /// Read, write and execute.
    pub const ALL: Stage2 = Stage2 { read: true, write: true, execute: true };
}

/// Counters a test reads to prove the stage-2 mirror did work, rather than inferring it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stage2Stats {
    /// Host mapping changes the hook saw that intersected an attachment.
    pub mirrored_changes: u64,
    /// `hv_vm_map` calls the mirror made.
    pub maps: u64,
    /// `hv_vm_unmap` calls the mirror made.
    pub unmaps: u64,
    /// Calls that failed. **A non-zero value means stage 2 is not the host's truth any more**, and
    /// a CPU backend must refuse to keep running guest code against it.
    pub failures: u64,
}

/// The process's one virtual machine.
///
/// Created on first use and never destroyed: every host thread that ran guest code holds a vCPU in
/// it until that thread exits, and `hv_vm_destroy` requires every vCPU to be gone first.
pub struct Vm {
    _private: (),
}

static VM: Vm = Vm { _private: () };

impl Vm {
    /// The VM, creating it if this is the first call in the process.
    ///
    /// # Errors
    ///
    /// [`HvError::Unsupported`] off Apple silicon, [`HvError::Denied`] without the entitlement,
    /// [`HvError::Failed`] if the framework refuses for another reason. The outcome of the first
    /// attempt is remembered: a process that was refused stays refused.
    pub fn get() -> HvResult<&'static Vm> {
        backend::vm_init()?;
        Ok(&VM)
    }

    /// This host's limits.
    #[must_use]
    pub fn limits(&self) -> HostLimits {
        backend::limits()
    }

    /// Make `[base, base + len)` visible to the guest at IPA == VA, with stage-2 permission equal to
    /// the host's protection **now and after every later change** (see the module docs).
    ///
    /// # Errors
    ///
    /// [`HvError::Range`] for an unaligned range, one reaching past [`HostLimits::ipa_bits`], or one
    /// overlapping another attachment or [`map_private`](Vm::map_private) memory;
    /// [`HvError::Failed`] if the initial mapping failed.
    pub fn attach(&self, base: usize, len: usize) -> HvResult<Attachment> {
        backend::attach(base, len)?;
        Ok(Attachment { base, len })
    }

    /// Map memory the caller owns at an IPA **outside** every attachment -- a CPU backend's exception
    /// vectors and translation tables.
    ///
    /// # Safety
    ///
    /// `host` must point to `len` bytes, page-aligned, that stay allocated for the life of the
    /// process: the mapping is never removed, and the guest reaches it with the given permission.
    ///
    /// # Errors
    ///
    /// [`HvError::Range`] if the IPA range overlaps an attachment; [`HvError::Failed`].
    pub unsafe fn map_private(
        &self,
        host: *mut u8,
        ipa: u64,
        len: usize,
        permission: Stage2,
    ) -> HvResult<()> {
        // SAFETY: the caller's contract.
        unsafe { backend::map_private(host, ipa, len, permission) }
    }

    /// Point one page of an attachment at other memory, and keep it there whatever the host does to
    /// that page until [`remove_overlay`](Vm::remove_overlay).
    ///
    /// For a CPU backend's trap veneers: a page of guest address space that must *execute* as
    /// something the guest's own memory does not hold.
    ///
    /// # Safety
    ///
    /// `host` must point to one page that stays allocated until the overlay is removed or its
    /// attachment dropped.
    ///
    /// # Errors
    ///
    /// [`HvError::Range`] if `ipa_page` is not a page inside an attachment; [`HvError::Failed`].
    pub unsafe fn overlay(&self, ipa_page: u64, host: *mut u8, permission: Stage2) -> HvResult<()> {
        // SAFETY: the caller's contract.
        unsafe { backend::overlay(ipa_page, host, permission) }
    }

    /// Give a page back to the host's protection. Returns whether it was overlaid.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`] if stage 2 could not be restored.
    pub fn remove_overlay(&self, ipa_page: u64) -> HvResult<bool> {
        backend::remove_overlay(ipa_page)
    }

    /// What the stage-2 mirror has done, process-wide.
    #[must_use]
    pub fn stage2_stats(&self) -> Stage2Stats {
        backend::stage2_stats()
    }

    /// vCPUs live in the process.
    #[must_use]
    pub fn live_vcpus(&self) -> u32 {
        backend::live_vcpus()
    }
}

impl fmt::Debug for Vm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm").field("limits", &self.limits()).finish()
    }
}

/// A host range mirrored into stage 2. Dropping it unmaps the range from the guest and removes any
/// overlays inside it.
#[derive(Debug)]
pub struct Attachment {
    base: usize,
    len: usize,
}

impl Attachment {
    /// First address.
    #[must_use]
    pub fn base(&self) -> usize {
        self.base
    }

    /// Length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Always false: an empty attachment is refused.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        backend::detach(self.base, self.len);
    }
}

/// A general-purpose or special register of a [`Vcpu`], `hv_reg_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reg {
    /// `X0`-`X30`. Out-of-range indices are refused by [`Vcpu::reg`].
    X(u8),
    /// The program counter.
    Pc,
    /// `FPCR`.
    Fpcr,
    /// `FPSR`.
    Fpsr,
    /// `CPSR` (`PSTATE`: condition flags, exception level, masks).
    Cpsr,
}

/// A system register of a [`Vcpu`], by its architectural `op0:op1:CRn:CRm:op2` encoding, which is
/// what `hv_sys_reg_t` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum SysReg {
    SpEl0,
    SpEl1,
    TpidrEl0,
    TpidrroEl0,
    SctlrEl1,
    CpacrEl1,
    TcrEl1,
    Ttbr0El1,
    Ttbr1El1,
    MairEl1,
    VbarEl1,
    CntkctlEl1,
    ElrEl1,
    SpsrEl1,
    EsrEl1,
    FarEl1,
    CntvCtlEl0,
    CntvCvalEl0,
}

impl SysReg {
    /// The `hv_sys_reg_t` value: `op0 << 14 | op1 << 11 | CRn << 7 | CRm << 3 | op2`.
    #[must_use]
    pub const fn encoding(self) -> u16 {
        const fn enc(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
            (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
        }
        match self {
            SysReg::SpEl0 => enc(3, 0, 4, 1, 0),
            SysReg::SpEl1 => enc(3, 4, 4, 1, 0),
            SysReg::TpidrEl0 => enc(3, 3, 13, 0, 2),
            SysReg::TpidrroEl0 => enc(3, 3, 13, 0, 3),
            SysReg::SctlrEl1 => enc(3, 0, 1, 0, 0),
            SysReg::CpacrEl1 => enc(3, 0, 1, 0, 2),
            SysReg::TcrEl1 => enc(3, 0, 2, 0, 2),
            SysReg::Ttbr0El1 => enc(3, 0, 2, 0, 0),
            SysReg::Ttbr1El1 => enc(3, 0, 2, 0, 1),
            SysReg::MairEl1 => enc(3, 0, 10, 2, 0),
            SysReg::VbarEl1 => enc(3, 0, 12, 0, 0),
            SysReg::CntkctlEl1 => enc(3, 0, 14, 1, 0),
            SysReg::ElrEl1 => enc(3, 0, 4, 0, 1),
            SysReg::SpsrEl1 => enc(3, 0, 4, 0, 0),
            SysReg::EsrEl1 => enc(3, 0, 5, 2, 0),
            SysReg::FarEl1 => enc(3, 0, 6, 0, 0),
            SysReg::CntvCtlEl0 => enc(3, 3, 14, 3, 1),
            SysReg::CntvCvalEl0 => enc(3, 3, 14, 3, 2),
        }
    }
}

/// Why [`Vcpu::run`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcpuExit {
    /// Another thread asked (`hv_vcpus_exit`).
    Canceled,
    /// A synchronous exception to EL2: an `hvc`, a stage-2 abort, a trapped instruction.
    Exception {
        /// `ESR_EL2`.
        syndrome: u64,
        /// `FAR_EL2`: the faulting virtual address, for an abort.
        virtual_address: u64,
        /// The faulting IPA, for a stage-2 abort.
        physical_address: u64,
    },
    /// The virtual timer fired. It stays masked until [`Vcpu::set_vtimer_mask`]`(false)`.
    VtimerActivated,
    /// A reason this seam does not know, with its raw value.
    Unknown(u32),
}

/// One virtual CPU.
///
/// **Bound to the thread that created it** -- every `hv_vcpu_*` call must come from that thread,
/// and a thread may hold only one -- so this type is neither `Send` nor `Sync`. Dropping it destroys
/// the vCPU.
pub struct Vcpu {
    inner: backend::VcpuInner,
    _bound: core::marker::PhantomData<*const ()>,
}

impl Vcpu {
    /// Create a vCPU for the calling thread, in the process's [`Vm`].
    ///
    /// # Errors
    ///
    /// [`HvError::VcpuLimit`] at the per-VM limit; [`HvError::Failed`] if the thread already holds
    /// one or the framework refuses; the [`Vm::get`] errors.
    pub fn create() -> HvResult<Vcpu> {
        Vm::get()?;
        Ok(Vcpu { inner: backend::vcpu_create()?, _bound: core::marker::PhantomData })
    }

    /// Run until the next exit.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`] if the framework could not run the vCPU at all -- distinct from the guest
    /// stopping, which is a [`VcpuExit`].
    pub fn run(&mut self) -> HvResult<VcpuExit> {
        self.inner.run()
    }

    /// Read a register.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`]; [`HvError::Range`] for `X(n)` with `n > 30`.
    pub fn reg(&self, reg: Reg) -> HvResult<u64> {
        self.inner.reg(reg)
    }

    /// Write a register.
    ///
    /// # Errors
    ///
    /// As [`reg`](Vcpu::reg).
    pub fn set_reg(&mut self, reg: Reg, value: u64) -> HvResult<()> {
        self.inner.set_reg(reg, value)
    }

    /// Read a system register.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`].
    pub fn sys_reg(&self, reg: SysReg) -> HvResult<u64> {
        self.inner.sys_reg(reg)
    }

    /// Write a system register.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`].
    pub fn set_sys_reg(&mut self, reg: SysReg, value: u64) -> HvResult<()> {
        self.inner.set_sys_reg(reg, value)
    }

    /// Read `Q{index}` as 128 bits.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`]; [`HvError::Range`] for `index > 31`.
    pub fn simd(&self, index: u8) -> HvResult<u128> {
        self.inner.simd(index)
    }

    /// Write `Q{index}`.
    ///
    /// # Errors
    ///
    /// As [`simd`](Vcpu::simd).
    pub fn set_simd(&mut self, index: u8, value: u128) -> HvResult<()> {
        self.inner.set_simd(index, value)
    }

    /// Mask or unmask the virtual timer's exit. See [`VcpuExit::VtimerActivated`].
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`].
    pub fn set_vtimer_mask(&mut self, masked: bool) -> HvResult<()> {
        self.inner.set_vtimer_mask(masked)
    }

    /// `CNTVCT_EL0 = counter_now() - offset` for this vCPU.
    ///
    /// # Errors
    ///
    /// [`HvError::Failed`].
    pub fn vtimer_offset(&self) -> HvResult<u64> {
        self.inner.vtimer_offset()
    }
}

impl fmt::Debug for Vcpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vcpu").finish_non_exhaustive()
    }
}

/// The host counter a vCPU's virtual counter is derived from (`mach_absolute_time` on macOS), in
/// ticks of [`counter_frequency`].
#[must_use]
pub fn counter_now() -> u64 {
    backend::counter_now()
}

/// Ticks per second of [`counter_now`].
#[must_use]
pub fn counter_frequency() -> u64 {
    backend::counter_frequency()
}

/// Make `len` bytes of freshly written code at `address` visible to instruction fetch, on every
/// core, for the host and the guest alike (they share physical pages).
///
/// # Safety
///
/// The range must be mapped readable in this process.
pub unsafe fn icache_invalidate(address: *mut u8, len: usize) {
    // SAFETY: the caller's contract.
    unsafe { backend::icache_invalidate(address, len) }
}
