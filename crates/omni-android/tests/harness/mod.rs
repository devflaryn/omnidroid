//! A real guest for the boundary's tests: a real [`GuestSpace`], a real translating backend, real
//! translated ARM64 code, and a real thunk region in the same address space.
//!
//! Nothing here is a mock. Every claim the boundary makes is about where a value *is* when guest code
//! branches out of its world, and a mock register file would only establish that the mock puts it
//! where the test put it. The unit tests in `src/` do use a synthetic frame — deliberately, because
//! they are about the AAPCS64 rules rather than about the crossing — and these are the ones that put
//! real `BL`s in front of them.

#![allow(dead_code)]

pub mod a64;

use std::sync::Arc;

use omni_android::{Boundary, BoundaryBuilder};
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu, RunLimit};
use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};

/// Bytes of guest code region.
pub const CODE_BYTES: usize = 64 * 1024;
/// Bytes of guest data region: scratch for arguments, results, `va_list`s and save areas.
pub const DATA_BYTES: usize = 64 * 1024;
/// Bytes of guest stack. Generous: a callback nested eight deep still has room.
pub const STACK_BYTES: usize = 256 * 1024;

/// How long any one test's guest is allowed to run.
///
/// A counted budget rather than [`RunLimit::Unlimited`], because every program here is a handful of
/// instructions and a test that hung would hang the suite. Comfortably below `i64::MAX`, which is
/// D16's footgun: the emitted comparison is signed, so a budget with bit 63 set reads as already
/// spent and the guest returns to the dispatcher after every block.
pub const BUDGET: RunLimit = RunLimit::Instructions(10_000_000);

/// A guest address space with code, data and a stack, and a backend over it.
pub struct Guest {
    pub space: Arc<GuestSpace>,
    pub backend: DynarmicBackend,
    pub code: GuestAddr,
    pub data: GuestAddr,
    /// A mapped, committed, **read-only** page: a guest handing its own `.rodata` over as an output
    /// buffer.
    pub readonly: GuestAddr,
    pub stack_base: GuestAddr,
    /// `SP` at entry, 16-byte aligned as AAPCS64 requires at a public interface.
    pub stack_top: GuestAddr,
    /// An address inside the space that no mapping covers, taken from the region list.
    pub unmapped: GuestAddr,
    next_code: std::cell::Cell<usize>,
}

impl Guest {
    pub fn new() -> Self {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));
        let page = space.page_size();
        let map = |len: usize, protection: Protection, policy: CommitPolicy| {
            space
                .map_anonymous(Placement::Anywhere { align: page }, len, protection, policy)
                .expect("a guest mapping")
        };
        let code = map(CODE_BYTES, Protection::ReadWrite, CommitPolicy::Eager);
        let data = map(DATA_BYTES, Protection::ReadWrite, CommitPolicy::Eager);
        let readonly = map(page, Protection::ReadWrite, CommitPolicy::Eager);
        space.protect(readonly, page, Protection::Read).expect("drop to read-only");
        let stack_base = map(STACK_BYTES, Protection::ReadWrite, CommitPolicy::Eager);
        let unmapped = space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= page)
            .map(|r| (r.start + r.len / 2) & !0xF)
            .expect("some free address space");

        let backend = DynarmicBackend::new(Arc::clone(&space), DynarmicOptions::default())
            .expect("a translating backend");
        // The same assertion `omni-cpu`'s suites make, for the same reason: a backend with no demand
        // pager puts every guest fault on dynarmic's own handler and the 30-49x recompiled callback
        // path. The tests would pass and would have stopped testing the path they name.
        assert!(
            backend.owns_guest_paging(),
            "this guest has no demand pager, so it is not the configuration the boundary will run in"
        );
        Self {
            space,
            backend,
            code,
            data,
            readonly,
            stack_base,
            stack_top: (stack_base + STACK_BYTES) & !0xF,
            unmapped,
            next_code: std::cell::Cell::new(0),
        }
    }

    /// A boundary builder over **this** guest's address space, so its thunk region is identity-mapped
    /// alongside the code the tests write.
    pub fn boundary(&self, slots: usize) -> BoundaryBuilder {
        BoundaryBuilder::new(Arc::clone(&self.space), slots, 4096).expect("a thunk region")
    }

    /// Where the next [`load`](Guest::load) will put a program.
    ///
    /// Needed because a `BL` displacement is computed from the branch's own address, so a program that
    /// calls a thunk has to know where it will live before it is assembled.
    pub fn next_entry(&self) -> GuestAddr {
        self.code + self.next_code.get()
    }

    /// Write a program into the code region at the next free offset, and make it executable.
    ///
    /// Returns its entry address. Programs are packed in order rather than all placed at zero,
    /// because several tests load more than one and a second `load` must not overwrite the first.
    pub fn load(&self, program: &[u32]) -> GuestAddr {
        let offset = self.next_code.get();
        let bytes = program.len() * 4;
        assert!(offset + bytes <= CODE_BYTES, "the code region is full");
        let entry = self.code + offset;
        self.space
            .protect(self.code, CODE_BYTES, Protection::ReadWrite)
            .expect("code region writable");
        let ptr = self.space.ptr(entry, bytes).expect("a host pointer for the code");
        // SAFETY: `ptr` is `GuestSpace`'s own pointer for a committed, writable range of exactly this
        // length, and D4's identity mapping makes the guest address a host address. No guest thread is
        // running: `load` is called before `run`.
        unsafe {
            core::ptr::copy_nonoverlapping(program.as_ptr(), ptr.cast::<u32>(), program.len());
        }
        self.space
            .protect(self.code, CODE_BYTES, Protection::ReadExecute)
            .expect("code region executable");
        // Word-align the next program, and leave a gap so a fall-through past the end of one lands in
        // a `BRK`-free hole rather than at the top of the next.
        self.next_code.set(offset + bytes + 16);
        entry
    }

    /// A guest thread with the boundary installed, a stack, and `X30` set to the boundary's sentinel
    /// so the guest's own `RET` finishes the call.
    pub fn thread(&self, boundary: &Arc<Boundary>) -> DynarmicCpu {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        boundary.install(&mut cpu).expect("install the boundary");
        cpu.set_sp(self.stack_top);
        cpu.set_x(x(30), boundary.sentinel() as u64);
        cpu
    }

    /// `X30` and `SP` back where `thread` left them, for a context used for a second call.
    pub fn rearm(&self, cpu: &mut DynarmicCpu, boundary: &Arc<Boundary>) {
        cpu.set_sp(self.stack_top);
        cpu.set_x(x(30), boundary.sentinel() as u64);
    }

    pub fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `load`; the range is checked by `ptr` and the mapping is eagerly committed.
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }

    pub fn write_u64(&self, address: GuestAddr, value: u64) {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `read_u64`.
        unsafe { ptr.cast::<u64>().write_unaligned(value) }
    }

    pub fn write_u32(&self, address: GuestAddr, value: u32) {
        let ptr = self.space.ptr(address, 4).expect("a host pointer");
        // SAFETY: as `read_u64`.
        unsafe { ptr.cast::<u32>().write_unaligned(value) }
    }

    pub fn write_bytes(&self, address: GuestAddr, bytes: &[u8]) {
        let ptr = self.space.ptr(address, bytes.len()).expect("a host pointer");
        // SAFETY: as `read_u64`.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len()) }
    }

    pub fn read_f64(&self, address: GuestAddr) -> f64 {
        f64::from_bits(self.read_u64(address))
    }

    pub fn write_f64(&self, address: GuestAddr, value: f64) {
        self.write_u64(address, value.to_bits());
    }
}

/// `X{n}`, panicking on an index that names no register — a test bug, not a guest one.
pub fn x(n: u8) -> omni_cpu::XReg {
    omni_cpu::XReg::new(n).expect("a general-purpose register")
}

/// `V{n}`.
pub fn v(n: u8) -> omni_cpu::VReg {
    omni_cpu::VReg::new(n).expect("a SIMD register")
}

/// **Serializes every test in the binary.**
///
/// The handlers are bare `fn`s — that is what [`omni_android::ImportFn`] is — so what a handler saw
/// has to be recorded in a `static`, and two tests sharing one would be measuring each other. Each
/// test takes this first.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A tiny assembler that knows where it is, so a `BL` can be written against a target address.
pub struct Asm {
    at: GuestAddr,
    words: Vec<u32>,
}

impl Asm {
    /// Start assembling at `at`, which must be where the program will be loaded.
    pub fn at(at: GuestAddr) -> Self {
        Self { at, words: Vec::new() }
    }

    /// The address of the *next* instruction.
    pub fn pc(&self) -> GuestAddr {
        self.at + self.words.len() * 4
    }

    pub fn push(&mut self, word: u32) -> &mut Self {
        self.words.push(word);
        self
    }

    pub fn extend(&mut self, words: impl IntoIterator<Item = u32>) -> &mut Self {
        self.words.extend(words);
        self
    }

    /// `MOV Xd, #value`, however many instructions that takes.
    pub fn mov(&mut self, rd: u32, value: u64) -> &mut Self {
        self.extend(a64::mov64(rd, value))
    }

    /// `BL target`, with the displacement computed from here.
    pub fn bl(&mut self, target: GuestAddr) -> &mut Self {
        let word = a64::bl_to(self.pc(), target);
        self.push(word)
    }

    /// `B target`, with the displacement computed from here.
    pub fn b(&mut self, target: GuestAddr) -> &mut Self {
        let word = a64::b_to(self.pc(), target);
        self.push(word)
    }

    pub fn words(&self) -> &[u32] {
        &self.words
    }
}
