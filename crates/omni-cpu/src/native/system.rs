//! What the guest's EL1 is: one page of exception vectors and one stage-1 translation table, the same
//! for every vCPU in the process, and the per-host-thread vCPU that loads them.
//!
//! See `docs/ports/macos-hvf.md` section 2 for why the guest runs at EL0 under this, and why stage 1
//! is flat while stage 2 carries every permission.

use std::cell::RefCell;
use std::sync::OnceLock;

use omni_platform::hypervisor::{self as hv, HvError, Reg, Stage2, SysReg, Vcpu, Vm};

use crate::error::{CpuError, CpuResult};

use super::BACKEND_NAME;

/// Where the backend's own page sits in guest-physical (and guest-virtual) space.
///
/// Below 4 GiB, which on an arm64 macOS executable is `__PAGEZERO`: no host mapping, and therefore
/// no guest address (IPA == VA), can ever be there. Stage 1 maps its 32 MiB block EL1-only.
pub(crate) const SYSTEM_IPA: u64 = 0x1000_0000;

/// The trap every word of a thunk veneer page holds: `BRK #0xF00D`.
///
/// `BRK` rather than `SVC` because its exception names the instruction itself in `ELR_EL1` (an `SVC`'s
/// is the next word), and its immediate lands in `ESR_EL1.ISS`, so a guest's own `BRK #0xF00D` at an
/// address nobody registered is told apart by the address, exactly as `add_thunk` defines a thunk.
pub(crate) const VENEER_BRK: u32 = 0xD420_0000 | (0xF00D << 5);

/// `hvc #n` for vector slot `n`.
const fn hvc(n: u32) -> u32 {
    0xD400_0002 | (n << 5)
}

/// The slot a synchronous exception from AArch64 EL0 lands in (`VBAR_EL1 + 0x400`).
pub(crate) const LOWER_EL_SYNC_SLOT: u64 = 8;

/// Stage 1: 16 KiB granule, `T0SZ = 28` (36-bit VA, walk starts at level 2), write-back cacheable
/// inner-shareable walks, `TTBR1` walks disabled, 36-bit output. `TBI0` is **clear**: a tagged
/// pointer faults, as it does on the translating backend's 64-bit fastmem.
const TCR_EL1: u64 = 28 // T0SZ
    | (1 << 8) // IRGN0: write-back, write-allocate
    | (1 << 10) // ORGN0
    | (3 << 12) // SH0: inner shareable
    | (2 << 14) // TG0: 16 KiB
    | (28 << 16) // T1SZ (unused: EPD1)
    | (1 << 23) // EPD1
    | (1 << 30) // TG1: 16 KiB
    | (1 << 32); // IPS: 36 bits

/// Attribute 0: Normal memory, write-back, read/write-allocate, inner and outer.
const MAIR_EL1: u64 = 0xFF;

/// `SCTLR_EL1`: MMU and caches on, and what EL0 may do without trapping -- `DC ZVA` (`DZE`), read
/// `CTR_EL0` (`UCT`), `WFI`/`WFE` (`nTWI`/`nTWE`), cache maintenance by VA (`UCI`) -- plus stack
/// alignment checks (`SA`, `SA0`), as Linux runs EL0. `0x30D0_0800` is the ARMv8.0 RES1 set.
const SCTLR_EL1: u64 = 0x30D0_0800
    | 1 // M
    | (1 << 2) // C
    | (1 << 3) // SA
    | (1 << 4) // SA0
    | (1 << 12) // I
    | (1 << 14) // DZE
    | (1 << 15) // UCT
    | (1 << 16) // nTWI
    | (1 << 18) // nTWE
    | (1 << 26); // UCI

/// `FPEN = 0b11`: FP/SIMD at EL0 and EL1 without trapping.
const CPACR_EL1: u64 = 3 << 20;

/// `EL0VCTEN | EL0PCTEN`: EL0 may read the counters. (`CNTPCT_EL0` still traps to the host, which
/// emulates it; MEASURED in the probe.)
const CNTKCTL_EL1: u64 = 0b11;

/// Stage-1 level-2 block descriptor fields.
const BLOCK: u64 = 0b01;
const AP_EL0_RW: u64 = 0b01 << 6;
const SH_INNER: u64 = 0b11 << 8;
const AF: u64 = 1 << 10;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

/// 32 MiB, one level-2 block with a 16 KiB granule.
const BLOCK_BYTES: u64 = 1 << 25;

/// The process-wide EL1 page, built and mapped once.
static SYSTEM: OnceLock<Result<(), CpuError>> = OnceLock::new();

fn page() -> usize {
    omni_platform::vm::page_size()
}

fn hv_error(operation: &'static str, error: HvError) -> CpuError {
    match error {
        HvError::Unsupported { reason, .. } => {
            CpuError::Unsupported { backend: BACKEND_NAME, operation, reason }
        }
        other => CpuError::Backend { backend: BACKEND_NAME, operation, detail: other.to_string() },
    }
}

pub(crate) fn hv_err(operation: &'static str) -> impl FnOnce(HvError) -> CpuError {
    move |error| hv_error(operation, error)
}

/// Build the vector page and the translation table, and map both at [`SYSTEM_IPA`].
pub(crate) fn system() -> CpuResult<()> {
    SYSTEM
        .get_or_init(|| {
            let vm = Vm::get().map_err(hv_err("create the virtual machine"))?;
            let page = page();
            let layout = std::alloc::Layout::from_size_align(2 * page, page).map_err(|_| {
                CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "lay out the EL1 page",
                    detail: format!("{page} is not a usable page size"),
                }
            })?;
            // SAFETY: a non-zero size. Leaked on purpose: the mapping lives as long as the VM,
            // which lives as long as the process.
            let host = unsafe { std::alloc::alloc_zeroed(layout) };
            if host.is_null() {
                return Err(CpuError::Backend {
                    backend: BACKEND_NAME,
                    operation: "allocate the EL1 page",
                    detail: "the allocator returned null".into(),
                });
            }
            // Page 0: sixteen vector slots of 0x80 bytes, each `hvc #slot` and then a branch to
            // itself that is never reached (the host resumes by writing PC and CPSR).
            for slot in 0..16u32 {
                let at = (slot as usize) * 0x80;
                // SAFETY: inside the first page of the allocation.
                unsafe {
                    host.add(at).cast::<u32>().write(hvc(slot));
                    host.add(at + 4).cast::<u32>().write(0x1400_0000);
                }
            }
            // Page 1: the level-2 table. Every 32 MiB block of the 64 GiB space identity-mapped for
            // EL0 read-write-execute -- stage 2 decides what is really there -- except the block
            // holding this page, which EL0 cannot touch and EL1 may execute.
            let system_block = SYSTEM_IPA / BLOCK_BYTES;
            for index in 0..2048u64 {
                let output = index * BLOCK_BYTES;
                let descriptor = if index == system_block {
                    output | BLOCK | SH_INNER | AF | UXN
                } else {
                    output | BLOCK | AP_EL0_RW | SH_INNER | AF | PXN
                };
                // SAFETY: inside the second page.
                unsafe { host.add(page).cast::<u64>().add(index as usize).write(descriptor) };
            }
            // SAFETY: the allocation is 2 pages, page-aligned, and never freed.
            unsafe { vm.map_private(host, SYSTEM_IPA, 2 * page, Stage2::READ_EXECUTE) }
                .map_err(hv_err("map the EL1 page"))
        })
        .as_ref()
        .map(|()| ())
        .map_err(again)
}

/// The same refusal again, for a second caller of a failed one-time setup. `CpuError` is not
/// `Clone` (it can carry a `MemError`), and the two variants setup produces are rebuilt exactly.
fn again(error: &CpuError) -> CpuError {
    match error {
        CpuError::Unsupported { backend, operation, reason } => {
            CpuError::Unsupported { backend, operation, reason }
        }
        CpuError::Backend { backend, operation, detail } => {
            CpuError::Backend { backend, operation, detail: detail.clone() }
        }
        other => CpuError::Backend {
            backend: BACKEND_NAME,
            operation: "set up the EL1 page",
            detail: other.to_string(),
        },
    }
}

/// This host thread's vCPU, and whose registers it holds.
pub(crate) struct ThreadVcpu {
    pub(crate) cpu: Vcpu,
    /// The context whose register file is live in `cpu`, by serial; 0 for none.
    pub(crate) loaded: u64,
    /// `CNTVCT_EL0 = counter - offset`.
    pub(crate) vtimer_offset: u64,
    /// Whether the watchdog tick is armed and unmasked on this vCPU.
    pub(crate) watchdog_armed: bool,
}

thread_local! {
    static THREAD_VCPU: RefCell<Option<ThreadVcpu>> = const { RefCell::new(None) };
}

fn configure(cpu: &mut Vcpu) -> Result<(), HvError> {
    let page = page() as u64;
    cpu.set_sys_reg(SysReg::TcrEl1, TCR_EL1)?;
    cpu.set_sys_reg(SysReg::MairEl1, MAIR_EL1)?;
    cpu.set_sys_reg(SysReg::Ttbr0El1, SYSTEM_IPA + page)?;
    cpu.set_sys_reg(SysReg::Ttbr1El1, 0)?;
    cpu.set_sys_reg(SysReg::VbarEl1, SYSTEM_IPA)?;
    cpu.set_sys_reg(SysReg::CpacrEl1, CPACR_EL1)?;
    cpu.set_sys_reg(SysReg::CntkctlEl1, CNTKCTL_EL1)?;
    cpu.set_sys_reg(SysReg::SctlrEl1, SCTLR_EL1)?;
    cpu.set_reg(Reg::Cpsr, 0)?;
    Ok(())
}

/// Run `f` with this thread's vCPU, creating and configuring it on first use.
pub(crate) fn with_thread_vcpu<R>(f: impl FnOnce(&mut ThreadVcpu) -> CpuResult<R>) -> CpuResult<R> {
    THREAD_VCPU.with(|cell| {
        let mut slot = cell.try_borrow_mut().map_err(|_| CpuError::Backend {
            backend: BACKEND_NAME,
            operation: "run guest code",
            detail: "this host thread's vCPU is already running a guest: a context was run from \
                     inside another context's run on the same thread"
                .into(),
        })?;
        if slot.is_none() {
            system()?;
            let mut cpu = Vcpu::create().map_err(|error| match error {
                HvError::VcpuLimit { .. } => CpuError::Unsupported {
                    backend: BACKEND_NAME,
                    operation: "give this host thread a vCPU",
                    reason: "every vCPU this host allows per VM is held by a live host thread that \
                             has run guest code (hv_vm_get_max_vcpu_count: 64 on the M1); one VM per \
                             process, and M:N multiplexing is not built",
                },
                other => hv_error("create a vCPU", other),
            })?;
            configure(&mut cpu).map_err(hv_err("configure the vCPU's EL1"))?;
            let vtimer_offset = cpu.vtimer_offset().map_err(hv_err("read the vtimer offset"))?;
            *slot = Some(ThreadVcpu { cpu, loaded: 0, vtimer_offset, watchdog_armed: false });
        }
        let Some(thread) = slot.as_mut() else {
            unreachable!("filled just above")
        };
        f(thread)
    })
}

/// Counter ticks in `duration`, at the frequency the vtimer compares at.
pub(crate) fn ticks(duration: std::time::Duration) -> u64 {
    let frequency = hv::counter_frequency();
    u64::try_from(duration.as_nanos().saturating_mul(u128::from(frequency)) / 1_000_000_000)
        .unwrap_or(u64::MAX)
        .max(1)
}
