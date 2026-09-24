//! **A guest store-exclusive to a read-only page is a typed fault, not a dead process** (dynarmic
//! patch 0014), on the real `libroblox.so`.
//!
//! The native-backend workstream's survey of all 245,117 `.eh_frame` functions killed the process
//! on dynarmic's arm64 backend at `0x224822c` (docs/ports/macos-hvf.md 4.7). Reduced by delta
//! debugging to two functions, called as the survey called them (two 1 MiB buffers as arguments):
//! `0x2247264` leaves a pointer into `.data.rel.ro` in the first buffer, and `0x224822c` hands it to
//! the outlined `__aarch64_swp8_rel`, whose `LDXR` reads the sealed page and whose `STLXR` faults on
//! the write. Patch 0007's inline store-exclusive had registered only its load as a fastmem patch
//! location, so dynarmic's own handler found no record of the store and aborted the process -- every
//! guest thread with it, which Global Constraint 11 forbids. The fault must instead come back as the
//! write it is.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::roblox::{serialized, Roblox};
use harness::x;
use omni_cpu::{AccessKind, ExitReason, GuestAddr, GuestCpu, RunLimit};
use omni_mem::{CommitPolicy, GuestSpace, Placement, Protection};

const LEAVES_A_RELRO_POINTER: usize = 0x224_7264;
const SWAPS_THROUGH_IT: usize = 0x224_822c;
/// `.data.rel.ro` of this build: `[0x62dc1c0, 0x67c2790)` (`objdump -h`).
const DATA_REL_RO: std::ops::Range<usize> = 0x62d_c1c0..0x67c_2790;
const BUFFER: usize = 1 << 20;

fn call_with_buffers(roblox: &Roblox, cpu: &mut dyn GuestCpu, buffers: GuestAddr, entry: usize) -> ExitReason {
    cpu.set_x(x(0), buffers as u64);
    cpu.set_x(x(1), BUFFER as u64);
    cpu.set_x(x(2), (buffers + BUFFER) as u64);
    cpu.set_x(x(3), BUFFER as u64);
    for r in 4..8u8 {
        cpu.set_x(x(r), 64);
    }
    cpu.set_sp(roblox.stack_top);
    cpu.set_x(x(30), roblox.sentinel as u64);
    cpu.run(roblox.object.base + entry, RunLimit::Instructions(5_000_000)).expect("a run, not an error")
}

fn fill(space: &GuestSpace, buffers: GuestAddr) {
    let ptr = space.ptr(buffers, 2 * BUFFER).expect("the buffers");
    for i in 0..(2 * BUFFER / 8) {
        // SAFETY: inside the committed buffers; no guest is running.
        unsafe { ptr.cast::<u64>().add(i).write((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1) };
    }
}

#[test]
fn a_store_exclusive_into_sealed_relro_comes_back_as_a_write_fault() {
    let _serial = serialized();
    let Some(roblox) = Roblox::load() else { return };
    let buffers = roblox
        .space
        .map_anonymous(Placement::Anywhere { align: roblox.space.page_size() }, 2 * BUFFER, Protection::ReadWrite, CommitPolicy::Eager)
        .expect("argument buffers");
    fill(&roblox.space, buffers);
    let mut cpu = roblox.thread();

    // The first function runs off the end of what it was given and jumps to 0: a typed fault too.
    let first = call_with_buffers(&roblox, &mut cpu, buffers, LEAVES_A_RELRO_POINTER);
    assert!(matches!(first, ExitReason::MemoryFault { .. }), "0x2247264: {first:?}");

    // The second is the one that killed the process.
    let exit = call_with_buffers(&roblox, &mut cpu, buffers, SWAPS_THROUGH_IT);
    let ExitReason::MemoryFault { address, access, .. } = exit else {
        panic!("0x224822c must stop at the store-exclusive's write fault: {exit:?}");
    };
    assert_eq!(access, AccessKind::Write, "the load-exclusive read the page; the store faulted");
    let link = address - roblox.object.base;
    assert!(DATA_REL_RO.contains(&link), "the store's address {link:#x} is in .data.rel.ro");
    let pc_link = cpu.pc() - roblox.object.base;
    println!("0x224822c -> write fault at link {link:#x}, guest pc link {pc_link:#x}");
}
