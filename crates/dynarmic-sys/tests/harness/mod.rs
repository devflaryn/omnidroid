//! A minimal guest for the tests: some code, some memory, and the callback
//! discipline the crate documentation requires.
//!
//! This is also the worked example of that discipline, so it is written the way
//! `omni-cpu`'s adapter will have to be:
//!
//! * [`Ctx`] lives in its own heap allocation behind an `UnsafeCell`, reached
//!   only through a raw pointer. It is **not** the same object as [`Vm`], which
//!   holds the jit handle, so no `&mut Ctx` can be live at a `run` call site.
//! * [`Vm::run`] takes `&self`.
//! * Every callback forms its `&mut Ctx` for the body of that callback and
//!   never stores it, and every callback runs its body inside `catch_unwind`.
//! * A depth counter in `Ctx` records the deepest nesting ever observed, so the
//!   "callbacks are never nested" claim is measured rather than asserted.
//!
//! Guest layout: code at [`CODE_BASE`], served through `read_code`; data in the
//! low 1 MiB, reachable through fastmem at `mem`'s address.

#![allow(dead_code)]

pub mod a64;

use dynarmic_sys::*;
use std::cell::UnsafeCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Guest virtual address the test code is placed at. Deliberately not 0, and
/// deliberately outside the data arena.
pub const CODE_BASE: u64 = 0x1000_0000;

/// `fastmem_address_space_bits` used by the tests: 1 MiB of guest data.
pub const MEM_BITS: u32 = 20;
/// Size of the guest data arena.
pub const MEM_SIZE: usize = 1 << MEM_BITS;
/// Slack past the end of the arena. dynarmic masks the *address* to
/// `MEM_BITS` when `silently_mirror_fastmem` is on, not the access, so a
/// 16-byte load at guest `0xF_FFF8` still reads eight bytes past `MEM_SIZE`.
pub const MEM_GUARD: usize = 64;

/// Halt bit the test guest uses for "finished": raised by `SVC #0`.
pub const HALT_DONE: u32 = OD_HALT_USER1;
/// Halt bit raised when a callback panicked. See [`Ctx::panic_msg`].
pub const HALT_PANIC: u32 = OD_HALT_USER8;

/// Host-side state reachable from generated guest code.
pub struct Ctx {
    /// Guest address of `code[0]`.
    pub code_base: u64,
    /// The guest program, one instruction word per entry.
    pub code: Vec<u32>,
    /// Guest data arena. `u64` rather than `u8` so the base is 8-aligned.
    pub mem: Vec<u64>,
    /// Where the guest data arena really is: `mem`'s buffer, or a buffer shared with other `Vm`s
    /// ([`VmOptions::shared_arena`]), `MEM_SIZE + MEM_GUARD` bytes either way.
    pub arena: *mut u64,
    /// The jit this context belongs to, filled in after `od_jit_new`. Callbacks
    /// need it to halt execution, which is the only way out of a panic.
    pub jit: *mut c_void,

    /// `SVC` immediates seen, in order.
    pub svc: Vec<u32>,
    /// `(pc, kind)` for every exception raised.
    pub exceptions: Vec<(u64, u32)>,
    /// `(op, vaddr)` for every instruction-cache maintenance operation.
    pub icache: Vec<(u32, u64)>,
    /// `(pc, num_insns)` for every `interpreter_fallback`, in order.
    pub fallbacks: Vec<(u64, u64)>,

    /// Current callback nesting depth.
    pub depth: i32,
    /// Deepest nesting ever reached. Must stay at 1.
    pub max_depth: i32,

    /// Remaining cycle budget, when cycle counting is on.
    pub ticks_remaining: u64,
    /// Total ticks charged.
    pub ticks_used: u64,

    /// Set when a callback panicked; the panic is re-raised after `run`.
    pub panic_msg: Option<String>,
    /// Test hook: make the next `read8` panic, to prove containment works.
    pub panic_on_read8: bool,
    /// Test hook: make `call_svc` re-enter `od_jit_run`, which must be refused.
    pub reenter_on_svc: bool,
    /// What the refused `od_jit_run` re-entry returned.
    pub reenter_result: Option<u32>,
    /// What the refused `od_jit_step` re-entry returned. `Jit::Step` carries
    /// the same `ASSERT(!is_executing)` as `Jit::Run`, so it needs the same
    /// guard and the same evidence that the guard is there.
    pub reenter_step_result: Option<u32>,
    /// Test hook: halt with [`HALT_DONE`] from inside `call_svc`.
    pub halt_on_svc: bool,
    /// Test hook: make `interpreter_fallback` behave as an interpreter that
    /// executed its `num_insns` instructions as no-ops -- advance the guest PC
    /// past them and do **not** halt -- so a test can see where execution goes
    /// after the fallback returns.
    pub fallback_skips: bool,
    /// The `FPCR` the host thread held inside the last `interpreter_fallback`,
    /// read on arm64 hosts only (0 elsewhere).
    pub fallback_host_fpcr: u32,
    /// Test hook: ask for a zero-length code invalidation from inside
    /// `call_svc`, which is what a guest `IC IVAU` over an empty range does.
    pub zero_invalidate_on_svc: bool,
}

impl Ctx {
    fn code_word(&self, vaddr: u64) -> Option<u32> {
        if vaddr < self.code_base || (vaddr - self.code_base) % 4 != 0 {
            return None;
        }
        let idx = ((vaddr - self.code_base) / 4) as usize;
        self.code.get(idx).copied()
    }

    /// Guest data arena as bytes, for tests that want to seed or check memory.
    pub fn bytes(&mut self) -> &mut [u8] {
        let len = MEM_SIZE + MEM_GUARD;
        // SAFETY: `arena` is `mem`'s live buffer or a shared one of the same size that outlives
        // every `Vm` using it; `u8` has weaker alignment than `u64`, so reinterpreting the buffer
        // as bytes is in bounds for `len` bytes. A shared arena is only ever used by tests whose
        // concurrent accesses to it all happen under the exclusive monitor's lock.
        unsafe { std::slice::from_raw_parts_mut(self.arena.cast::<u8>(), len) }
    }

    /// Read a little-endian `u64` from guest address `addr`.
    pub fn read_u64(&mut self, addr: u64) -> u64 {
        let a = (addr as usize) & (MEM_SIZE - 1);
        let b = self.bytes();
        u64::from_le_bytes(b[a..a + 8].try_into().unwrap())
    }

    /// Write a little-endian `u64` to guest address `addr`.
    pub fn write_u64(&mut self, addr: u64, v: u64) {
        let a = (addr as usize) & (MEM_SIZE - 1);
        let b = self.bytes();
        b[a..a + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Host address the guest arena is mapped at.
    fn fastmem_base(&self) -> u64 {
        self.arena as u64
    }
}

/// Runs `f` with a `&mut Ctx` built from the opaque callback context.
///
/// This is the whole re-entrancy and panic discipline in one place.
///
/// # Safety
/// `ctx` must be the pointer given to `od_jit_new` as `OdConfig::ctx`, which in
/// this harness is always `Box<UnsafeCell<Ctx>>::get()` for a box that outlives
/// the jit.
unsafe fn with<R>(ctx: *mut c_void, fallback: R, f: impl FnOnce(&mut Ctx) -> R) -> R {
    // SAFETY: `Vm::new` passes `UnsafeCell::get()` of a box it owns and keeps
    // alive past `od_jit_free`, and dynarmic passes `ctx` through verbatim.
    // Going via `UnsafeCell` is what makes forming `&mut` legal while `Vm` is
    // only shared-borrowed.
    let raw: *mut Ctx = unsafe { (*(ctx as *const UnsafeCell<Ctx>)).get() };

    // SAFETY: no other reference to `*raw` can be live. dynarmic never nests
    // callbacks and never runs them off-thread, `Vm::run` takes `&self` so no
    // `&mut Ctx` exists at the call site, and `Vm::with_ctx` cannot overlap
    // because it would have to run on this thread, which is inside `run`.
    // `max_depth` measures that claim instead of trusting it.
    unsafe {
        (*raw).depth += 1;
        if (*raw).depth > (*raw).max_depth {
            (*raw).max_depth = (*raw).depth;
        }
    }

    // SAFETY (the `&mut *raw`): same invariant, narrowed to this call.
    let result = catch_unwind(AssertUnwindSafe(|| f(unsafe { &mut *raw })));

    // SAFETY: as above; the closure's borrow has ended.
    unsafe {
        (*raw).depth -= 1;
    }

    match result {
        Ok(v) => v,
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".to_string());
            // A panic must not unwind through JIT frames. Record it, ask the
            // jit to stop, and hand the caller something harmless; `Vm::run`
            // re-raises once the generated code has been left behind.
            // SAFETY: as above. `od_jit_halt` is documented by dynarmic as
            // callable from inside a callback -- it only sets an atomic flag.
            unsafe {
                (*raw).panic_msg = Some(msg);
                let jit = (*raw).jit;
                if !jit.is_null() {
                    od_jit_halt(jit, HALT_PANIC);
                }
            }
            fallback
        }
    }
}

unsafe extern "C" fn cb_read_code(ctx: *mut c_void, vaddr: u64, out: *mut u32) -> i32 {
    // SAFETY: `ctx` is the harness context; `out` is dynarmic's stack slot.
    unsafe {
        with(ctx, 0, |c| match c.code_word(vaddr) {
            Some(w) => {
                *out = w;
                1
            }
            None => 0,
        })
    }
}

macro_rules! read_cb {
    ($name:ident, $ty:ty, $n:expr) => {
        unsafe extern "C" fn $name(ctx: *mut c_void, vaddr: u64) -> $ty {
            // SAFETY: `ctx` is the harness context.
            unsafe {
                with(ctx, 0, |c| {
                    if c.panic_on_read8 {
                        c.panic_on_read8 = false;
                        panic!("callback panic, on purpose");
                    }
                    let a = (vaddr as usize) & (MEM_SIZE - 1);
                    let b = c.bytes();
                    <$ty>::from_le_bytes(b[a..a + $n].try_into().unwrap())
                })
            }
        }
    };
}

read_cb!(cb_read8, u8, 1);
read_cb!(cb_read16, u16, 2);
read_cb!(cb_read32, u32, 4);
read_cb!(cb_read64, u64, 8);

unsafe extern "C" fn cb_read128(ctx: *mut c_void, vaddr: u64, out: *mut u64) {
    // SAFETY: `ctx` is the harness context; `out` is two writable `u64`.
    unsafe {
        with(ctx, (), |c| {
            let a = (vaddr as usize) & (MEM_SIZE - 1);
            let b = c.bytes();
            *out = u64::from_le_bytes(b[a..a + 8].try_into().unwrap());
            *out.add(1) = u64::from_le_bytes(b[a + 8..a + 16].try_into().unwrap());
        })
    }
}

macro_rules! write_cb {
    ($name:ident, $ty:ty, $n:expr) => {
        unsafe extern "C" fn $name(ctx: *mut c_void, vaddr: u64, value: $ty) {
            // SAFETY: `ctx` is the harness context.
            unsafe {
                with(ctx, (), |c| {
                    let a = (vaddr as usize) & (MEM_SIZE - 1);
                    let b = c.bytes();
                    b[a..a + $n].copy_from_slice(&value.to_le_bytes());
                })
            }
        }
    };
}

write_cb!(cb_write8, u8, 1);
write_cb!(cb_write16, u16, 2);
write_cb!(cb_write32, u32, 4);
write_cb!(cb_write64, u64, 8);

unsafe extern "C" fn cb_write128(ctx: *mut c_void, vaddr: u64, value: *const u64) {
    // SAFETY: `ctx` is the harness context; `value` is two readable `u64`.
    unsafe {
        with(ctx, (), |c| {
            let lo = *value;
            let hi = *value.add(1);
            let a = (vaddr as usize) & (MEM_SIZE - 1);
            let b = c.bytes();
            b[a..a + 8].copy_from_slice(&lo.to_le_bytes());
            b[a + 8..a + 16].copy_from_slice(&hi.to_le_bytes());
        })
    }
}

macro_rules! exclusive_cb {
    ($name:ident, $ty:ty, $n:expr) => {
        unsafe extern "C" fn $name(ctx: *mut c_void, vaddr: u64, value: $ty, expected: $ty) -> i32 {
            // SAFETY: `ctx` is the harness context.
            unsafe {
                with(ctx, 0, |c| {
                    let a = (vaddr as usize) & (MEM_SIZE - 1);
                    let b = c.bytes();
                    let cur = <$ty>::from_le_bytes(b[a..a + $n].try_into().unwrap());
                    if cur != expected {
                        return 0;
                    }
                    b[a..a + $n].copy_from_slice(&value.to_le_bytes());
                    1
                })
            }
        }
    };
}

exclusive_cb!(cb_wx8, u8, 1);
exclusive_cb!(cb_wx16, u16, 2);
exclusive_cb!(cb_wx32, u32, 4);
exclusive_cb!(cb_wx64, u64, 8);

unsafe extern "C" fn cb_wx128(
    ctx: *mut c_void,
    vaddr: u64,
    value: *const u64,
    expected: *const u64,
) -> i32 {
    // SAFETY: `ctx` is the harness context; both arrays are two readable `u64`.
    unsafe {
        with(ctx, 0, |c| {
            let (vlo, vhi) = (*value, *value.add(1));
            let (elo, ehi) = (*expected, *expected.add(1));
            let a = (vaddr as usize) & (MEM_SIZE - 1);
            let b = c.bytes();
            let lo = u64::from_le_bytes(b[a..a + 8].try_into().unwrap());
            let hi = u64::from_le_bytes(b[a + 8..a + 16].try_into().unwrap());
            if lo != elo || hi != ehi {
                return 0;
            }
            b[a..a + 8].copy_from_slice(&vlo.to_le_bytes());
            b[a + 8..a + 16].copy_from_slice(&vhi.to_le_bytes());
            1
        })
    }
}

unsafe extern "C" fn cb_interpreter_fallback(ctx: *mut c_void, pc: u64, n: u64) {
    // SAFETY: `ctx` is the harness context.
    unsafe {
        with(ctx, (), |c| {
            c.fallbacks.push((pc, n));
            #[cfg(target_arch = "aarch64")]
            {
                let fpcr: u64;
                // SAFETY: `FPCR` is readable at EL0 and `mrs` touches no memory.
                core::arch::asm!("mrs {}, fpcr", out(reg) fpcr, options(nomem, nostack));
                c.fallback_host_fpcr = fpcr as u32;
            }
            if c.fallback_skips {
                od_jit_set_pc(c.jit, pc + 4 * n);
                return;
            }
            // Nothing here can interpret A64, so refuse loudly rather than
            // silently skipping instructions: record it and stop.
            c.exceptions.push((pc, u32::MAX));
            let jit = c.jit;
            if !jit.is_null() {
                od_jit_halt(jit, OD_HALT_USER7);
            }
        })
    }
}

unsafe extern "C" fn cb_call_svc(ctx: *mut c_void, swi: u32) {
    // SAFETY: `ctx` is the harness context.
    unsafe {
        with(ctx, (), |c| {
            c.svc.push(swi);
            let jit = c.jit;
            if c.reenter_on_svc {
                // Deliberate violation: guest code has reached a callback, and
                // the callback asks the jit to run again. dynarmic would
                // `ASSERT(!is_executing)` and terminate; the shim refuses.
                c.reenter_result = Some(od_jit_run(jit));
                c.reenter_step_result = Some(od_jit_step(jit));
            }
            if c.zero_invalidate_on_svc {
                od_jit_invalidate_range(jit, c.code_base, 0);
            }
            if c.halt_on_svc {
                od_jit_halt(jit, HALT_DONE);
            }
        })
    }
}

unsafe extern "C" fn cb_exception_raised(ctx: *mut c_void, pc: u64, kind: u32) {
    // SAFETY: `ctx` is the harness context.
    unsafe {
        with(ctx, (), |c| {
            c.exceptions.push((pc, kind));
            let jit = c.jit;
            if !jit.is_null() {
                od_jit_halt(jit, OD_HALT_USER6);
            }
        })
    }
}

unsafe extern "C" fn cb_icache_op(ctx: *mut c_void, op: u32, vaddr: u64) {
    // SAFETY: `ctx` is the harness context.
    unsafe {
        with(ctx, (), |c| c.icache.push((op, vaddr)))
    }
}

unsafe extern "C" fn cb_get_cntpct(ctx: *mut c_void) -> u64 {
    // SAFETY: `ctx` is the harness context.
    unsafe { with(ctx, 0, |c| c.ticks_used) }
}

unsafe extern "C" fn cb_add_ticks(ctx: *mut c_void, ticks: u64) {
    // SAFETY: `ctx` is the harness context.
    unsafe {
        with(ctx, (), |c| {
            c.ticks_used = c.ticks_used.saturating_add(ticks);
            c.ticks_remaining = c.ticks_remaining.saturating_sub(ticks);
        })
    }
}

unsafe extern "C" fn cb_get_ticks_remaining(ctx: *mut c_void) -> u64 {
    // SAFETY: `ctx` is the harness context.
    unsafe { with(ctx, 0, |c| c.ticks_remaining) }
}

/// The complete callback table. Every slot is filled; `od_jit_new` rejects a
/// table with a hole rather than letting generated code call address zero.
pub const CALLBACKS: OdCallbacks = OdCallbacks {
    read_code: Some(cb_read_code),
    read8: Some(cb_read8),
    read16: Some(cb_read16),
    read32: Some(cb_read32),
    read64: Some(cb_read64),
    read128: Some(cb_read128),
    write8: Some(cb_write8),
    write16: Some(cb_write16),
    write32: Some(cb_write32),
    write64: Some(cb_write64),
    write128: Some(cb_write128),
    write_exclusive8: Some(cb_wx8),
    write_exclusive16: Some(cb_wx16),
    write_exclusive32: Some(cb_wx32),
    write_exclusive64: Some(cb_wx64),
    write_exclusive128: Some(cb_wx128),
    interpreter_fallback: Some(cb_interpreter_fallback),
    call_svc: Some(cb_call_svc),
    exception_raised: Some(cb_exception_raised),
    instruction_cache_op: Some(cb_icache_op),
    get_cntpct: Some(cb_get_cntpct),
    add_ticks: Some(cb_add_ticks),
    get_ticks_remaining: Some(cb_get_ticks_remaining),
};

/// Knobs the tests vary.
#[derive(Clone, Copy)]
pub struct VmOptions {
    /// Route guest memory through fastmem rather than the callbacks.
    pub fastmem: bool,
    /// Enable the tick callbacks, which is how a step budget is enforced.
    pub cycle_counting: bool,
    /// Give the jit a shared exclusive monitor.
    pub monitor: bool,
    /// Raise `exception_raised` for hint instructions.
    pub hook_hints: bool,
    /// Use fastmem for `LDXR`/`STXR` rather than the monitor's callbacks.
    pub fastmem_exclusive: bool,
    /// Code cache bytes; 0 selects dynarmic's default.
    pub code_cache_size: u64,
    /// `OdConfig::optimizations`.
    pub optimizations: u32,
    /// An `od_monitor_new` handle shared with other `Vm`s (as `usize`, so options stay `Send`);
    /// 0 gives this `Vm` a monitor of its own when [`VmOptions::monitor`] is set.
    pub shared_monitor: usize,
    /// `OdConfig::processor_id`: this `Vm`'s index into a shared monitor.
    pub processor_id: u32,
    /// A guest data arena shared with other `Vm`s (`MEM_SIZE + MEM_GUARD` bytes, 8-aligned, as
    /// `usize`); 0 uses this `Vm`'s own.
    pub shared_arena: usize,
    /// `silently_mirror_fastmem`. Off, a guest address past `MEM_BITS` misses fastmem and goes to
    /// the callbacks (which mask it into the arena).
    pub mirror: bool,
}

impl Default for VmOptions {
    fn default() -> Self {
        Self {
            fastmem: true,
            cycle_counting: false,
            monitor: true,
            hook_hints: false,
            fastmem_exclusive: false,
            // 8 MiB: dynarmic's documented minimum, and the tests do not need
            // the 128 MiB default. D5 measured 20-35 MiB committed per thread,
            // so a test suite that made 30 jits at the default size would be a
            // memory problem of its own.
            code_cache_size: 8 << 20,
            optimizations: optimization::ALL_SAFE,
            shared_monitor: 0,
            processor_id: 0,
            shared_arena: 0,
            mirror: true,
        }
    }
}

/// One guest thread: a jit, its context and the pointers dynarmic bakes into
/// generated code.
pub struct Vm {
    jit: *mut c_void,
    /// Separate allocation from `Vm` on purpose: see the module docs.
    ctx: Box<UnsafeCell<Ctx>>,
    /// Boxed so the address dynarmic inlines into generated code never moves.
    tpidr: Box<u64>,
    tpidrro: Box<u64>,
    monitor: *mut c_void,
    /// Whether `Drop` frees `monitor` (not when it is shared).
    owns_monitor: bool,
}

impl Vm {
    /// Build a guest running `code` at [`CODE_BASE`].
    pub fn new(code: Vec<u32>, opts: VmOptions) -> Self {
        let mut mem = vec![0u64; (MEM_SIZE + MEM_GUARD) / 8];
        let arena = if opts.shared_arena != 0 { opts.shared_arena as *mut u64 } else { mem.as_mut_ptr() };
        let ctx = Box::new(UnsafeCell::new(Ctx {
            code_base: CODE_BASE,
            code,
            mem,
            arena,
            jit: std::ptr::null_mut(),
            svc: Vec::new(),
            exceptions: Vec::new(),
            icache: Vec::new(),
            fallbacks: Vec::new(),
            depth: 0,
            max_depth: 0,
            ticks_remaining: u64::MAX,
            ticks_used: 0,
            panic_msg: None,
            panic_on_read8: false,
            reenter_on_svc: false,
            reenter_result: None,
            reenter_step_result: None,
            halt_on_svc: true,
            fallback_skips: false,
            fallback_host_fpcr: 0,
            zero_invalidate_on_svc: false,
        }));

        let mut tpidr = Box::new(0u64);
        let tpidrro = Box::new(0u64);
        let owns_monitor = opts.monitor && opts.shared_monitor == 0;
        let monitor = if opts.shared_monitor != 0 {
            opts.shared_monitor as *mut c_void
        } else if opts.monitor {
            // SAFETY: freed exactly once in `Drop`, after the jit that uses it.
            unsafe { od_monitor_new(1) }
        } else {
            std::ptr::null_mut()
        };
        assert!(!opts.monitor || !monitor.is_null(), "od_monitor_new failed");

        // SAFETY: the box is live and not otherwise borrowed here.
        let fastmem_base = unsafe { (*ctx.get()).fastmem_base() };

        let cfg = OdConfig {
            abi_version: OD_DYNARMIC_ABI_VERSION,
            callbacks: &CALLBACKS,
            ctx: ctx.get().cast::<c_void>(),
            tpidr_el0: &mut *tpidr,
            tpidrro_el0: &*tpidrro,
            fastmem_enabled: i32::from(opts.fastmem),
            fastmem_pointer: fastmem_base,
            fastmem_address_space_bits: MEM_BITS,
            // Masks the guest address into the arena, so a wild guest address
            // wraps instead of reading off the end of the allocation.
            silently_mirror_fastmem: i32::from(opts.mirror),
            recompile_on_fastmem_failure: 1,
            fastmem_exclusive_access: i32::from(opts.fastmem_exclusive),
            monitor,
            processor_id: opts.processor_id,
            code_cache_size: opts.code_cache_size,
            cntfrq_el0: 0,
            ctr_el0: 0,
            dczid_el0: 4,
            enable_cycle_counting: i32::from(opts.cycle_counting),
            wall_clock_cntpct: 0,
            hook_hint_instructions: i32::from(opts.hook_hints),
            define_unpredictable_behaviour: 0,
            check_halt_on_memory_access: 0,
            unsafe_optimizations: 0,
            optimizations: opts.optimizations,
        };

        // SAFETY: `cfg` is fully initialised; `callbacks` is a `'static`
        // constant; `ctx`, `tpidr`, `tpidrro` and `monitor` are all owned by
        // the `Vm` being built and are dropped only in `Drop`, after
        // `od_jit_free`. dynarmic copies `cfg` and keeps the pointers.
        let jit = unsafe { od_jit_new(&cfg) };
        assert!(!jit.is_null(), "od_jit_new rejected the configuration");

        // SAFETY: nothing is executing yet, so no callback can hold a `&mut`.
        unsafe {
            (*ctx.get()).jit = jit;
        }

        Self { jit, ctx, tpidr, tpidrro, monitor, owns_monitor }
    }

    /// The raw jit handle, for tests that call the C ABI directly.
    pub fn raw(&self) -> *mut c_void {
        self.jit
    }

    /// Borrow the guest context. Must not be called while `run` is on the
    /// stack; on one thread that is structurally impossible.
    pub fn with_ctx<R>(&self, f: impl FnOnce(&mut Ctx) -> R) -> R {
        assert_eq!(
            // SAFETY: `self.jit` is live for `self`'s lifetime.
            unsafe { od_jit_is_executing(self.jit) },
            0,
            "with_ctx called while the jit is executing"
        );
        // SAFETY: not executing, so no callback holds a reference; `&self` only
        // shares the `Box`, and `UnsafeCell` permits forming `&mut` to its
        // contents.
        f(unsafe { &mut *self.ctx.get() })
    }

    /// Run until halted. Takes `&self`, which is what keeps a caller from
    /// holding `&mut Ctx` across the call.
    pub fn run(&self) -> u32 {
        // SAFETY: `self.jit` is live; callbacks cannot unwind past `with`.
        let hr = unsafe { od_jit_run(self.jit) };
        self.resume_panic();
        hr
    }

    /// Execute one guest instruction.
    pub fn step(&self) -> u32 {
        // SAFETY: as `run`.
        let hr = unsafe { od_jit_step(self.jit) };
        self.resume_panic();
        hr
    }

    /// Run until [`HALT_DONE`], or until `max_rounds` runs have returned for
    /// any other reason. Cache-invalidation halts are resumed, which is what a
    /// real dispatcher does.
    pub fn run_to_completion(&self, max_rounds: u32) -> u32 {
        let mut hr = 0;
        for _ in 0..max_rounds {
            hr = self.run();
            if hr & HALT_DONE != 0 {
                // SAFETY: `self.jit` is live and not executing.
                unsafe { od_jit_clear_halt(self.jit, HALT_DONE) };
                return hr;
            }
            if hr & OD_HALT_CACHE_INVALIDATION != 0 {
                // SAFETY: as above.
                unsafe { od_jit_clear_halt(self.jit, OD_HALT_CACHE_INVALIDATION) };
                continue;
            }
            if hr != 0 {
                return hr;
            }
            if self.with_ctx(|c| c.ticks_remaining) == 0 {
                return hr;
            }
        }
        panic!("guest did not finish in {max_rounds} rounds (last halt {hr:#x})");
    }

    fn resume_panic(&self) {
        // SAFETY: execution has returned, so no callback holds a reference.
        let msg = unsafe { (*self.ctx.get()).panic_msg.take() };
        if let Some(m) = msg {
            panic!("callback panicked: {m}");
        }
    }

    /// Read `X0`-`X30`.
    pub fn reg(&self, i: u32) -> u64 {
        // SAFETY: `self.jit` is live; the index is bounds-checked by the shim.
        unsafe { od_jit_get_reg(self.jit, i) }
    }

    /// Write `X0`-`X30`.
    pub fn set_reg(&self, i: u32, v: u64) {
        // SAFETY: as `reg`.
        unsafe { od_jit_set_reg(self.jit, i, v) }
    }

    /// Read `V0`-`V31` as `[low, high]`.
    pub fn vec(&self, i: u32) -> [u64; 2] {
        let mut out = [0u64; 2];
        // SAFETY: `out` is two writable `u64`.
        unsafe { od_jit_get_vec(self.jit, i, out.as_mut_ptr()) };
        out
    }

    /// Write `V0`-`V31`.
    pub fn set_vec(&self, i: u32, v: [u64; 2]) {
        // SAFETY: `v` is two readable `u64`.
        unsafe { od_jit_set_vec(self.jit, i, v.as_ptr()) }
    }

    /// Read `SP`.
    pub fn sp(&self) -> u64 {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_get_sp(self.jit) }
    }

    /// Write `SP`.
    pub fn set_sp(&self, v: u64) {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_set_sp(self.jit, v) }
    }

    /// Read `PC`.
    pub fn pc(&self) -> u64 {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_get_pc(self.jit) }
    }

    /// Write `PC`.
    pub fn set_pc(&self, v: u64) {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_set_pc(self.jit, v) }
    }

    /// Read `PSTATE`; `NZCV` is bits 31:28.
    pub fn pstate(&self) -> u32 {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_get_pstate(self.jit) }
    }

    /// Set `TPIDR_EL0`, which dynarmic reads through an inlined pointer.
    pub fn set_tpidr_el0(&mut self, v: u64) {
        *self.tpidr = v;
    }

    /// Host address of the `TPIDR_EL0` slot.
    pub fn tpidr_el0_ptr(&self) -> u64 {
        &*self.tpidr as *const u64 as u64
    }

    /// Set `TPIDRRO_EL0`.
    pub fn set_tpidrro_el0(&mut self, v: u64) {
        *self.tpidrro = v;
    }

    /// Callback-entry counters.
    pub fn stats(&self) -> OdStats {
        let mut s = OdStats::default();
        // SAFETY: `self.jit` is live and `s` is writable.
        unsafe { od_jit_stats(self.jit, &mut s) };
        s
    }

    /// Zero the callback-entry counters.
    pub fn reset_stats(&self) {
        // SAFETY: `self.jit` is live.
        unsafe { od_jit_reset_stats(self.jit) }
    }

    /// What dynarmic is actually configured with.
    pub fn effective_config(&self) -> OdEffectiveConfig {
        let mut c = OdEffectiveConfig::default();
        // SAFETY: `self.jit` is live and `c` is writable.
        unsafe { od_jit_effective_config(self.jit, &mut c) };
        c
    }

    /// Start executing at [`CODE_BASE`] with a clean tick budget.
    pub fn start(&self, budget: u64) {
        self.set_pc(CODE_BASE);
        self.with_ctx(|c| {
            c.ticks_remaining = budget;
            c.ticks_used = 0;
        });
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        // SAFETY: the jit is not executing (a `&mut self` exists), and each
        // handle is freed exactly once. The jit is freed before the monitor
        // because the jit holds a pointer to it.
        unsafe {
            od_jit_free(self.jit);
            if self.owns_monitor && !self.monitor.is_null() {
                od_monitor_free(self.monitor);
            }
        }
    }
}
