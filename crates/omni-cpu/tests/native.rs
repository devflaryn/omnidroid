//! The native backend (`native-hvf`): guest ARM64 at EL0 in a Hypervisor.framework VM, run for real.
//!
//! ```text
//! CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh \
//!     cargo test -p omni-cpu --release --features native-hvf --test native -- --test-threads=1
//! ```
//!
//! The runner signs the binary with `com.apple.security.hypervisor`. Unsigned, every test here fails
//! at `NativeBackend::new` with the refusal naming the entitlement -- a failure, never a skip.
//!
//! Three parts: the seam's contracts on small guest programs (what each exit means, what state
//! survives, what is refused); **the M2 gate on the real `libroblox.so`**, the same three functions
//! and predictions as `tests/roblox.rs`; and, `#[ignore]`d, the measurements
//! `docs/ports/macos-hvf.md` reports.

#![cfg(all(feature = "native-hvf", feature = "dynarmic", target_arch = "aarch64"))]

mod harness;

use std::sync::Arc;
use std::time::{Duration, Instant};

use harness::a64::*;
use harness::roblox::serialized;
use harness::x;
use omni_cpu::native::{NativeBackend, NativeCpu, NativeOptions};
use omni_cpu::{AccessKind, CpuError, ExitReason, GuestAddr, GuestCpu, RunLimit, VReg};
use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};

// ------------------------------------------------------------------------------------------ fixture

const CODE_BYTES: usize = 64 * 1024;
const DATA_BYTES: usize = 64 * 1024;
const LAZY_BYTES: usize = 1024 * 1024;
/// Where the boundary would put its thunk area: `Protection::Read`, lazily committed, never
/// executable -- `omni-android/src/region.rs`'s design.
const THUNK_BYTES: usize = 16 * 1024;

/// `BLR Xn`.
const fn blr(rn: u32) -> u32 {
    0xD63F_0000 | (rn << 5)
}
/// `CAS Ws, Wt, [Xn]` (32-bit, no ordering), `0x88A0_7C00`.
const fn cas_w(rs: u32, rt: u32, rn: u32) -> u32 {
    0x88A0_7C00 | (rs << 16) | (rn << 5) | rt
}
/// `WFI`.
const WFI: u32 = 0xD503_207F;
/// `MRS X0, ID_AA64ISAR0_EL1`: EL1-only, so it traps at EL0.
const MRS_ID_AA64ISAR0: u32 = 0xD538_0600;

struct Native {
    space: Arc<GuestSpace>,
    backend: NativeBackend,
    code: GuestAddr,
    data: GuestAddr,
    readonly: GuestAddr,
    lazy: GuestAddr,
    thunks: GuestAddr,
    unmapped: GuestAddr,
}

impl Native {
    fn new() -> Self {
        Self::with_options(NativeOptions::default())
    }

    fn with_options(options: NativeOptions) -> Self {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let page = space.page_size();
        let map = |len, protection, commit| {
            space
                .map_anonymous(Placement::Anywhere { align: page }, len, protection, commit)
                .expect("a guest mapping")
        };
        let code = map(CODE_BYTES, Protection::ReadWrite, CommitPolicy::Eager);
        // Code is executable and not writable whenever a guest runs; `load_at` flips it for a write.
        space.protect(code, CODE_BYTES, Protection::ReadExecute).expect("code executable");
        let data = map(DATA_BYTES, Protection::ReadWrite, CommitPolicy::Eager);
        let readonly = map(page, Protection::ReadWrite, CommitPolicy::Eager);
        space.protect(readonly, page, Protection::Read).expect("drop to read-only");
        let lazy = map(LAZY_BYTES, Protection::ReadWrite, CommitPolicy::Lazy);
        let thunks = map(THUNK_BYTES, Protection::Read, CommitPolicy::Lazy);
        let unmapped = space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= page)
            .map(|r| (r.start + r.len / 2) & !0xF)
            .expect("some free address space");
        let backend = NativeBackend::new(Arc::clone(&space), options).unwrap_or_else(|error| {
            panic!(
                "the native backend could not be brought up: {error}. Is the test binary signed? \
                 Run with CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh"
            )
        });
        Self { space, backend, code, data, readonly, lazy, thunks, unmapped }
    }

    fn load_at(&self, offset: usize, program: &[u32]) -> GuestAddr {
        let entry = self.code + offset;
        self.space.protect(self.code, CODE_BYTES, Protection::ReadWrite).expect("writable");
        let ptr = self.space.ptr(entry, program.len() * 4).expect("a host pointer");
        // SAFETY: a committed, writable range of the code region, and no guest is running.
        unsafe { core::ptr::copy_nonoverlapping(program.as_ptr(), ptr.cast::<u32>(), program.len()) };
        self.space.protect(self.code, CODE_BYTES, Protection::ReadExecute).expect("executable");
        entry
    }

    fn load(&self, program: &[u32]) -> GuestAddr {
        self.load_at(0, program)
    }

    fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: a committed range (or one the host demand pager commits on this read).
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }

    fn write_u64(&self, address: GuestAddr, value: u64) {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `read_u64`.
        unsafe { ptr.cast::<u64>().write_unaligned(value) }
    }

    /// A context with a TLS block and the return sentinel armed at the top of the code region --
    /// an executable page whose word there is zero (`UDF #0`), so it traps with nothing planted.
    fn thread(&self) -> (NativeCpu, GuestAddr) {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        let sentinel = self.code + CODE_BYTES - 4;
        cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
        cpu.set_x(x(30), sentinel as u64);
        cpu.set_sp(self.data + DATA_BYTES - 64);
        (cpu, sentinel)
    }
}

fn run(cpu: &mut NativeCpu, from: GuestAddr) -> ExitReason {
    cpu.run(from, RunLimit::Unlimited).expect("the backend runs")
}

// ------------------------------------------------------------------------------ the seam's contract

#[test]
fn the_capabilities_are_what_a_native_backend_can_do_and_the_rest_is_refused_by_name() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    let caps = cpu.capabilities();
    assert!(!caps.counted_step_limit && caps.asynchronous_halt && !caps.breakpoints && !caps.inline_thunks);
    assert_eq!(cpu.backend_name(), "native-hvf");
    let entry = guest.load(&[movz(0, 1, 0), ret(30)]);
    for (what, result) in [
        ("a counted run", cpu.run(entry, RunLimit::Instructions(1_000)).map(|_| ())),
        ("an inline thunk", cpu.add_inline_thunk(guest.thunks, |_| {}, Default::default())),
        ("a breakpoint", cpu.add_breakpoint(entry)),
    ] {
        assert!(
            matches!(result, Err(CpuError::Unsupported { backend: "native-hvf", .. })),
            "{what} must be refused by name, not ignored: {result:?}"
        );
    }
    assert_eq!(cpu.last_run_instructions(), 0, "nothing is counted, and nothing is claimed");
}

#[test]
fn a_guest_loop_computes_and_returns_through_the_sentinel_with_its_registers() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    // x0 = sum(1..=x1); leaves x1 = 0 and the flags from the last SUBS (Z set).
    let entry = guest.load(&[
        movz(0, 0, 0),
        add_reg(0, 0, 1),
        subs_imm(1, 1, 1),
        b_cond(1, -2),
        ret(30),
    ]);
    cpu.set_x(x(1), 100_000);
    cpu.set_x(x(7), 0x7777);
    let exit = run(&mut cpu, entry);
    assert_eq!(exit, ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(0)), 100_000 * 100_001 / 2);
    assert_eq!(cpu.x(x(1)), 0);
    assert_eq!(cpu.x(x(7)), 0x7777, "a register the guest did not touch survives the round trip");
    assert!(cpu.nzcv().z, "the guest's last SUBS left Z set, and the flags come back with the state");
    assert_eq!(cpu.pc(), sentinel);
}

#[test]
fn the_thread_pointer_is_programmed_and_a_guest_write_to_it_comes_back() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    let guard = cpu.tls().expect("a TLS block").stack_guard();
    // x1 = [TPIDR_EL0 + 0x28]; then TPIDR_EL0 = x2.
    let entry = guest.load(&[mrs_tpidr_el0(0), ldr_imm(1, 0, 0x28), msr_tpidr_el0(2), ret(30)]);
    cpu.set_x(x(2), 0x0000_0003_1234_5000);
    let tp = cpu.tpidr_el0();
    let _ = run(&mut cpu, entry);
    assert_eq!(cpu.x(x(0)) as GuestAddr, tp, "D13: the thread pointer is the TLS block's");
    assert_eq!(cpu.x(x(1)), guard, "and TLS_SLOT_STACK_GUARD holds the arena's guard");
    assert_eq!(cpu.tpidr_el0(), 0x0000_0003_1234_5000, "the guest's own MSR is saved back");
}

#[test]
fn the_full_vector_file_crosses_both_ways() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    // Q2 = Q0 (all 128 bits), then Q0 = Q1: `str q0/ldr` through the data region.
    let buf = guest.data;
    let entry = guest.load(&[
        str_q(0, 3, 0),
        ldr_q(2, 3, 0),
        str_q(1, 3, 16),
        ldr_q(0, 3, 16),
        ret(30),
    ]);
    let a = u128::from_le_bytes(core::array::from_fn(|i| (i as u8) ^ 0x5A));
    let b = u128::from_le_bytes(core::array::from_fn(|i| (i as u8).wrapping_mul(7) ^ 0xC3));
    cpu.set_v(VReg::new(0).expect("V0"), a);
    cpu.set_v(VReg::new(1).expect("V1"), b);
    cpu.set_x(x(3), buf as u64);
    let _ = run(&mut cpu, entry);
    assert_eq!(cpu.v(VReg::new(2).expect("V2")), a, "all 128 bits, in lane order");
    assert_eq!(cpu.v(VReg::new(0).expect("V0")), b);
}

#[test]
fn a_thunk_stops_at_its_address_and_resumes_where_the_host_sends_it() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    let thunk = guest.thunks + 16;
    cpu.add_thunk(thunk).expect("a thunk in the non-executable thunk area");
    assert_eq!(guest.backend.veneer_pages(), 1);
    // Keep the return address in x19; x0 = 1; call the thunk through x9; x0 += x0 (the host's
    // answer); return through x19.
    let entry = guest.load(&[mov_reg(19, 30), movz(0, 1, 0), blr(9), add_reg(0, 0, 0), br(19)]);
    cpu.set_x(x(9), thunk as u64);
    let exit = run(&mut cpu, entry);
    assert_eq!(exit, ExitReason::Thunk { pc: thunk });
    assert_eq!(cpu.pc(), thunk, "stopped at the thunk, not past it");
    assert_eq!(cpu.x(x(0)), 1, "the guest's argument is in X0 at the crossing");
    let lr = cpu.x(x(30));
    assert_eq!(lr as GuestAddr, entry + 12, "X30 is the call's return address");
    // The host services the call: X0 = 21, resume at X30.
    cpu.set_x(x(0), 21);
    let exit = run(&mut cpu, lr as GuestAddr);
    assert_eq!(exit, ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(0)), 42);

    // A branch into the middle of a slot is not a thunk: a typed fault naming the address.
    cpu.set_x(x(9), (thunk + 4) as u64);
    cpu.set_x(x(30), sentinel as u64);
    let exit = run(&mut cpu, entry);
    assert_eq!(
        exit,
        ExitReason::MemoryFault { pc: thunk + 4, address: thunk + 4, access: AccessKind::Execute }
    );
    // Removed, the address is a fault too.
    assert!(cpu.remove_thunk(thunk).expect("remove"));
    cpu.set_x(x(9), thunk as u64);
    let exit = run(&mut cpu, entry);
    assert!(matches!(exit, ExitReason::MemoryFault { access: AccessKind::Execute, .. }), "{exit:?}");
}

#[test]
fn a_thunk_in_executable_code_or_writable_data_is_refused_rather_than_overlaid() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    let entry = guest.load(&[movz(0, 1, 0), ret(30)]);
    let code = cpu.add_thunk(entry);
    assert!(matches!(code, Err(CpuError::Unsupported { .. })), "real code: {code:?}");
    let data = cpu.add_thunk(guest.data + 64);
    assert!(matches!(data, Err(CpuError::Unsupported { .. })), "writable data: {data:?}");
    assert_eq!(guest.backend.veneer_pages(), 0, "and nothing was overlaid");
}

#[test]
fn every_bad_access_is_a_typed_fault_at_the_faulting_instruction() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    let load = guest.load_at(0, &[ldr_imm(1, 0, 0), ret(30)]);
    let store = guest.load_at(64, &[str_imm(1, 0, 0), ret(30)]);
    let jump = guest.load_at(128, &[br(0)]);
    let host_heap = Box::new(0x5A5A_5A5Au64);
    let host_address = &*host_heap as *const u64 as GuestAddr;
    for (name, entry, address, access) in [
        ("load from free space", load, guest.unmapped, AccessKind::Read),
        ("store to a read-only page", store, guest.readonly, AccessKind::Write),
        ("jump to free space", jump, guest.unmapped, AccessKind::Execute),
        ("jump into the data region", jump, guest.data, AccessKind::Execute),
        // The isolation D4 amendment 1 says the translating backend does not have: a host pointer
        // is not guest memory, and reading it is a fault rather than a silent success.
        ("load from the host's heap", load, host_address, AccessKind::Read),
        ("load at 64 GiB and above", load, 1usize << 36, AccessKind::Read),
    ] {
        cpu.set_x(x(0), address as u64);
        let exit = run(&mut cpu, entry);
        let pc = if access == AccessKind::Execute { address } else { entry };
        assert_eq!(exit, ExitReason::MemoryFault { pc, address, access }, "{name}");
    }
    assert_eq!(*host_heap, 0x5A5A_5A5A);
}

#[test]
fn a_first_touch_of_lazy_memory_is_paged_in_through_the_policy_and_charged() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    let granule = guest.space.commit_granule();
    // Store x1 to [x0], [x0 + granule], [x0 + 2 * granule].
    let entry = guest.load(&[
        str_imm(1, 0, 0),
        add_reg(0, 0, 2),
        str_imm(1, 0, 0),
        add_reg(0, 0, 2),
        str_imm(1, 0, 0),
        ret(30),
    ]);
    let before = guest.space.stats();
    cpu.set_x(x(0), guest.lazy as u64);
    cpu.set_x(x(1), 0xFEED_F00D);
    cpu.set_x(x(2), granule as u64);
    let exit = run(&mut cpu, entry);
    assert_eq!(exit, ExitReason::Returned { pc: sentinel });
    let after = guest.space.stats();
    assert_eq!(cpu.exit_counts().demand_faults, 3, "one stage-2 fault per untouched granule");
    assert_eq!(
        after.committed - before.committed,
        3 * granule,
        "D10: the guest's first touches are charged to the space, a granule each"
    );
    for i in 0..3 {
        assert_eq!(guest.read_u64(guest.lazy + i * granule), 0xFEED_F00D);
    }
}

#[test]
fn a_guest_svc_an_undefined_word_and_a_privileged_register_are_named() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, _) = guest.thread();
    let svc_at = guest.load_at(0, &[svc(0x42)]);
    let udf_at = guest.load_at(64, &[UNALLOCATED]);
    let mrs_at = guest.load_at(128, &[MRS_ID_AA64ISAR0]);
    assert_eq!(
        run(&mut cpu, svc_at),
        ExitReason::UnsupportedInstruction { pc: svc_at, encoding: svc(0x42) }
    );
    assert_eq!(run(&mut cpu, udf_at), ExitReason::UnsupportedInstruction { pc: udf_at, encoding: UNALLOCATED });
    assert_eq!(
        run(&mut cpu, mrs_at),
        ExitReason::UnsupportedInstruction { pc: mrs_at, encoding: MRS_ID_AA64ISAR0 }
    );
}

#[test]
fn wfi_and_the_counters_are_served_and_the_counter_is_one_monotonic_clock() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    // x0 = CNTVCT; WFI; x1 = CNTPCT (trapped and emulated); x2 = CNTFRQ.
    let entry = guest.load(&[mrs_cntvct_el0(0), WFI, mrs_cntpct_el0(1), mrs_cntfrq_el0(2), ret(30)]);
    let exit = run(&mut cpu, entry);
    assert_eq!(exit, ExitReason::Returned { pc: sentinel });
    let (vct, pct, frq) = (cpu.x(x(0)), cpu.x(x(1)), cpu.x(x(2)));
    assert!(pct >= vct, "CNTPCT after CNTVCT on one clock: {vct} then {pct}");
    assert!(pct - vct < frq, "and within a second of it: {} ticks at {frq} Hz", pct - vct);
    assert!(cpu.exit_counts().emulated >= 1, "CNTPCT_EL0 traps and is served in place");
    println!("guest CNTFRQ_EL0 = {frq} Hz");
}

#[test]
fn a_real_lse_atomic_executes_natively() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    // CAS W1, W2, [X0]: compare [x0] with w1; if equal store w2. W1 gets the old value either way.
    let entry = guest.load(&[cas_w(1, 2, 0), ret(30)]);
    guest.write_u64(guest.data, 7);
    cpu.set_x(x(0), guest.data as u64);
    cpu.set_x(x(1), 7);
    cpu.set_x(x(2), 9);
    assert_eq!(run(&mut cpu, entry), ExitReason::Returned { pc: sentinel });
    assert_eq!((cpu.x(x(1)), guest.read_u64(guest.data)), (7, 9), "equal: swapped");
    cpu.set_x(x(1), 7);
    cpu.set_x(x(2), 11);
    cpu.set_x(x(30), sentinel as u64);
    assert_eq!(run(&mut cpu, entry), ExitReason::Returned { pc: sentinel });
    assert_eq!((cpu.x(x(1)), guest.read_u64(guest.data)), (9, 9), "unequal: old value, no store");
}

#[test]
fn a_runaway_guest_is_halted_from_another_thread_within_a_tick() {
    let _serial = serialized();
    let guest = Native::with_options(NativeOptions { watchdog_tick: Duration::from_millis(2), ..NativeOptions::default() });
    let (mut cpu, _) = guest.thread();
    let spin = guest.load(&[b(0)]);
    let halt = cpu.halt_handle();
    let requested = Arc::new(std::sync::Mutex::new(None));
    let stamp = Arc::clone(&requested);
    let halter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        *stamp.lock().expect("stamp") = Some(Instant::now());
        halt.request();
    });
    let exit = run(&mut cpu, spin);
    let stopped = Instant::now();
    halter.join().expect("the halting thread");
    assert_eq!(exit, ExitReason::Halted { pc: spin });
    let latency = stopped - requested.lock().expect("stamp").expect("requested");
    assert!(latency < Duration::from_millis(50), "halt latency {latency:?} for a 2 ms tick");
    assert!(cpu.exit_counts().vtimer >= 10, "the watchdog ticked: {:?}", cpu.exit_counts());
    // Sticky until cleared: the next run stops at once.
    assert_eq!(run(&mut cpu, spin), ExitReason::Halted { pc: spin });
    assert!(cpu.halt_handle().clear());
}

#[test]
fn a_context_is_run_from_several_threads_and_two_contexts_share_one_thread() {
    let _serial = serialized();
    let guest = Arc::new(Native::new());
    // x0 += x1; return.
    let entry = guest.load(&[add_reg(0, 0, 1), ret(30)]);
    let (mut a, sentinel) = guest.thread();
    let (mut b, _) = guest.thread();
    a.set_x(x(0), 0);
    a.set_x(x(1), 1);
    b.set_x(x(0), 1000);
    b.set_x(x(1), 10);
    // Interleave the two on this thread: each must see its own registers, not the vCPU's leftovers.
    for round in 1..=3u64 {
        a.set_x(x(30), sentinel as u64);
        b.set_x(x(30), sentinel as u64);
        assert_eq!(run(&mut a, entry), ExitReason::Returned { pc: sentinel });
        assert_eq!(run(&mut b, entry), ExitReason::Returned { pc: sentinel });
        assert_eq!((a.x(x(0)), b.x(x(0))), (round, 1000 + 10 * round));
    }
    // Moved to another thread, which has its own vCPU, and back.
    let a = std::thread::spawn(move || {
        a.set_x(x(30), sentinel as u64);
        assert_eq!(run(&mut a, entry), ExitReason::Returned { pc: sentinel });
        a
    })
    .join()
    .expect("the other thread");
    assert_eq!(a.x(x(0)), 4, "the register file travelled with the context");
    let mut a = a;
    a.set_x(x(30), sentinel as u64);
    assert_eq!(run(&mut a, entry), ExitReason::Returned { pc: sentinel });
    assert_eq!(a.x(x(0)), 5);
}

#[test]
fn rewritten_code_runs_after_invalidate_code() {
    let _serial = serialized();
    let guest = Native::new();
    let (mut cpu, sentinel) = guest.thread();
    let entry = guest.load(&[movz(0, 1, 0), ret(30)]);
    assert_eq!(run(&mut cpu, entry), ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(0)), 1);
    // The host rewrites the code (the region goes RW then RX: two protection changes the stage-2
    // mirror follows) and says so.
    guest.load(&[movz(0, 2, 0), ret(30)]);
    cpu.invalidate_code(omni_cpu::GuestRange::new(entry, 8).expect("a range")).expect("invalidate");
    cpu.set_x(x(30), sentinel as u64);
    assert_eq!(run(&mut cpu, entry), ExitReason::Returned { pc: sentinel });
    assert_eq!(cpu.x(x(0)), 2, "the new instruction ran, not a stale one");
}

#[test]
fn a_thread_that_exits_gives_its_vcpu_back() {
    let _serial = serialized();
    let guest = Arc::new(Native::new());
    let vm = omni_platform::hypervisor::Vm::get().expect("the VM");
    let entry = guest.load(&[ret(30)]);
    let before = vm.live_vcpus();
    let g = Arc::clone(&guest);
    let during = std::thread::spawn(move || {
        let (mut cpu, sentinel) = g.thread();
        assert_eq!(run(&mut cpu, entry), ExitReason::Returned { pc: sentinel });
        omni_platform::hypervisor::Vm::get().expect("the VM").live_vcpus()
    })
    .join()
    .expect("the guest thread");
    assert_eq!(during, before + 1, "the thread made one vCPU");
    assert_eq!(vm.live_vcpus(), before, "and its exit destroyed it");
}
