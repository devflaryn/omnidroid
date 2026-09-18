//! The seam, exercised through a trait object.
//!
//! There is no backend in this crate and this file does not pretend there is one. What it does is
//! implement [`GuestCpu`] the way the **ARM64-native** backend will have to — a register file, a
//! thread pointer, thunk and breakpoint sets, a halt flag, and *no translator anywhere* — and then
//! drive it through `dyn GuestCpu`. That is the check the trait actually needs: a shape that
//! quietly assumed translation would be uncomfortable or impossible to implement here, and the
//! discomfort is the signal.
//!
//! Its [`run`](GuestCpu::run) executes nothing and says so by returning
//! [`CpuError::Unsupported`], because Global Constraint 1 forbids placeholder success. Every other
//! method does its real job, and every assertion below is on a value.

use omni_cpu::{
    Capabilities, ContextCost, CpuError, CpuResult, ExitReason, GuestAddr, GuestAddressSpace,
    GuestCpu, GuestCpuBackend, GuestRange, GuestThreadConfig, HaltHandle, Nzcv, RunLimit, VReg,
    XReg, TLS_SLOT_STACK_GUARD_OFFSET,
};

/// A guest address space large enough to hold a TLS block and some code, at a high address.
///
/// 0x7F00_0000_0000 is the host VA D4's identity-mapping measurement ran at (bit 46), so the
/// address-space arithmetic here is exercised at the magnitude it will really see rather than at
/// 0x1000.
const BASE: GuestAddr = 0x7F00_0000_0000;
const LEN: usize = 64 * 1024 * 1024;
const TLS: GuestAddr = BASE + 0x1000;

/// A [`GuestCpu`] that keeps architectural state and translates nothing.
///
/// Deliberately shaped like the ARM64-native backend: the registers are *the* registers, there is no
/// code cache, and [`invalidate_code`](GuestCpu::invalidate_code) has nothing to throw away — on a
/// real ARM64 host it would issue `IC IVAU`/`DSB ISH`/`ISB` and here it records the range so a test
/// can see the call arrived.
struct RegisterFile {
    space: GuestAddressSpace,
    x: [u64; XReg::COUNT],
    v: [u128; VReg::COUNT],
    sp: GuestAddr,
    pc: GuestAddr,
    nzcv: Nzcv,
    tpidr_el0: GuestAddr,
    halt: HaltHandle,
    thunks: Vec<GuestAddr>,
    breakpoints: Vec<GuestAddr>,
    invalidated: Vec<GuestRange>,
}

impl RegisterFile {
    const NAME: &'static str = "register-file";

    fn new(config: GuestThreadConfig) -> Self {
        Self {
            space: config.space(),
            x: [0; XReg::COUNT],
            v: [0; VReg::COUNT],
            sp: config.space().end(),
            pc: 0,
            nzcv: Nzcv::default(),
            // D13: the thread pointer is programmed when the thread is created, not later.
            tpidr_el0: config.tpidr_el0(),
            halt: HaltHandle::new(),
            thunks: Vec::new(),
            breakpoints: Vec::new(),
            invalidated: Vec::new(),
        }
    }
}

impl GuestCpu for RegisterFile {
    fn backend_name(&self) -> &'static str {
        Self::NAME
    }

    fn capabilities(&self) -> Capabilities {
        // Exactly what this implementation can do. `counted_step_limit: false` is the honest answer
        // for a backend with no translator to count in, and it is what makes `run` refuse a counted
        // budget below instead of ignoring it.
        Capabilities { counted_step_limit: false, asynchronous_halt: true, breakpoints: true }
    }

    fn space(&self) -> GuestAddressSpace {
        self.space
    }

    fn run(&mut self, from: GuestAddr, limit: RunLimit) -> CpuResult<ExitReason> {
        self.pc = from;
        if limit.instructions().is_some() {
            return Err(CpuError::Unsupported {
                backend: Self::NAME,
                operation: "honour a counted step limit",
                reason: "it has no translator in which to put an instruction counter",
            });
        }
        if self.halt.clear() {
            return Ok(ExitReason::Halted { pc: self.pc });
        }
        Err(CpuError::Unsupported {
            backend: Self::NAME,
            operation: "execute guest code",
            reason: "it holds architectural state only; there is no backend in omni-cpu yet",
        })
    }

    fn halt_handle(&self) -> HaltHandle {
        self.halt.clone()
    }

    fn x(&self, reg: XReg) -> u64 {
        self.x[usize::from(reg.index())]
    }

    fn set_x(&mut self, reg: XReg, value: u64) {
        self.x[usize::from(reg.index())] = value;
    }

    fn sp(&self) -> GuestAddr {
        self.sp
    }

    fn set_sp(&mut self, value: GuestAddr) {
        self.sp = value;
    }

    fn pc(&self) -> GuestAddr {
        self.pc
    }

    fn set_pc(&mut self, value: GuestAddr) {
        self.pc = value;
    }

    fn nzcv(&self) -> Nzcv {
        self.nzcv
    }

    fn set_nzcv(&mut self, value: Nzcv) {
        self.nzcv = value;
    }

    fn v(&self, reg: VReg) -> u128 {
        self.v[usize::from(reg.index())]
    }

    fn set_v(&mut self, reg: VReg, value: u128) {
        self.v[usize::from(reg.index())] = value;
    }

    fn tpidr_el0(&self) -> GuestAddr {
        self.tpidr_el0
    }

    fn set_tpidr_el0(&mut self, value: GuestAddr) {
        self.tpidr_el0 = value;
    }

    fn invalidate_code(&mut self, range: GuestRange) -> CpuResult<()> {
        self.invalidated.push(range);
        Ok(())
    }

    fn add_thunk(&mut self, address: GuestAddr) -> CpuResult<()> {
        if !self.thunks.contains(&address) {
            self.thunks.push(address);
        }
        Ok(())
    }

    fn remove_thunk(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let before = self.thunks.len();
        self.thunks.retain(|&thunk| thunk != address);
        Ok(self.thunks.len() != before)
    }

    fn add_breakpoint(&mut self, address: GuestAddr) -> CpuResult<()> {
        if !self.breakpoints.contains(&address) {
            self.breakpoints.push(address);
        }
        Ok(())
    }

    fn remove_breakpoint(&mut self, address: GuestAddr) -> CpuResult<bool> {
        let before = self.breakpoints.len();
        self.breakpoints.retain(|&point| point != address);
        Ok(self.breakpoints.len() != before)
    }

    fn cost(&self) -> ContextCost {
        // What a translation-free context costs: its own state, and no shared section at all. The
        // second field being zero here is the point — a backend that emits code fills it in, and
        // D15's gap is visible in the difference.
        ContextCost {
            private_committed: core::mem::size_of::<Self>(),
            shared_committed: 0,
        }
    }
}

/// The factory. Holds nothing shared, because a native backend has nothing to share.
struct NativeShaped;

impl GuestCpuBackend for NativeShaped {
    fn name(&self) -> &'static str {
        RegisterFile::NAME
    }

    fn create_thread(&self, config: GuestThreadConfig) -> CpuResult<Box<dyn GuestCpu>> {
        Ok(Box::new(RegisterFile::new(config)))
    }

    fn shared_cost(&self) -> ContextCost {
        ContextCost::default()
    }
}

fn space() -> GuestAddressSpace {
    GuestAddressSpace::new(BASE, LEN).expect("a guest address space")
}

fn cpu() -> Box<dyn GuestCpu> {
    let config = GuestThreadConfig::new(space(), TLS).expect("a guest thread");
    NativeShaped.create_thread(config).expect("a CPU context")
}

/// The whole register file round-trips through the trait object, every register distinguishable
/// from every other.
#[test]
fn every_register_the_brief_names_round_trips_through_the_trait_object() {
    let mut cpu = cpu();

    // X0-X30, each with a value that identifies it.
    for reg in XReg::all() {
        cpu.set_x(reg, 0xAAAA_0000_0000_0000 | u64::from(reg.index()));
    }
    for reg in XReg::all() {
        assert_eq!(
            cpu.x(reg),
            0xAAAA_0000_0000_0000 | u64::from(reg.index()),
            "{reg} does not hold its own value"
        );
    }
    assert_eq!(cpu.x(XReg::LR), 0xAAAA_0000_0000_001E, "X30 is the link register");
    assert_eq!(cpu.x(XReg::X0), 0xAAAA_0000_0000_0000);

    // V0-V31, full 128 bits, so a backend that kept only the low 64 is caught.
    for reg in VReg::all() {
        cpu.set_v(reg, 0x0123_4567_89AB_CDEF_0000_0000_0000_0000u128 | u128::from(reg.index()));
    }
    for reg in VReg::all() {
        let value = cpu.v(reg);
        assert_eq!(value as u64, u64::from(reg.index()), "{reg} lost its low half");
        assert_eq!(
            (value >> 64) as u64,
            0x0123_4567_89AB_CDEF,
            "{reg} lost its high half: a 128-bit register kept in 64 bits"
        );
    }

    // SP and PC are their own registers, not X31.
    cpu.set_sp(BASE + 0x20_0000);
    cpu.set_pc(BASE + 0x30_0000);
    assert_eq!(cpu.sp(), BASE + 0x20_0000);
    assert_eq!(cpu.pc(), BASE + 0x30_0000);
    assert_ne!(cpu.sp() as u64, cpu.x(XReg::LR), "SP must not alias a general-purpose register");

    // NZCV, and only NZCV.
    cpu.set_nzcv(Nzcv { n: true, z: false, c: true, v: false });
    assert_eq!(cpu.nzcv(), Nzcv { n: true, z: false, c: true, v: false });
    assert_eq!(cpu.nzcv().to_pstate(), 0xA000_0000);
    assert_eq!(cpu.nzcv().to_string(), "N-C-");
}

/// D13, through the trait: a context is *born* with a thread pointer, and it is readable.
#[test]
fn a_context_is_born_with_tpidr_el0_already_programmed() {
    let cpu = cpu();
    assert_eq!(
        cpu.tpidr_el0(),
        TLS,
        "TPIDR_EL0 must already hold the configured thread pointer before any guest code runs: \
         1,276 of libroblox.so's 1,282 MRS TPIDR_EL0 instructions immediately load [Xt, #0x28], \
         and the first runs before the first static initializer (D13)"
    );
    assert!(cpu.space().contains(cpu.tpidr_el0() + TLS_SLOT_STACK_GUARD_OFFSET));

    // A guest thread calling __set_tls re-points it, and the new value is what is read back.
    let mut cpu = cpu;
    cpu.set_tpidr_el0(TLS + 0x800);
    assert_eq!(cpu.tpidr_el0(), TLS + 0x800);

    // And a thread cannot be configured without one at all, which is what makes the read above
    // meaningful rather than a default.
    assert!(matches!(
        GuestThreadConfig::new(space(), 0),
        Err(CpuError::MissingThreadPointer { .. })
    ));
    assert!(matches!(
        GuestThreadConfig::new(space(), BASE - 1),
        Err(CpuError::MissingThreadPointer { .. })
    ));
}

/// A backend that cannot count instructions refuses a counted budget instead of running unbounded.
///
/// This is the trait's answer to the shape that nearly assumed translation. If `run` had only ever
/// taken a step count, the ARM64-native backend's only options would have been to lie or to be
/// impossible.
#[test]
fn a_backend_without_an_instruction_counter_refuses_a_counted_budget() {
    let mut cpu = cpu();
    assert!(!cpu.capabilities().counted_step_limit);

    match cpu.run(BASE, RunLimit::Instructions(1000)) {
        Err(CpuError::Unsupported { backend, operation, .. }) => {
            assert_eq!(backend, "register-file");
            assert!(operation.contains("counted step limit"), "{operation}");
        }
        other => panic!("a counted budget must be refused by this backend, got {other:?}"),
    }
    // The refusal still moved PC to where it was asked to run from, so a caller can see where it
    // would have started.
    assert_eq!(cpu.pc(), BASE);

    // And an uncounted run reaches the backend proper, which reports that it executes nothing
    // rather than returning a fabricated exit.
    match cpu.run(BASE, RunLimit::Unlimited) {
        Err(CpuError::Unsupported { operation, .. }) => {
            assert!(operation.contains("execute guest code"), "{operation}");
        }
        other => panic!("this backend executes nothing and must say so, got {other:?}"),
    }
}

/// The halt handle is the stop control both backends have, and it works across a thread boundary.
#[test]
fn a_halt_requested_from_another_thread_stops_the_run() {
    let mut cpu = cpu();
    assert!(cpu.capabilities().asynchronous_halt);

    let handle = cpu.halt_handle();
    std::thread::spawn(move || handle.request()).join().expect("the requesting thread");

    let exit = cpu.run(BASE + 0x40, RunLimit::Unlimited).expect("a halted run is not an error");
    assert_eq!(
        exit,
        ExitReason::Halted { pc: BASE + 0x40 },
        "a halt requested before the run started must stop it, or the bound on untrusted guest \
         code is a race"
    );
    assert!(exit.is_resumable());

    // The halt is consumed, so the next run is not stopped by the previous request.
    assert!(matches!(cpu.run(BASE, RunLimit::Unlimited), Err(CpuError::Unsupported { .. })));
}

/// Thunks, breakpoints and code invalidation are reachable through the trait object and are
/// idempotent where the documentation says they are.
#[test]
fn thunks_breakpoints_and_invalidation_behave_as_documented() {
    let mut cpu = cpu();

    cpu.add_thunk(BASE + 0x100).expect("add a thunk");
    cpu.add_thunk(BASE + 0x100).expect("adding it twice is idempotent");
    assert!(cpu.remove_thunk(BASE + 0x100).expect("remove"), "the thunk was there");
    assert!(!cpu.remove_thunk(BASE + 0x100).expect("remove"), "and is not there twice");

    assert!(cpu.capabilities().breakpoints);
    cpu.add_breakpoint(BASE + 0x200).expect("add a breakpoint");
    cpu.add_breakpoint(BASE + 0x200).expect("idempotent");
    assert!(cpu.remove_breakpoint(BASE + 0x200).expect("remove"));
    assert!(!cpu.remove_breakpoint(BASE + 0x200).expect("remove"));

    cpu.invalidate_code(GuestRange::new(BASE, 0x1000).expect("a range")).expect("invalidate");

    // A range that wraps never reaches a backend at all: it cannot be constructed.
    assert!(matches!(GuestRange::new(BASE, usize::MAX), Err(CpuError::InvalidRange { .. })));
    assert!(matches!(GuestRange::new(BASE, 0), Err(CpuError::InvalidRange { .. })));
}

/// Per-context cost is reported, and the shared half is reported separately so that summing guest
/// threads does not count it once per thread.
#[test]
fn cost_is_reported_per_context_with_the_shared_part_kept_out_of_it() {
    let backend = NativeShaped;
    let config = GuestThreadConfig::new(space(), TLS).expect("a guest thread");

    let threads: Vec<Box<dyn GuestCpu>> =
        (0..4).map(|_| backend.create_thread(config).expect("a context")).collect();
    let summed = threads
        .iter()
        .map(|thread| thread.cost())
        .fold(ContextCost::default(), ContextCost::saturating_add);

    assert_eq!(summed.private_committed, 4 * threads[0].cost().private_committed);
    assert!(summed.private_committed > 0, "a context costs something");
    assert_eq!(
        summed.shared_committed, 0,
        "a backend that translates nothing has no pagefile-backed section, so the half \
         process_commit_charge cannot see is genuinely zero here"
    );
    assert_eq!(summed.total(), summed.private_committed);
    assert_eq!(backend.shared_cost(), ContextCost::default());
    assert_eq!(backend.name(), "register-file");
}

/// The runtime holds the backend without knowing which one it is.
#[test]
fn the_backend_is_usable_entirely_through_trait_objects() {
    let backend: Box<dyn GuestCpuBackend> = Box::new(NativeShaped);
    let config = GuestThreadConfig::new(space(), TLS).expect("a guest thread");
    let mut cpu: Box<dyn GuestCpu> = backend.create_thread(config).expect("a context");

    cpu.set_x(XReg::X0, 0x1234);
    assert_eq!(cpu.x(XReg::X0), 0x1234);
    assert_eq!(cpu.backend_name(), backend.name());
    assert_eq!(cpu.space().base(), BASE);
    assert_eq!(cpu.space().address_bits(), 47, "D4 measured identity mapping at bit 46");

    // A context is `Send`: it is created wherever the guest thread is brought up and moved to the
    // thread that runs it.
    let moved = std::thread::spawn(move || cpu.x(XReg::X0)).join().expect("the guest thread");
    assert_eq!(moved, 0x1234);
}
