//! The native backend: guest ARM64 code runs **on the host's own cores**, at EL0 in a
//! Hypervisor.framework VM, behind the same `GuestCpu` trait as the translating backend.
//!
//! Behind the non-default `native-hvf` feature and `target_arch = "aarch64"`. The design, and every
//! measured fact it stands on, is `docs/ports/macos-hvf.md`; what follows is the short form a reader
//! of this file needs.
//!
//! * **EL0 guest, host-owned EL1.** EL1 is sixteen `hvc #slot` vectors in a page the guest cannot
//!   reach ([`system::SYSTEM_IPA`]). Every synchronous exception from the guest -- `svc`, `brk`, an
//!   undefined instruction, a stage-1 fault, a trapped system register -- exits to the host through
//!   slot 8, and the host resumes the guest by writing `PC` and `CPSR`.
//! * **Stage 1 is flat, stage 2 is the truth, IPA == VA (D4).** The guest space is attached to the
//!   VM with `omni_platform::hypervisor::Vm::attach`, which keeps stage 2 equal to the host's
//!   protection through every later `mmap`/`mprotect`/decommit. A lazily committed page is therefore
//!   unmapped at stage 2, and the guest's first touch is a stage-2 abort this backend answers with
//!   [`omni_mem::admit`] -- the demand pager's own policy -- and a retry (D10).
//! * **Thunks and the return sentinel are addresses that trap.** A page holding one is overlaid at
//!   stage 2 with a page of `BRK #0xF00D` ([`system::VENEER_BRK`]) when the guest's own page there is
//!   not executable; see [`NativeCpu::add_thunk`] for when that is refused.
//! * **No counting, a vtimer watchdog.** `RunLimit::Instructions` is refused;
//!   [`HaltHandle`](crate::HaltHandle) is honoured within one [`NativeOptions::watchdog_tick`].
//! * **One vCPU per host thread** (the framework binds a vCPU to its creating thread). A context's
//!   registers live in the context between runs and are loaded into the running thread's vCPU at
//!   `run` and saved at every exit.

mod system;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use omni_mem::{DemandPager, FaultAccess, GuestAddr, GuestSpace, RegionKind};
use omni_platform::hypervisor::{self as hv, Attachment, Reg, Stage2, SysReg, VcpuExit, Vm};

use crate::context::{ContextCost, GuestAddressSpace, GuestRange, GuestThreadConfig};
use crate::cpu::{Capabilities, GuestCpu, GuestCpuBackend, HaltHandle, InlineThunkCounts};
use crate::error::{CpuError, CpuResult};
use crate::exit::{AccessKind, ExitReason, RunLimit};
use crate::regs::{Nzcv, VReg, XReg};
use crate::thunk::{ThunkContext, ThunkFn};
use crate::tls::{GuestTls, TlsArena};

use system::{hv_err, with_thread_vcpu, LOWER_EL_SYNC_SLOT, VENEER_BRK};

/// This backend's name, as `GuestCpu::backend_name` reports it.
pub const BACKEND_NAME: &str = "native-hvf";

/// How the native backend is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeOptions {
    /// How many guest threads the TLS arena is sized for. Independent of the vCPU limit: a guest
    /// thread holds a vCPU only through the host thread that runs it.
    pub max_threads: u32,
    /// How often a running guest is interrupted to look at its [`HaltHandle`](crate::HaltHandle):
    /// the bound on halt latency. Each tick is one VM exit (~0.8 us, MEASURED), so 1 ms costs under
    /// 0.1% of a core.
    pub watchdog_tick: Duration,
}

impl Default for NativeOptions {
    fn default() -> Self {
        Self { max_threads: 64, watchdog_tick: Duration::from_millis(1) }
    }
}

/// Exception classes, `ESR_ELx.EC`.
mod ec {
    pub(super) const UNKNOWN: u64 = 0x00;
    pub(super) const WFX: u64 = 0x01;
    pub(super) const SVC64: u64 = 0x15;
    pub(super) const HVC64: u64 = 0x16;
    pub(super) const SYS64: u64 = 0x18;
    pub(super) const INSTRUCTION_ABORT_LOWER: u64 = 0x20;
    pub(super) const PC_ALIGNMENT: u64 = 0x22;
    pub(super) const DATA_ABORT_LOWER: u64 = 0x24;
    pub(super) const SP_ALIGNMENT: u64 = 0x26;
    pub(super) const BRK64: u64 = 0x3C;
}

/// `ISS.WnR` of a data abort: the access was a write.
const ISS_WNR: u64 = 1 << 6;
/// `ISS.S1PTW` of a stage-2 abort: it happened walking stage-1 tables, which are this backend's own.
const ISS_S1PTW: u64 = 1 << 7;

/// `MRS Xt, CNTPCT_EL0`'s `ISS` for a trapped system-register access, `Rt` and direction masked
/// out: `Op0 = 3, Op2 = 1, Op1 = 3, CRn = 14, CRm = 0`.
const ISS_SYSREG_MASK: u64 = 0x3F_FC1E;
const ISS_CNTPCT_EL0: u64 = (3 << 20) | (1 << 17) | (3 << 14) | (14 << 10);

/// What every context of one guest address space shares.
struct Shared {
    space: Arc<GuestSpace>,
    extent: GuestAddressSpace,
    tls: TlsArena,
    options: NativeOptions,
    /// Host-side guest faults (a handler touching a lazily committed page) still need the pager.
    _pager: Option<DemandPager>,
    owns_guest_paging: bool,
    /// Pages overlaid with a `BRK` veneer, and the veneer page each points at. Process-wide in
    /// effect (stage 2 is the VM's), so kept here and never undone while the backend lives.
    veneers: parking_lot::Mutex<BTreeMap<GuestAddr, VeneerPage>>,
    /// Stage-2 mirror failures already seen when this backend was made. More than this means stage 2
    /// may no longer be the host's truth, and `run` refuses.
    stage2_failures: u64,
    /// Declared last so it is dropped after `veneers`' pages are no longer referenced... and before
    /// nothing: dropping it unmaps the whole guest space, overlays included, from stage 2.
    _attachment: Attachment,
}

/// One page of `BRK` words, owned by the backend while an overlay points at it.
struct VeneerPage(*mut u8, std::alloc::Layout);

// SAFETY: a heap page this backend owns and only ever reads through stage 2; nothing on the host
// dereferences it after it is filled.
unsafe impl Send for VeneerPage {}

impl Drop for Shared {
    fn drop(&mut self) {
        // The attachment is a field and is dropped after this body; the veneer pages must outlive
        // the stage-2 mappings that point at them, so they are unmapped here first.
        let vm = Vm::get();
        let mut veneers = self.veneers.lock();
        for (&page, _) in veneers.iter() {
            if let Ok(vm) = vm {
                let _ = vm.remove_overlay(page as u64);
            }
        }
        for (_, VeneerPage(ptr, layout)) in std::mem::take(&mut *veneers) {
            // SAFETY: allocated with this layout by `ensure_trap`, and no stage-2 mapping refers to it
            // any more (removed just above).
            unsafe { std::alloc::dealloc(ptr, layout) };
        }
    }
}

/// Makes [`NativeCpu`] contexts over one guest address space.
pub struct NativeBackend {
    shared: Arc<Shared>,
}

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);

impl NativeBackend {
    /// Bring the native backend up over `space`.
    ///
    /// # Errors
    ///
    /// [`CpuError::Unsupported`] off Apple silicon, or when the space reaches past this host's
    /// guest-physical address size (36 bits on the M1: IPA == VA means the space must lie below
    /// 64 GiB). [`CpuError::Backend`] when the VM cannot be created -- **including a process not signed
    /// with `com.apple.security.hypervisor`**, which the error names -- or stage 2 cannot be set up.
    /// [`CpuError::Memory`] if the TLS arena could not be reserved.
    pub fn new(space: Arc<GuestSpace>, options: NativeOptions) -> CpuResult<Self> {
        let extent = GuestAddressSpace::of(&space)?;
        system::system()?;
        let vm = Vm::get().map_err(hv_err("create the virtual machine"))?;
        let ipa_bits = vm.limits().ipa_bits;
        if extent.end() as u64 > 1u64 << ipa_bits {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "attach the guest address space",
                reason: "the space reaches past this host's guest-physical address size, and the \
                         guest address is used as the guest-physical one (IPA == VA, D4); place the \
                         space lower (the M1 allows 36 bits, 64 GiB)",
            });
        }
        let tls = TlsArena::new(&space, options.max_threads.max(1) as usize)?;
        let pager = match DemandPager::install(Arc::clone(&space)) {
            Ok(pager) => Some(pager),
            Err(e) if e.is_unsupported() => None,
            Err(e) => {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "install the guest demand pager",
                    detail: format!(
                        "{e}. The host side of this runtime touches lazily committed guest memory, \
                         and needs the pager to commit it"
                    ),
                })
            }
        };
        let owns_guest_paging = pager.is_some();
        let stage2_failures = vm.stage2_stats().failures;
        let attachment =
            vm.attach(space.base(), space.len()).map_err(hv_err("attach the guest space to stage 2"))?;
        Ok(Self {
            shared: Arc::new(Shared {
                space,
                extent,
                tls,
                options,
                _pager: pager,
                owns_guest_paging,
                veneers: parking_lot::Mutex::new(BTreeMap::new()),
                stage2_failures,
                _attachment: attachment,
            }),
        })
    }

    /// Whether host-side faults on lazily committed guest memory are resolved (the demand pager is
    /// installed). Guest-side first touches are resolved by this backend itself either way.
    #[must_use]
    pub fn owns_guest_paging(&self) -> bool {
        self.shared.owns_guest_paging
    }

    /// The options in force.
    #[must_use]
    pub fn options(&self) -> NativeOptions {
        self.shared.options
    }

    /// The TLS arena every guest thread's block comes from (one stack guard per space, D13).
    #[must_use]
    pub fn tls(&self) -> &TlsArena {
        &self.shared.tls
    }

    /// Pages currently overlaid with a thunk veneer.
    #[must_use]
    pub fn veneer_pages(&self) -> usize {
        self.shared.veneers.lock().len()
    }

    /// Bring up a context with a freshly allocated bionic TLS block (D13).
    ///
    /// # Errors
    ///
    /// [`CpuError::Memory`] if the block could not be committed, or `Unsupported` when the arena is
    /// full.
    pub fn create_thread_with_tls(&self) -> CpuResult<NativeCpu> {
        let tls = self.shared.tls.allocate(&self.shared.space)?;
        let config = GuestThreadConfig::new(self.shared.extent, tls.thread_pointer())?;
        Ok(NativeCpu::new(Arc::clone(&self.shared), config, Some(tls)))
    }
}

impl GuestCpuBackend for NativeBackend {
    fn name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn create_thread(&self, config: GuestThreadConfig) -> CpuResult<Box<dyn GuestCpu>> {
        Ok(Box::new(NativeCpu::new(Arc::clone(&self.shared), config, None)))
    }

    fn create_guest_thread(&self) -> CpuResult<Box<dyn GuestCpu>> {
        Ok(Box::new(self.create_thread_with_tls()?))
    }

    fn shared_cost(&self) -> ContextCost {
        self.shared.tls.cost()
    }
}

impl core::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeBackend")
            .field("extent", &self.shared.extent)
            .field("options", &self.shared.options)
            .finish_non_exhaustive()
    }
}

/// The guest register file, as it stands between runs.
#[derive(Clone)]
struct RegFile {
    x: [u64; 31],
    sp: u64,
    pc: u64,
    /// `PSTATE.NZCV`, in bits 31:28.
    nzcv: u64,
    v: [u128; 32],
    fpcr: u64,
    fpsr: u64,
    tpidr_el0: u64,
    tpidrro_el0: u64,
}

/// Which registers a setter changed since the vCPU last held them, so a re-entry on the same thread
/// writes only those.
#[derive(Default, Clone, Copy)]
struct Dirty {
    x: u32,
    v: u32,
    sp: bool,
    fp: bool,
    tpidr: bool,
}

/// How many times in a row one address may be admitted and fault again before the backend calls
/// stage 2 broken rather than looping. Two, because another thread's decommit can legitimately race
/// one retry.
const MAX_ADMITTED_REFAULTS: u32 = 2;

/// One guest thread's CPU.
pub struct NativeCpu {
    shared: Arc<Shared>,
    serial: u64,
    regs: RegFile,
    dirty: Dirty,
    thunks: BTreeSet<GuestAddr>,
    sentinel: Option<GuestAddr>,
    halt: HaltHandle,
    tls: Option<GuestTls>,
    cost: ContextCost,
    exits: ExitCounts,
}

/// What a context's runs have exited for, so a measurement can say what it measured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExitCounts {
    /// Exits through the EL1 vector (thunks, sentinel, `svc`, undefined instructions, stage-1 faults).
    pub vector: u64,
    /// Stage-2 aborts that `admit` resolved (a demand-paged first touch) and were retried.
    pub demand_faults: u64,
    /// Watchdog ticks.
    pub vtimer: u64,
    /// `WFI`s and trapped counter reads served in place.
    pub emulated: u64,
}

impl NativeCpu {
    fn new(shared: Arc<Shared>, config: GuestThreadConfig, tls: Option<GuestTls>) -> Self {
        let tp = config.tpidr_el0() as u64;
        let tls_bytes = tls.as_ref().map_or(0, GuestTls::len);
        Self {
            shared,
            serial: NEXT_SERIAL.fetch_add(1, Ordering::Relaxed),
            regs: RegFile {
                x: [0; 31],
                sp: 0,
                pc: 0,
                nzcv: 0,
                v: [0; 32],
                fpcr: 0,
                fpsr: 0,
                // D13: programmed before the context exists, so there is no window without one.
                tpidr_el0: tp,
                tpidrro_el0: tp,
            },
            dirty: Dirty::default(),
            thunks: BTreeSet::new(),
            sentinel: None,
            halt: HaltHandle::new(),
            tls,
            cost: ContextCost {
                // The guest's TLS block and this context's register file. The vCPU itself is the
                // host thread's, not the context's; its footprint is measured per thread in
                // `tests/native.rs` and reported in docs/ports/macos-hvf.md, not folded in here.
                private_committed: tls_bytes.saturating_add(core::mem::size_of::<RegFile>()),
                shared_committed: 0,
            },
            exits: ExitCounts::default(),
        }
    }

    /// What this context's runs have exited for.
    #[must_use]
    pub fn exit_counts(&self) -> ExitCounts {
        self.exits
    }

    /// The bionic TLS block this context owns, if it allocated one.
    #[must_use]
    pub fn tls(&self) -> Option<&GuestTls> {
        self.tls.as_ref()
    }

    /// Read `TPIDRRO_EL0`.
    #[must_use]
    pub fn tpidrro_el0(&self) -> GuestAddr {
        self.regs.tpidrro_el0 as GuestAddr
    }

    /// Make `address` trap for this backend: nothing to do where the guest's own word already traps,
    /// a `BRK` veneer page overlaid at stage 2 where the guest's page is not executable, and a refusal
    /// where it is executable code or writable data.
    fn ensure_trap(&self, address: GuestAddr) -> CpuResult<()> {
        let page = self.shared.space.page_size();
        let page_start = address & !(page - 1);
        if self.shared.veneers.lock().contains_key(&page_start) {
            return Ok(());
        }
        if !self.shared.extent.contains(address) {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "make an address outside the guest space trap",
                reason: "a trap is a stage-2 overlay of a page of the attached guest space, and this \
                         address is not in it",
            });
        }
        match self.shared.space.region_at(address) {
            Some(region) if region.protection.is_executable() => {
                // Real code there. It traps already only if the word is an undefined encoding
                // (`UDF`, whose top half is zero) or a `BRK`; otherwise a veneer would break the code
                // beside it and planting one is not built.
                let word = self.read_word(address);
                return match word {
                    Some(word) if word & 0xFFFF_0000 == 0 || word & 0xFFE0_001F == 0xD420_0000 => Ok(()),
                    _ => Err(CpuError::Unsupported {
                        backend: BACKEND_NAME,
                        operation: "make an address in executable guest code trap",
                        reason: "the page holds code that is not a trap, so it cannot be overlaid \
                                 without breaking the instructions beside it, and planting a BRK \
                                 into guest code is not built in this backend",
                    }),
                };
            }
            Some(region) if region.protection.is_writable() => {
                return Err(CpuError::Unsupported {
                    backend: BACKEND_NAME,
                    operation: "make an address in writable guest data trap",
                    reason: "a veneer overlay would hide the guest's own data on that page from it",
                });
            }
            // Read-only, inaccessible, or free: nothing the guest can execute is there, and a
            // guest load from the page is the only thing the overlay changes (it reads BRK words).
            _ => {}
        }
        let layout = std::alloc::Layout::from_size_align(page, page).map_err(|_| CpuError::Backend {
            backend: BACKEND_NAME,
            operation: "lay out a veneer page",
            detail: format!("{page} is not a usable page size"),
        })?;
        // SAFETY: a non-zero size.
        let host = unsafe { std::alloc::alloc(layout) };
        if host.is_null() {
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "allocate a veneer page",
                detail: "the allocator returned null".into(),
            });
        }
        for i in 0..page / 4 {
            // SAFETY: inside the page just allocated.
            unsafe { host.cast::<u32>().add(i).write(VENEER_BRK) };
        }
        let vm = Vm::get().map_err(hv_err("overlay a veneer page"))?;
        // SAFETY: the page stays allocated until `Shared::drop` removes the overlay and frees it.
        if let Err(error) = unsafe { vm.overlay(page_start as u64, host, Stage2::READ_EXECUTE) } {
            // SAFETY: allocated just above with this layout and never mapped.
            unsafe { std::alloc::dealloc(host, layout) };
            return Err(hv_err("overlay a veneer page")(error));
        }
        self.shared.veneers.lock().insert(page_start, VeneerPage(host, layout));
        Ok(())
    }

    /// The 32-bit word at `pc`, if the host can read it without faulting.
    fn read_word(&self, pc: GuestAddr) -> Option<u32> {
        let region = self.shared.space.region_at(pc)?;
        let readable = region.protection.is_readable()
            && (matches!(region.kind, RegionKind::File { .. }) || region.is_committed());
        if !readable || pc % 4 != 0 {
            return None;
        }
        let ptr = self.shared.space.ptr(pc, 4).ok()?;
        // SAFETY: a readable, committed (or file-backed) range of the guest space, 4-aligned; the
        // guest may be writing it concurrently, which a volatile read of one word tolerates.
        Some(unsafe { ptr.cast::<u32>().read_volatile() })
    }

    /// Load this context into the thread's vCPU: everything, or only what changed if it is still
    /// the one there.
    fn load(&mut self, thread: &mut system::ThreadVcpu) -> Result<(), hv::HvError> {
        let cpu = &mut thread.cpu;
        let full = thread.loaded != self.serial;
        let r = &self.regs;
        for i in 0..31u8 {
            if full || self.dirty.x & (1 << i) != 0 {
                cpu.set_reg(Reg::X(i), r.x[i as usize])?;
            }
        }
        for i in 0..32u8 {
            if full || self.dirty.v & (1 << i) != 0 {
                cpu.set_simd(i, r.v[i as usize])?;
            }
        }
        if full || self.dirty.sp {
            cpu.set_sys_reg(SysReg::SpEl0, r.sp)?;
        }
        if full || self.dirty.fp {
            cpu.set_reg(Reg::Fpcr, r.fpcr)?;
            cpu.set_reg(Reg::Fpsr, r.fpsr)?;
        }
        if full || self.dirty.tpidr {
            cpu.set_sys_reg(SysReg::TpidrEl0, r.tpidr_el0)?;
            cpu.set_sys_reg(SysReg::TpidrroEl0, r.tpidrro_el0)?;
        }
        cpu.set_reg(Reg::Pc, r.pc)?;
        // EL0t, interrupts unmasked (nothing is ever delivered), condition flags as the guest left
        // them.
        cpu.set_reg(Reg::Cpsr, r.nzcv & u64::from(Nzcv::MASK))?;
        thread.loaded = self.serial;
        self.dirty = Dirty::default();
        Ok(())
    }

    /// Save the vCPU's registers into this context. `pc` and `pstate` are the guest's, which after
    /// an exception to EL1 are `ELR_EL1` and `SPSR_EL1` rather than the vCPU's own.
    fn save(&mut self, thread: &system::ThreadVcpu, pc: u64, pstate: u64) -> Result<(), hv::HvError> {
        let cpu = &thread.cpu;
        for i in 0..31u8 {
            self.regs.x[i as usize] = cpu.reg(Reg::X(i))?;
        }
        for i in 0..32u8 {
            self.regs.v[i as usize] = cpu.simd(i)?;
        }
        self.regs.sp = cpu.sys_reg(SysReg::SpEl0)?;
        self.regs.fpcr = cpu.reg(Reg::Fpcr)?;
        self.regs.fpsr = cpu.reg(Reg::Fpsr)?;
        // The guest may write `TPIDR_EL0` itself at EL0 (`MSR`), so it is read back. `TPIDRRO_EL0`
        // is read-only at EL0 and only this context changes it.
        self.regs.tpidr_el0 = cpu.sys_reg(SysReg::TpidrEl0)?;
        self.regs.pc = pc;
        self.regs.nzcv = pstate & u64::from(Nzcv::MASK);
        Ok(())
    }

    /// Arm the watchdog: the vtimer fires one tick from now.
    ///
    /// The timer is the thread's vCPU's, not the context's, so it is armed when the thread has none
    /// running -- on its first run and after each tick -- rather than on every `run`: a tick that
    /// fires while the vCPU is idle between runs is simply the first exit of the next one. Every
    /// crossing is a `run`, so this is three framework calls fewer per crossing.
    fn arm(&self, thread: &mut system::ThreadVcpu) -> Result<(), hv::HvError> {
        let deadline = hv::counter_now()
            .wrapping_sub(thread.vtimer_offset)
            .wrapping_add(system::ticks(self.shared.options.watchdog_tick));
        thread.cpu.set_sys_reg(SysReg::CntvCvalEl0, deadline)?;
        thread.cpu.set_sys_reg(SysReg::CntvCtlEl0, 1)?;
        thread.cpu.set_vtimer_mask(false)?;
        thread.watchdog_armed = true;
        Ok(())
    }

    fn run_loop(&mut self, thread: &mut system::ThreadVcpu) -> CpuResult<ExitReason> {
        let io = |operation| hv_err(operation);
        self.load(thread).map_err(io("load the guest registers"))?;
        if !thread.watchdog_armed {
            self.arm(thread).map_err(io("arm the watchdog"))?;
        }
        let mut refaults = 0u32;
        let mut last_fault = None;
        loop {
            let exit = thread.cpu.run().map_err(io("run the vCPU"))?;
            match exit {
                VcpuExit::VtimerActivated | VcpuExit::Canceled => {
                    self.exits.vtimer += 1;
                    if exit == VcpuExit::VtimerActivated {
                        // The framework masks the timer on this exit; it stays masked until armed.
                        thread.watchdog_armed = false;
                    }
                    if self.halt.is_requested() {
                        let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                        let pstate = thread.cpu.reg(Reg::Cpsr).map_err(io("read CPSR"))?;
                        self.save(thread, pc, pstate).map_err(io("save the guest registers"))?;
                        return Ok(ExitReason::Halted { pc: pc as GuestAddr });
                    }
                    self.arm(thread).map_err(io("re-arm the watchdog"))?;
                }
                VcpuExit::Exception { syndrome, virtual_address, .. } => {
                    let class = syndrome >> 26;
                    let iss = syndrome & 0x1FF_FFFF;
                    match class {
                        ec::HVC64 => {
                            self.exits.vector += 1;
                            return self.from_el1(thread, iss);
                        }
                        ec::INSTRUCTION_ABORT_LOWER | ec::DATA_ABORT_LOWER => {
                            let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                            if iss & ISS_S1PTW != 0 {
                                return Err(CpuError::Backend {
                                    backend: BACKEND_NAME,
                                    operation: "walk the guest's stage-1 tables",
                                    detail: format!(
                                        "a stage-2 abort at {virtual_address:#x} while walking the \
                                         backend's own translation table (ESR_EL2 {syndrome:#x}, \
                                         PC {pc:#x})"
                                    ),
                                });
                            }
                            let access = if class == ec::INSTRUCTION_ABORT_LOWER {
                                AccessKind::Execute
                            } else if iss & ISS_WNR != 0 {
                                AccessKind::Write
                            } else {
                                AccessKind::Read
                            };
                            let address = virtual_address as GuestAddr;
                            if self.admit(address, access) {
                                // Committed (or already committed by another thread): the mirror has
                                // mapped it, and the instruction is retried.
                                if last_fault == Some((pc, address)) {
                                    refaults += 1;
                                    if refaults > MAX_ADMITTED_REFAULTS {
                                        return Err(CpuError::Backend {
                                            backend: BACKEND_NAME,
                                            operation: "resolve a guest page fault",
                                            detail: format!(
                                                "{access} of {address:#x} at PC {pc:#x} was admitted \
                                                 by the paging policy {refaults} times and faulted \
                                                 at stage 2 each time: stage 2 does not match the \
                                                 host ({:?})",
                                                Vm::get().map(Vm::stage2_stats)
                                            ),
                                        });
                                    }
                                } else {
                                    refaults = 0;
                                }
                                last_fault = Some((pc, address));
                                self.exits.demand_faults += 1;
                                continue;
                            }
                            let pstate = thread.cpu.reg(Reg::Cpsr).map_err(io("read CPSR"))?;
                            self.save(thread, pc, pstate).map_err(io("save the guest registers"))?;
                            return Ok(ExitReason::MemoryFault {
                                pc: pc as GuestAddr,
                                address,
                                access,
                            });
                        }
                        ec::WFX => {
                            // `WFI` (and `WFE`, if the framework traps it): a hint, which the
                            // architecture lets an implementation complete at once. Give the host
                            // thread's slice away, as the translating backend does, and go on.
                            self.exits.emulated += 1;
                            std::thread::yield_now();
                            let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                            thread.cpu.set_reg(Reg::Pc, pc + 4).map_err(io("step past WFI"))?;
                        }
                        ec::SYS64 if iss & ISS_SYSREG_MASK == ISS_CNTPCT_EL0 && iss & 1 == 1 => {
                            // `MRS Xt, CNTPCT_EL0`, which traps to EL2 here. The same counter the
                            // guest's `CNTVCT_EL0` reads, in the same units (`CNTFRQ_EL0`).
                            self.exits.emulated += 1;
                            let rt = ((iss >> 5) & 0x1F) as u8;
                            let value = hv::counter_now().wrapping_sub(thread.vtimer_offset);
                            if rt != 31 {
                                thread.cpu.set_reg(Reg::X(rt), value).map_err(io("write Rt"))?;
                            }
                            let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                            thread.cpu.set_reg(Reg::Pc, pc + 4).map_err(io("step past MRS"))?;
                        }
                        _ => {
                            let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                            let pstate = thread.cpu.reg(Reg::Cpsr).map_err(io("read CPSR"))?;
                            self.save(thread, pc, pstate).map_err(io("save the guest registers"))?;
                            return match self.read_word(pc as GuestAddr) {
                                Some(encoding) if class == ec::SYS64 => {
                                    Ok(ExitReason::UnsupportedInstruction { pc: pc as GuestAddr, encoding })
                                }
                                _ => Err(CpuError::Backend {
                                    backend: BACKEND_NAME,
                                    operation: "classify a guest exit",
                                    detail: format!(
                                        "the guest exited to the host with exception class \
                                         {class:#x} (ESR_EL2 {syndrome:#x}) at PC {pc:#x}, which this \
                                         backend does not handle"
                                    ),
                                }),
                            };
                        }
                    }
                }
                VcpuExit::Unknown(reason) => {
                    return Err(CpuError::Backend {
                        backend: BACKEND_NAME,
                        operation: "run the vCPU",
                        detail: format!("the framework returned exit reason {reason}, which is not documented"),
                    })
                }
            }
            if self.halt.is_requested() {
                let pc = thread.cpu.reg(Reg::Pc).map_err(io("read PC"))?;
                let pstate = thread.cpu.reg(Reg::Cpsr).map_err(io("read CPSR"))?;
                self.save(thread, pc, pstate).map_err(io("save the guest registers"))?;
                return Ok(ExitReason::Halted { pc: pc as GuestAddr });
            }
        }
    }

    /// Ask the paging policy whether the guest may make this access, committing if that is all that
    /// is missing. The same function the demand pager asks (D10), so the two cannot disagree.
    fn admit(&self, address: GuestAddr, access: AccessKind) -> bool {
        if !self.shared.extent.contains(address) {
            return false;
        }
        let access = match access {
            AccessKind::Read => FaultAccess::Read,
            AccessKind::Write => FaultAccess::Write,
            AccessKind::Execute => FaultAccess::Execute,
        };
        omni_mem::admit(&self.shared.space, address, 1, access).is_ok()
    }

    /// An exception the guest took to EL1, which the vector sent on with `hvc #slot`.
    fn from_el1(&mut self, thread: &mut system::ThreadVcpu, slot: u64) -> CpuResult<ExitReason> {
        let io = |operation| hv_err(operation);
        let cpu = &thread.cpu;
        let esr = cpu.sys_reg(SysReg::EsrEl1).map_err(io("read ESR_EL1"))?;
        let elr = cpu.sys_reg(SysReg::ElrEl1).map_err(io("read ELR_EL1"))?;
        let spsr = cpu.sys_reg(SysReg::SpsrEl1).map_err(io("read SPSR_EL1"))?;
        if slot != LOWER_EL_SYNC_SLOT {
            // An exception taken *at* EL1 (the vector page or the tables themselves), or an
            // asynchronous one nothing here generates: the backend's EL1 is broken, not the guest.
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "run guest code",
                detail: format!(
                    "EL1 took an exception through vector slot {slot} (ESR_EL1 {esr:#x}, ELR_EL1 \
                     {elr:#x}, SPSR_EL1 {spsr:#x}); only slot 8, a synchronous exception from EL0, is \
                     expected"
                ),
            });
        }
        let class = esr >> 26;
        let iss = esr & 0x1FF_FFFF;
        let at = elr as GuestAddr;
        // A registered address is a stop whatever trapped there: the veneer's BRK, the UDF of an
        // empty executable page, an instruction abort on it.
        let traps_at_address =
            matches!(class, ec::BRK64 | ec::UNKNOWN | ec::INSTRUCTION_ABORT_LOWER);
        if traps_at_address && self.sentinel == Some(at) {
            self.save(thread, elr, spsr).map_err(io("save the guest registers"))?;
            return Ok(ExitReason::Returned { pc: at });
        }
        if traps_at_address && self.thunks.contains(&at) {
            self.save(thread, elr, spsr).map_err(io("save the guest registers"))?;
            return Ok(ExitReason::Thunk { pc: at });
        }
        let far = cpu.sys_reg(SysReg::FarEl1).map_err(io("read FAR_EL1"))?;
        self.save(thread, elr, spsr).map_err(io("save the guest registers"))?;
        Ok(match class {
            ec::SVC64 => {
                // `ELR_EL1` is the word after the `SVC`. The guest's `PC` is left there, as the
                // translating backend leaves it.
                let site = at.wrapping_sub(4);
                ExitReason::UnsupportedInstruction {
                    pc: site,
                    encoding: 0xD400_0001 | (((iss & 0xFFFF) as u32) << 5),
                }
            }
            ec::BRK64 if (iss & 0xFFFF) as u32 == 0xF00D && self.is_veneer(at) => {
                // A BRK of a veneer at an address nobody registered: a branch into the middle of a
                // thunk slot. It is not code; it is a fetch from a non-executable page of the guest.
                ExitReason::MemoryFault { pc: at, address: at, access: AccessKind::Execute }
            }
            ec::BRK64 => ExitReason::UnsupportedInstruction {
                pc: at,
                encoding: 0xD420_0000 | (((iss & 0xFFFF) as u32) << 5),
            },
            ec::INSTRUCTION_ABORT_LOWER | ec::PC_ALIGNMENT => {
                ExitReason::MemoryFault { pc: at, address: far as GuestAddr, access: AccessKind::Execute }
            }
            ec::DATA_ABORT_LOWER => ExitReason::MemoryFault {
                pc: at,
                address: far as GuestAddr,
                access: if iss & ISS_WNR != 0 { AccessKind::Write } else { AccessKind::Read },
            },
            ec::UNKNOWN | ec::SYS64 | ec::SP_ALIGNMENT => match self.read_word(at) {
                Some(encoding) => ExitReason::UnsupportedInstruction { pc: at, encoding },
                None => {
                    return Err(CpuError::Backend {
                        backend: BACKEND_NAME,
                        operation: "name an unsupported instruction",
                        detail: format!(
                            "exception class {class:#x} at {at:#x}, whose word the host cannot read"
                        ),
                    })
                }
            },
            _ => {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "classify a guest exception",
                    detail: format!(
                        "the guest took exception class {class:#x} (ESR_EL1 {esr:#x}) at {at:#x}, \
                         which this backend does not handle"
                    ),
                })
            }
        })
    }

    /// **For the measurement in `tests/native.rs` only**: time `rounds` full register saves and
    /// `rounds` full loads between this context and the calling thread's vCPU, the two halves of
    /// every crossing's cost that are this backend's rather than the hypervisor's.
    ///
    /// # Errors
    ///
    /// As [`GuestCpu::run`] when the vCPU cannot be created or accessed.
    #[doc(hidden)]
    pub fn measure_register_transfer(&mut self, rounds: u32) -> CpuResult<(Duration, Duration)> {
        with_thread_vcpu(|thread| {
            let io = |operation| hv_err(operation);
            // Loads first, so the vCPU holds this context's registers and every save below writes
            // back exactly what was loaded.
            let started = std::time::Instant::now();
            for _ in 0..rounds.max(1) {
                thread.loaded = 0;
                self.load(thread).map_err(io("load"))?;
            }
            let loading = started.elapsed();
            let started = std::time::Instant::now();
            for _ in 0..rounds {
                let (pc, nzcv) = (self.regs.pc, self.regs.nzcv);
                self.save(thread, pc, nzcv).map_err(io("save"))?;
            }
            Ok((started.elapsed(), loading))
        })
    }

    fn is_veneer(&self, address: GuestAddr) -> bool {
        let page = self.shared.space.page_size();
        self.shared.veneers.lock().contains_key(&(address & !(page - 1)))
    }
}

impl GuestCpu for NativeCpu {
    fn backend_name(&self) -> &'static str {
        BACKEND_NAME
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // Nothing counts native instructions: there is no translator to put a counter in, and
            // the PMU is not exposed to the guest.
            counted_step_limit: false,
            // The vtimer watchdog: a halt is seen within one tick.
            asynchronous_halt: true,
            // Would need a BRK planted in guest code and a single-step over it on resume; not built.
            breakpoints: false,
            // Every thunk is a VM exit (D17's design A); nothing dispatches inside the guest.
            inline_thunks: false,
        }
    }

    fn space(&self) -> GuestAddressSpace {
        self.shared.extent
    }

    fn run(&mut self, from: GuestAddr, limit: RunLimit) -> CpuResult<ExitReason> {
        if limit.instructions().is_some() {
            return Err(CpuError::Unsupported {
                backend: BACKEND_NAME,
                operation: "run for a counted number of guest instructions",
                reason: "guest code runs natively and nothing counts its instructions; bound it with \
                         a HaltHandle (the vtimer watchdog) and RunLimit::Unlimited",
            });
        }
        self.regs.pc = from as u64;
        if self.halt.is_requested() {
            return Ok(ExitReason::Halted { pc: from });
        }
        let failures = Vm::get().map(Vm::stage2_stats).map_or(0, |s| s.failures);
        if failures > self.shared.stage2_failures {
            return Err(CpuError::Backend {
                backend: BACKEND_NAME,
                operation: "run guest code",
                detail: format!(
                    "{} stage-2 mirror call(s) have failed since this backend was made, so stage 2 \
                     may not match the host's protection and no guest code is run against it",
                    failures - self.shared.stage2_failures
                ),
            });
        }
        with_thread_vcpu(|thread| {
            let result = self.run_loop(thread);
            if result.is_err() {
                // Whatever the vCPU holds is not known to be this context's any more.
                thread.loaded = 0;
            }
            result
        })
    }

    fn last_run_instructions(&self) -> u64 {
        // Honest: nothing is counted, and counted budgets are refused.
        0
    }

    fn halt_handle(&self) -> HaltHandle {
        self.halt.clone()
    }

    fn x(&self, reg: XReg) -> u64 {
        self.regs.x[reg.index() as usize]
    }

    fn set_x(&mut self, reg: XReg, value: u64) {
        self.regs.x[reg.index() as usize] = value;
        self.dirty.x |= 1 << reg.index();
    }

    fn sp(&self) -> GuestAddr {
        self.regs.sp as GuestAddr
    }

    fn set_sp(&mut self, value: GuestAddr) {
        self.regs.sp = value as u64;
        self.dirty.sp = true;
    }

    fn pc(&self) -> GuestAddr {
        self.regs.pc as GuestAddr
    }

    fn set_pc(&mut self, value: GuestAddr) {
        // Always written at `run`; nothing to mark.
        self.regs.pc = value as u64;
    }

    fn nzcv(&self) -> Nzcv {
        Nzcv::from_pstate(self.regs.nzcv)
    }

    fn set_nzcv(&mut self, value: Nzcv) {
        // Always written at `run` with `CPSR`.
        self.regs.nzcv = value.to_pstate();
    }

    fn v(&self, reg: VReg) -> u128 {
        self.regs.v[reg.index() as usize]
    }

    fn set_v(&mut self, reg: VReg, value: u128) {
        self.regs.v[reg.index() as usize] = value;
        self.dirty.v |= 1 << reg.index();
    }

    fn tpidr_el0(&self) -> GuestAddr {
        self.regs.tpidr_el0 as GuestAddr
    }

    fn set_tpidr_el0(&mut self, value: GuestAddr) {
        // Both move together, as bionic's kernel keeps them (see the translating backend).
        self.regs.tpidr_el0 = value as u64;
        self.regs.tpidrro_el0 = value as u64;
        self.dirty.tpidr = true;
    }

    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        // The host's `IC IVAU` over whatever part of the range the host can read and the guest can
        // execute. The guest's own maintenance at EL0 runs natively; this is for the runtime's
        // callers (`mprotect`, `munmap`, `dlclose`) and for host writes to guest code.
        let space = &self.shared.space;
        let end = range.end().min(self.shared.extent.end());
        let mut at = range.start().max(self.shared.extent.base());
        while at < end {
            let Some(region) = space.region_at(at) else {
                at = (at | (space.page_size() - 1)) + 1;
                continue;
            };
            let to = region.end().min(end);
            let readable = region.protection.is_readable()
                && (matches!(region.kind, RegionKind::File { .. }) || region.is_committed());
            if readable && region.protection.is_executable() {
                if let Ok(ptr) = space.ptr(at, to - at) {
                    // SAFETY: a readable range of the guest space, just checked.
                    unsafe { hv::icache_invalidate(ptr, to - at) };
                }
            }
            at = to;
        }
        Ok(())
    }

    fn add_thunk(&mut self, address: GuestAddr) -> CpuResult<()> {
        self.ensure_trap(address)?;
        self.thunks.insert(address);
        Ok(())
    }

    fn remove_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        // The veneer page stays: other contexts of this space may still trap there, and a guest that
        // reaches it unregistered gets a typed fault either way.
        Ok(self.thunks.remove(&address))
    }

    fn add_inline_thunk(
        &mut self,
        _address: GuestAddr,
        _handler: ThunkFn,
        _context: ThunkContext,
    ) -> CpuResult<()> {
        Err(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "dispatch a thunk inside the run loop",
            reason: "guest code runs natively, so every thunk is a VM exit; the compatibility layer \
                     services it on the exit path (Capabilities::inline_thunks is false)",
        })
    }

    fn remove_inline_thunk(&mut self, _address: GuestAddr) -> CpuResult<bool> {
        Err(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "remove an inline thunk",
            reason: "this backend has no inline thunks (Capabilities::inline_thunks is false)",
        })
    }

    fn inline_thunk_calls(&self) -> InlineThunkCounts {
        InlineThunkCounts::default()
    }

    fn set_return_sentinel(&mut self, address: GuestAddr) -> CpuResult<()> {
        self.ensure_trap(address)?;
        self.sentinel = Some(address);
        Ok(())
    }

    fn return_sentinel(&self) -> Option<GuestAddr> {
        self.sentinel
    }

    fn add_breakpoint(&mut self, _address: GuestAddr) -> CpuResult<()> {
        Err(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "set a breakpoint",
            reason: "a native breakpoint is a BRK planted in guest code plus a single step over it \
                     on resume, and neither is built (Capabilities::breakpoints is false)",
        })
    }

    fn remove_breakpoint(&mut self, _address: GuestAddr) -> CpuResult<bool> {
        Err(CpuError::Unsupported {
            backend: BACKEND_NAME,
            operation: "remove a breakpoint",
            reason: "this backend has no breakpoints (Capabilities::breakpoints is false)",
        })
    }

    fn cost(&self) -> ContextCost {
        self.cost
    }
}

impl core::fmt::Debug for NativeCpu {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeCpu")
            .field("pc", &format_args!("{:#x}", self.regs.pc))
            .field("tpidr_el0", &format_args!("{:#x}", self.regs.tpidr_el0))
            .field("exits", &self.exits)
            .finish_non_exhaustive()
    }
}
