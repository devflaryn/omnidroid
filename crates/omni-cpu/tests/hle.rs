//! Native high-level emulation of hot `libc.so` functions (`DynarmicBackend::add_hle`,
//! `omni_cpu::dynarmic::set_hle`): a guest call to a registered entry runs a host `memcpy`/
//! `memmove`/`memset` with the AArch64 C ABI (args X0-X2, result X0, resume X30), and a fault
//! inside the copy becomes the same typed memory fault a guest access would.
//!
//! The switch is process-wide; these tests take [`SERIAL`] and register their own entries.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use harness::a64::*;
use harness::{x, Guest};
use omni_cpu::dynarmic::Hle;
use omni_cpu::{AccessKind, ExitReason, GuestAddr, GuestCpu, RunLimit};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialized() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

/// A guest "function": a single `RET` at a code offset, registered as an HLE entry. With the
/// switch on the native code runs; the `RET` is reached only if the switch is off (a distinguishing
/// no-op). Returns its guest address.
fn install(guest: &Guest, offset: usize, kind: Hle) -> GuestAddr {
    let entry = guest.load_at(offset, &[ret(30)]);
    guest.backend.add_hle(entry, kind);
    entry
}

/// Call the entry: args in X0-X2, X30 the sentinel, run to the return.
fn call(guest: &Guest, entry: GuestAddr, a0: u64, a1: u64, a2: u64) -> Result<(ExitReason, u64), ExitReason> {
    let (mut cpu, sentinel) = guest.thread();
    cpu.set_x(x(0), a0);
    cpu.set_x(x(1), a1);
    cpu.set_x(x(2), a2);
    cpu.set_x(x(30), sentinel as u64);
    let exit = cpu.run(entry, RunLimit::Unlimited).expect("an exit");
    if exit == (ExitReason::Returned { pc: sentinel }) {
        Ok((exit, cpu.x(x(0))))
    } else {
        Err(exit)
    }
}

fn bytes(guest: &Guest, addr: GuestAddr, n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| {
            let a = addr + i;
            ((guest.read_u64(a & !7) >> (8 * (a & 7))) & 0xff) as u8
        })
        .collect()
}

fn write_bytes(guest: &Guest, addr: GuestAddr, data: &[u8]) {
    for (i, &b) in data.iter().enumerate() {
        let a = addr + i;
        let word = a & !7;
        let shift = 8 * (a & 7);
        let v = (guest.read_u64(word) & !(0xffu64 << shift)) | (u64::from(b) << shift);
        guest.write_u64(word, v);
    }
}

#[test]
fn memcpy_of_various_sizes_and_alignments_matches_the_bytes() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memmove);
    let src = guest.data + 0x100;
    let dst = guest.data + 0x6000; // clear of src for the largest case: these are non-overlapping
    for (n, soff, doff) in [(64usize, 0usize, 0usize), (1, 0, 0), (7, 1, 3), (64, 1, 2), (255, 3, 5), (4096, 0, 0), (4097, 7, 1)] {
        let pattern: Vec<u8> = (0..n).map(|i| (i * 7 + 13) as u8).collect();
        write_bytes(&guest, src + soff, &pattern);
        write_bytes(&guest, dst + doff, &vec![0xEE; n]);
        let (_, ret) = call(&guest, entry, (dst + doff) as u64, (src + soff) as u64, n as u64).expect("returns");
        assert_eq!(ret, (dst + doff) as u64, "n={n}: result is the destination");
        assert_eq!(bytes(&guest, dst + doff, n), pattern, "n={n} soff={soff} doff={doff}");
    }
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn memset_fills_and_returns_the_destination() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memset);
    let dst = guest.data + 0x200;
    for (n, off, byte) in [(64usize, 0usize, 0xABu64), (1, 3, 0), (200, 5, 0xFF), (4096, 0, 0x5A)] {
        write_bytes(&guest, dst + off, &vec![0x11; n]);
        let (_, ret) = call(&guest, entry, (dst + off) as u64, byte, n as u64).expect("returns");
        assert_eq!(ret, (dst + off) as u64);
        assert_eq!(bytes(&guest, dst + off, n), vec![byte as u8; n], "n={n}");
    }
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn memmove_handles_overlap_in_both_directions() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memmove);
    let base = guest.data + 0x400;
    let pattern: Vec<u8> = (0..256).map(|i| i as u8).collect();
    // Forward overlap: dst > src.
    write_bytes(&guest, base, &pattern);
    call(&guest, entry, (base + 16) as u64, base as u64, 240).expect("returns");
    assert_eq!(bytes(&guest, base + 16, 240), pattern[..240], "dst>src");
    // Backward overlap: dst < src.
    write_bytes(&guest, base, &pattern);
    call(&guest, entry, base as u64, (base + 16) as u64, 240).expect("returns");
    assert_eq!(bytes(&guest, base, 240), pattern[16..256], "dst<src");
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn a_zero_length_call_touches_nothing_and_returns_the_destination() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let cpy = install(&guest, 0, Hle::Memmove);
    let set = install(&guest, 4, Hle::Memset);
    // Length 0 with an unmapped pointer must not fault (no byte is accessed).
    let (_, r) = call(&guest, cpy, guest.unmapped as u64, guest.unmapped as u64, 0).expect("returns");
    assert_eq!(r, guest.unmapped as u64);
    let (_, r) = call(&guest, set, guest.unmapped as u64, 0, 0).expect("returns");
    assert_eq!(r, guest.unmapped as u64);
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn a_copy_into_unmapped_memory_faults_at_that_address() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memmove);
    // Destination unmapped: a write fault at the destination (resolved before the read).
    match call(&guest, entry, guest.unmapped as u64, guest.data as u64, 64) {
        Err(ExitReason::MemoryFault { address, access, .. }) => {
            assert_eq!(address, guest.unmapped, "the faulting address");
            assert_eq!(access, AccessKind::Write);
        }
        other => panic!("expected a write fault, got {other:?}"),
    }
    // Source unmapped: a read fault at the source.
    match call(&guest, entry, guest.data as u64, guest.unmapped as u64, 64) {
        Err(ExitReason::MemoryFault { address, access, .. }) => {
            assert_eq!(address, guest.unmapped);
            assert_eq!(access, AccessKind::Read);
        }
        other => panic!("expected a read fault, got {other:?}"),
    }
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn a_memset_into_a_read_only_page_faults_as_a_write() {
    let _serial = serialized();
    omni_cpu::dynarmic::set_hle(true);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memset);
    match call(&guest, entry, guest.readonly as u64, 0, 64) {
        Err(ExitReason::MemoryFault { address, access, .. }) => {
            assert_eq!(address, guest.readonly);
            assert_eq!(access, AccessKind::Write);
        }
        other => panic!("expected a write fault, got {other:?}"),
    }
    omni_cpu::dynarmic::set_hle(false);
}

#[test]
fn the_switch_off_runs_the_guest_code_instead() {
    let _serial = serialized();
    // With the switch off, the entry is not planted: the guest's own instruction (here a bare RET)
    // runs. The memory left untouched tells it apart from the native memset.
    omni_cpu::dynarmic::set_hle(false);
    let guest = Guest::new();
    let entry = install(&guest, 0, Hle::Memset);
    let dst = guest.data + 0x200;
    write_bytes(&guest, dst, &[0x11; 32]);
    let (_, r) = call(&guest, entry, dst as u64, 0xAB, 32).expect("the RET returns");
    assert_eq!(r, dst as u64, "X0 is unchanged by a bare RET");
    assert_eq!(bytes(&guest, dst, 32), vec![0x11; 32], "the guest code (a no-op RET) did not fill");
}
