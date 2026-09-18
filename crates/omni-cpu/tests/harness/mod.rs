//! A real guest: a real [`GuestSpace`], a real code region, a real translating backend.
//!
//! Nothing here is a mock. The point of Task 3's tests is to assert facts about a **live**
//! configuration — zero callback-path entries, a startup check that fires, a thread pointer guest
//! code can read — and a mock would assert facts about the mock.
//!
//! The guest address space is placed wherever Windows puts a 4 GiB reservation, which on this host
//! is around **0x1C8_26EC_0000**: bit 40, so 41 address bits. That matters, and it is checked rather
//! than assumed by [`Guest::assert_high_addresses`]: the whole of D4's second footgun is that
//! dynarmic's default `fastmem_address_space_bits` of **36** covers only the bottom 64 GiB, so a
//! test whose guest happened to live below that would pass with the wrong configuration and prove
//! nothing.

#![allow(dead_code)]

pub mod a64;
pub mod roblox;

use std::sync::Arc;

use omni_cpu::dynarmic::{DynarmicBackend, DynarmicCpu, DynarmicOptions};
use omni_cpu::{GuestAddr, GuestCpu};
use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};

/// Bytes of guest code region. Enough for the largest test program many times over.
pub const CODE_BYTES: usize = 64 * 1024;
/// Bytes of guest data region.
pub const DATA_BYTES: usize = 64 * 1024;
/// Bytes of the lazily-committed region.
///
/// Larger than the eager one because the demand-paging tests want a page per guest thread, and it
/// costs nothing to reserve: the mapping is `CommitPolicy::Lazy`, so its charge is whatever the
/// guest actually touches (D10).
pub const LAZY_BYTES: usize = 512 * 1024;

/// A guest address space with a code region, a data region and a backend over it.
pub struct Guest {
    pub space: Arc<GuestSpace>,
    pub backend: DynarmicBackend,
    pub code: GuestAddr,
    pub data: GuestAddr,
    /// A mapped, committed, **read-only** page, for testing a guest write that must be refused.
    pub readonly: GuestAddr,
    /// A mapped but **lazily committed** region, for testing demand paging.
    pub lazy: GuestAddr,
    /// An address inside the space with nothing mapped at it at all.
    pub unmapped: GuestAddr,
}

impl Guest {
    /// Build a guest with the default backend options.
    pub fn new() -> Self {
        Self::with_options(DynarmicOptions::default())
    }

    pub fn with_options(options: DynarmicOptions) -> Self {
        let space = Arc::new(GuestSpace::new().expect("a guest address space"));

        let code = space
            .map_anonymous(
                Placement::Anywhere { align: space.page_size() },
                CODE_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a code region");
        let data = space
            .map_anonymous(
                Placement::Anywhere { align: space.page_size() },
                DATA_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a data region");
        let readonly = space
            .map_anonymous(
                Placement::Anywhere { align: space.page_size() },
                space.page_size(),
                Protection::ReadWrite,
                CommitPolicy::Eager,
            )
            .expect("a read-only region");
        space.protect(readonly, space.page_size(), Protection::Read).expect("drop to read-only");
        let lazy = space
            .map_anonymous(
                Placement::Anywhere { align: space.page_size() },
                LAZY_BYTES,
                Protection::ReadWrite,
                CommitPolicy::Lazy,
            )
            .expect("a lazily-committed region");

        // Somewhere inside the space that no mapping covers. The region list is authoritative, so
        // it is taken from there rather than guessed at.
        let unmapped = space
            .regions()
            .into_iter()
            .find(|r| r.is_free() && r.len >= space.page_size())
            .map(|r| (r.start + r.len / 2) & !7)
            .expect("some free address space");

        let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
        // **Every guest in these suites owns guest paging, and that is asserted rather than
        // assumed.** Before Task 4 the vectored-handler table held 8 slots and
        // `DynarmicBackend::new` swallowed `HandlerTableFull`, so a binary with more than eight
        // tests running in parallel could hand out a backend with no pager — which puts every guest
        // fault on dynarmic's own handler and the 30-49x recompiled callback path. The tests would
        // still pass; they would just have stopped testing the path they name. `hostile.rs` has 14
        // such tests and was the suite that could reach it.
        //
        // The table is 32 now and the refusal is loud, so this is belt and braces. It stays because
        // the failure it guards against is a *coverage* failure, and a coverage failure is invisible
        // by construction.
        assert!(
            backend.owns_guest_paging(),
            "this guest has no demand pager, so every fault it takes goes to dynarmic's own \
             handler and permanently deoptimizes the block. The test would pass and prove nothing"
        );
        Self { space, backend, code, data, readonly, lazy, unmapped }
    }

    /// Check the premise every identity-mapping test rests on: the guest lives above the 64 GiB that
    /// dynarmic's default `fastmem_address_space_bits = 36` would cover.
    ///
    /// If this ever fails, the tests below stop testing what they claim to — they would pass under
    /// the wrong configuration too — so it fails loudly rather than being skipped.
    pub fn assert_high_addresses(&self) {
        let top = self.space.end() - 1;
        assert!(
            top >= 1usize << 36,
            "this guest space tops out at {top:#x}, which dynarmic's default 36-bit fastmem window \
             would cover. The identity-mapping tests would then pass under the wrong configuration \
             and prove nothing"
        );
    }

    /// Write a program into the code region and make it executable. Returns its entry address.
    pub fn load(&self, program: &[u32]) -> GuestAddr {
        self.load_at(0, program)
    }

    /// Write a program at `offset` bytes into the code region.
    pub fn load_at(&self, offset: usize, program: &[u32]) -> GuestAddr {
        assert!(offset % 4 == 0 && offset + program.len() * 4 <= CODE_BYTES);
        let entry = self.code + offset;
        self.space
            .protect(self.code, CODE_BYTES, Protection::ReadWrite)
            .expect("code region writable");
        let ptr = self.space.ptr(entry, program.len() * 4).expect("a host pointer for the code");
        // SAFETY: `ptr` is `GuestSpace`'s own pointer for a committed, writable range of exactly
        // this length, and identity mapping (D4) makes it a real host pointer. No guest thread is
        // running: `load` is called before `run`.
        unsafe {
            core::ptr::copy_nonoverlapping(program.as_ptr(), ptr.cast::<u32>(), program.len());
        }
        self.space
            .protect(self.code, CODE_BYTES, Protection::ReadExecute)
            .expect("code region executable");
        entry
    }

    /// Read a `u64` out of guest memory.
    pub fn read_u64(&self, address: GuestAddr) -> u64 {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `load`; the range is checked by `ptr` and committed by the caller.
        unsafe { ptr.cast::<u64>().read_unaligned() }
    }

    /// Write a `u64` into guest memory.
    pub fn write_u64(&self, address: GuestAddr, value: u64) {
        let ptr = self.space.ptr(address, 8).expect("a host pointer");
        // SAFETY: as `load`.
        unsafe { ptr.cast::<u64>().write_unaligned(value) }
    }

    /// A context with its bionic TLS block (D13), and a return sentinel armed.
    ///
    /// The sentinel is placed at the top of the code region: an address that is inside the guest
    /// space — so it is a legal `X30` value — but that no program writes to.
    pub fn thread(&self) -> (DynarmicCpu, GuestAddr) {
        let mut cpu = self.backend.create_thread_with_tls().expect("a guest thread");
        let sentinel = self.code + CODE_BYTES - 4;
        cpu.set_return_sentinel(sentinel).expect("arm the sentinel");
        cpu.set_x(omni_cpu::XReg::new(30).expect("X30"), sentinel as u64);
        (cpu, sentinel)
    }
}

/// `X{n}`, panicking on an index that names no register — which is a test bug, not a guest one.
pub fn x(n: u8) -> omni_cpu::XReg {
    omni_cpu::XReg::new(n).expect("a general-purpose register")
}
