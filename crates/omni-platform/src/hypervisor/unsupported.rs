//! Every host but macOS on Apple silicon: no hypervisor this seam can drive, so every entry point
//! refuses by name. A native CPU backend built here reports that at its first call rather than
//! appearing to work.

use super::{HostLimits, HvError, HvResult, Reg, Stage2, Stage2Stats, SysReg, VcpuExit};

const REASON: &str = "the hypervisor seam is implemented only for macOS on Apple silicon \
                      (Hypervisor.framework); a same-ISA guest needs an arm64 host and a VM API";

fn unsupported<T>(operation: &'static str) -> HvResult<T> {
    Err(HvError::Unsupported { operation, reason: REASON })
}

pub(super) fn vm_init() -> HvResult<()> {
    unsupported("create a virtual machine")
}

pub(super) fn limits() -> HostLimits {
    HostLimits { ipa_bits: 0, max_vcpus: 0 }
}

pub(super) fn attach(_base: usize, _len: usize) -> HvResult<()> {
    unsupported("attach a host range to stage 2")
}

pub(super) fn detach(_base: usize, _len: usize) {}

pub(super) unsafe fn map_private(_host: *mut u8, _ipa: u64, _len: usize, _p: Stage2) -> HvResult<()> {
    unsupported("map private memory at stage 2")
}

pub(super) unsafe fn overlay(_ipa_page: u64, _host: *mut u8, _p: Stage2) -> HvResult<()> {
    unsupported("overlay a stage-2 page")
}

pub(super) fn remove_overlay(_ipa_page: u64) -> HvResult<bool> {
    unsupported("remove a stage-2 overlay")
}

pub(super) fn stage2_stats() -> Stage2Stats {
    Stage2Stats::default()
}

pub(super) fn live_vcpus() -> u32 {
    0
}

/// Uninhabited: no vCPU can exist here, so the compiler discharges every method.
pub(super) enum VcpuInner {}

pub(super) fn vcpu_create() -> HvResult<VcpuInner> {
    unsupported("create a vCPU")
}

impl VcpuInner {
    pub(super) fn run(&mut self) -> HvResult<VcpuExit> {
        match *self {}
    }
    pub(super) fn reg(&self, _reg: Reg) -> HvResult<u64> {
        match *self {}
    }
    pub(super) fn set_reg(&mut self, _reg: Reg, _value: u64) -> HvResult<()> {
        match *self {}
    }
    pub(super) fn sys_reg(&self, _reg: SysReg) -> HvResult<u64> {
        match *self {}
    }
    pub(super) fn set_sys_reg(&mut self, _reg: SysReg, _value: u64) -> HvResult<()> {
        match *self {}
    }
    pub(super) fn simd(&self, _index: u8) -> HvResult<u128> {
        match *self {}
    }
    pub(super) fn set_simd(&mut self, _index: u8, _value: u128) -> HvResult<()> {
        match *self {}
    }
    pub(super) fn set_vtimer_mask(&mut self, _masked: bool) -> HvResult<()> {
        match *self {}
    }
    pub(super) fn vtimer_offset(&self) -> HvResult<u64> {
        match *self {}
    }
}

/// The monotonic clock in nanoseconds: there is no virtual counter to be consistent with here, and
/// a caller converting a duration to ticks gets the right answer with [`counter_frequency`].
pub(super) fn counter_now() -> u64 {
    u64::try_from(crate::clock::monotonic_now().as_nanos()).unwrap_or(u64::MAX)
}

pub(super) fn counter_frequency() -> u64 {
    1_000_000_000
}

/// Nothing to do, and vacuously so rather than as a stub: no [`VcpuInner`] can exist on this host, so
/// no guest instruction fetch can have seen the range.
pub(super) unsafe fn icache_invalidate(_address: *mut u8, _len: usize) {}
