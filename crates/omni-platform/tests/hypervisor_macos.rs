//! The hypervisor seam on Apple silicon, run for real.
//!
//! ```text
//! CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER=$PWD/tools/hvf_run.sh \
//!     cargo test -p omni-platform --features hypervisor --test hypervisor_macos
//! ```
//!
//! The runner signs the binary with `com.apple.security.hypervisor`; without it every test here
//! fails at `Vm::get` with `HvError::Denied`, which is the refusal working, not a skip.
//!
//! Guest code here runs at **EL1 with the MMU off**, so a guest address *is* a guest-physical one
//! and the only translation is stage 2 -- the half this crate owns. Every test is serialized: the VM
//! and its vCPU count are process-wide.

#![cfg(all(feature = "hypervisor", target_os = "macos", target_arch = "aarch64"))]

use std::sync::{Mutex, MutexGuard};

use omni_platform::hypervisor::{HvError, Reg, Stage2, SysReg, Vcpu, VcpuExit, Vm};
use omni_platform::vm::{self, Protection};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `hvc #imm`.
const fn hvc(imm: u16) -> u32 {
    0xD400_0002 | ((imm as u32) << 5)
}
const LDR_X0_X1: u32 = 0xF940_0020; // ldr x0, [x1]
const STR_X2_X1: u32 = 0xF900_0022; // str x2, [x1]
/// EL1h, every exception masked.
const EL1H: u64 = 0x3C5;

/// A page of guest code at `ipa`, outside every attachment, mapped read+execute. Leaked on
/// purpose: `map_private` is for the life of the process.
fn code_page(ipa: u64, words: &[u32]) -> u64 {
    let page = vm::page_size();
    let layout = std::alloc::Layout::from_size_align(page, page).expect("a page layout");
    // SAFETY: a non-zero size.
    let host = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!host.is_null());
    for (i, word) in words.iter().enumerate() {
        // SAFETY: inside the page just allocated.
        unsafe { host.cast::<u32>().add(i).write(*word) };
    }
    let vm = Vm::get().expect("the VM");
    // SAFETY: the page is never freed.
    unsafe { vm.map_private(host, ipa, page, Stage2::READ_EXECUTE) }.expect("map the code page");
    ipa
}

fn run_at(cpu: &mut Vcpu, pc: u64) -> VcpuExit {
    cpu.set_reg(Reg::Pc, pc).expect("PC");
    cpu.set_reg(Reg::Cpsr, EL1H).expect("CPSR");
    cpu.run().expect("hv_vcpu_run")
}

fn exception_class(exit: VcpuExit) -> (u64, u64, u64) {
    match exit {
        VcpuExit::Exception { syndrome, virtual_address, .. } => {
            (syndrome >> 26, syndrome & 0x1FF_FFFF, virtual_address)
        }
        other => panic!("expected an exception exit, got {other:?}"),
    }
}

const EC_HVC: u64 = 0x16;
const EC_DATA_ABORT_LOWER: u64 = 0x24;
/// `ISS.WnR`: the abort was a write.
const WNR: u64 = 1 << 6;

#[test]
fn the_host_limits_are_read_from_the_host() {
    let _serial = serial();
    let vm = Vm::get().expect("the VM (is the binary signed? see tools/hvf_run.sh)");
    let limits = vm.limits();
    println!("hypervisor limits on this host: {limits:?}");
    // Architectural bounds, not this machine's values: an IPA is 32-52 bits and a VM has vCPUs.
    assert!((32..=52).contains(&limits.ipa_bits), "{limits:?}");
    assert!(limits.max_vcpus >= 1, "{limits:?}");
    assert!(Vm::get().is_ok(), "a second get is the same VM, not a second hv_vm_create");
}

#[test]
fn every_register_round_trips_through_a_vcpu() {
    let _serial = serial();
    let mut cpu = Vcpu::create().expect("a vCPU");
    for n in 0..=30u8 {
        cpu.set_reg(Reg::X(n), 0x0101_0101_0101_0101u64.wrapping_mul(u64::from(n) + 3)).expect("X");
    }
    for n in 0..=30u8 {
        assert_eq!(
            cpu.reg(Reg::X(n)).expect("X"),
            0x0101_0101_0101_0101u64.wrapping_mul(u64::from(n) + 3),
            "X{n}"
        );
    }
    assert!(matches!(cpu.reg(Reg::X(31)), Err(HvError::Range { .. })), "X31 is not a register here");
    // Every byte distinct, so a lane swap, a half swap or a byte reversal each fails.
    for n in 0..32u8 {
        let value = u128::from_le_bytes(core::array::from_fn(|i| (i as u8) ^ (n << 3) ^ 0xA5));
        cpu.set_simd(n, value).expect("set Q");
        assert_eq!(cpu.simd(n).expect("get Q"), value, "Q{n}");
    }
    assert!(matches!(cpu.simd(32), Err(HvError::Range { .. })));
    for (reg, value) in [
        (SysReg::TpidrEl0, 0x7777_0000_1234_5678u64),
        (SysReg::TpidrroEl0, 0x7777_0000_8765_4321),
        (SysReg::SpEl0, 0x0000_0003_0000_0ff0),
        (SysReg::VbarEl1, 0x1000_0800),
    ] {
        cpu.set_sys_reg(reg, value).expect("set sys reg");
        assert_eq!(cpu.sys_reg(reg).expect("get sys reg"), value, "{reg:?}");
    }
}

#[test]
fn an_el1_hvc_exits_with_its_immediate() {
    let _serial = serial();
    let pc = code_page(0x2000_0000, &[hvc(5), hvc(6)]);
    let mut cpu = Vcpu::create().expect("a vCPU");
    let (ec, iss, _) = exception_class(run_at(&mut cpu, pc));
    assert_eq!((ec, iss), (EC_HVC, 5), "the guest's own hvc #5 must be the exit");
    assert_eq!(cpu.reg(Reg::Pc).expect("PC"), pc + 4, "an hvc's return address is the next word");
    let (ec, iss, _) = exception_class(cpu.run().expect("resume"));
    assert_eq!((ec, iss), (EC_HVC, 6), "resuming runs the next instruction");
}

/// The whole reason the mirror exists, fact by fact: stage 2 follows the host's protection and the
/// host's replacement of a range, and stops following it when detached.
#[test]
fn stage_two_follows_host_protection_and_replacement_of_an_attached_range() {
    let _serial = serial();
    let page = vm::page_size();
    let vm_ = Vm::get().expect("the VM");
    let load = code_page(0x2100_0000, &[LDR_X0_X1, hvc(1)]);
    let store = code_page(0x2200_0000, &[STR_X2_X1, hvc(2)]);
    let reservation = vm::reserve(4 * page, page).expect("a reservation");
    let base = reservation.base();
    let before = vm_.stage2_stats();
    let attachment = vm_.attach(base, reservation.len()).expect("attach");
    let mut cpu = Vcpu::create().expect("a vCPU");
    cpu.set_reg(Reg::X(1), base as u64).expect("X1");

    // Reserved, PROT_NONE: unmapped at stage 2, so the guest faults where the host would.
    let (ec, _, address) = exception_class(run_at(&mut cpu, load));
    assert_eq!((ec, address), (EC_DATA_ABORT_LOWER, base as u64), "a PROT_NONE page must fault");

    // Committed read-write, through the seam: the hook maps it, and host and guest share the page.
    // SAFETY: the first page of a reservation this test owns.
    unsafe { vm::commit(reservation.as_ptr(), page, Protection::ReadWrite) }.expect("commit");
    // SAFETY: just committed read-write.
    unsafe { reservation.as_ptr().cast::<u64>().write(0x1122_3344_5566_7788) };
    let (ec, iss, _) = exception_class(run_at(&mut cpu, load));
    assert_eq!((ec, iss), (EC_HVC, 1));
    assert_eq!(cpu.reg(Reg::X(0)).expect("X0"), 0x1122_3344_5566_7788, "the guest reads the host's write");
    cpu.set_reg(Reg::X(2), 0xDEAD_BEEF_0000_0001).expect("X2");
    let (ec, iss, _) = exception_class(run_at(&mut cpu, store));
    assert_eq!((ec, iss), (EC_HVC, 2));
    // SAFETY: committed read-write above.
    assert_eq!(unsafe { reservation.as_ptr().cast::<u64>().read() }, 0xDEAD_BEEF_0000_0001);

    // Protected read-only: the guest's store is refused at stage 2 and names itself a write.
    // SAFETY: as above.
    unsafe { vm::protect(reservation.as_ptr(), page, Protection::Read) }.expect("protect");
    let (ec, iss, address) = exception_class(run_at(&mut cpu, store));
    assert_eq!((ec, address), (EC_DATA_ABORT_LOWER, base as u64));
    assert_ne!(iss & WNR, 0, "a refused store is reported as a write");
    let (ec, _, _) = exception_class(run_at(&mut cpu, load));
    assert_eq!(ec, EC_HVC, "a read-only page is still readable");

    // Decommitted: a fresh object replaces the page. Without the mirror the guest would still read
    // 0xDEAD_BEEF_0000_0001 from the old one (MEASURED in the probe); with it the page is gone...
    // SAFETY: as above.
    unsafe { vm::decommit(reservation.as_ptr(), page) }.expect("decommit");
    let (ec, _, _) = exception_class(run_at(&mut cpu, load));
    assert_eq!(ec, EC_DATA_ABORT_LOWER, "a decommitted page must not be readable by the guest");
    // ...and committed again, it reads zero, as D10 requires.
    // SAFETY: as above.
    unsafe { vm::commit(reservation.as_ptr(), page, Protection::ReadWrite) }.expect("recommit");
    let (ec, _, _) = exception_class(run_at(&mut cpu, load));
    assert_eq!(ec, EC_HVC);
    assert_eq!(cpu.reg(Reg::X(0)).expect("X0"), 0, "a recommitted page reads zero, not the old contents");

    let during = vm_.stage2_stats();
    assert!(during.mirrored_changes >= before.mirrored_changes + 4, "{before:?} -> {during:?}");
    assert_eq!(during.failures, 0, "{during:?}");

    // Detached: unmapped for the guest, and host changes are no longer mirrored.
    drop(attachment);
    let (ec, _, _) = exception_class(run_at(&mut cpu, load));
    assert_eq!(ec, EC_DATA_ABORT_LOWER, "a detached range is gone from stage 2");
    let detached = vm_.stage2_stats();
    // SAFETY: as above.
    unsafe { vm::protect(reservation.as_ptr(), page, Protection::ReadWrite) }.expect("protect");
    assert_eq!(vm_.stage2_stats().mirrored_changes, detached.mirrored_changes, "nothing is attached");
    drop(cpu);
    vm::release(reservation).expect("release");
}

#[test]
fn an_overlay_pins_a_page_until_it_is_removed() {
    let _serial = serial();
    let page = vm::page_size();
    let vm_ = Vm::get().expect("the VM");
    let load = code_page(0x2300_0000, &[LDR_X0_X1, hvc(1)]);
    let reservation = vm::reserve(2 * page, page).expect("a reservation");
    // SAFETY: a reservation this test owns.
    unsafe { vm::commit(reservation.as_ptr(), 2 * page, Protection::ReadWrite) }.expect("commit");
    // SAFETY: committed.
    unsafe { reservation.as_ptr().cast::<u64>().write(1) };
    let _attachment = vm_.attach(reservation.base(), reservation.len()).expect("attach");

    let layout = std::alloc::Layout::from_size_align(page, page).expect("layout");
    // SAFETY: non-zero size; leaked for the life of the process, as an overlay page may be.
    let other = unsafe { std::alloc::alloc_zeroed(layout) };
    // SAFETY: inside the page.
    unsafe { other.cast::<u64>().write(2) };
    // SAFETY: `other` is never freed.
    unsafe { vm_.overlay(reservation.base() as u64, other, Stage2::READ_EXECUTE) }.expect("overlay");

    let mut cpu = Vcpu::create().expect("a vCPU");
    cpu.set_reg(Reg::X(1), reservation.base() as u64).expect("X1");
    let _ = exception_class(run_at(&mut cpu, load));
    assert_eq!(cpu.reg(Reg::X(0)).expect("X0"), 2, "the overlay page, not the host's");
    // A host change to the overlaid page does not disturb it...
    // SAFETY: as above.
    unsafe { vm::protect(reservation.as_ptr(), page, Protection::Read) }.expect("protect");
    let _ = exception_class(run_at(&mut cpu, load));
    assert_eq!(cpu.reg(Reg::X(0)).expect("X0"), 2, "still the overlay after a host protect");
    // ...and removing it gives the page back to the host's truth.
    assert!(vm_.remove_overlay(reservation.base() as u64).expect("remove"));
    assert!(!vm_.remove_overlay(reservation.base() as u64).expect("remove twice"));
    let _ = exception_class(run_at(&mut cpu, load));
    assert_eq!(cpu.reg(Reg::X(0)).expect("X0"), 1, "the host's page again");
    // Refusals: an overlay outside every attachment, and an attachment over an attached range.
    // SAFETY: refused before anything is mapped.
    let outside = unsafe { vm_.overlay(0x2400_0000, other, Stage2::READ_EXECUTE) };
    assert!(matches!(outside, Err(HvError::Range { .. })), "{outside:?}");
    let twice = vm_.attach(reservation.base(), reservation.len());
    assert!(matches!(twice, Err(HvError::Range { .. })), "{twice:?}");
}

#[test]
fn an_attachment_beyond_the_ipa_space_or_unaligned_is_refused() {
    let _serial = serial();
    let vm_ = Vm::get().expect("the VM");
    let top = 1usize << vm_.limits().ipa_bits;
    let page = vm::page_size();
    for (base, len) in [(top - page, 2 * page), (top, page), (page + 1, page), (page, page + 1), (page, 0)] {
        let result = vm_.attach(base, len);
        assert!(matches!(result, Err(HvError::Range { .. })), "{base:#x}+{len:#x}: {result:?}");
    }
}

#[test]
fn the_vcpu_limit_is_a_typed_refusal() {
    let _serial = serial();
    let vm_ = Vm::get().expect("the VM");
    let max = vm_.limits().max_vcpus;
    let room = max - vm_.live_vcpus();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let release = std::sync::Arc::new(std::sync::Barrier::new(room as usize + 1));
    let holders: Vec<_> = (0..room)
        .map(|_| {
            let ready = ready_tx.clone();
            let release = std::sync::Arc::clone(&release);
            std::thread::spawn(move || {
                let cpu = Vcpu::create();
                ready.send(cpu.is_ok()).expect("report");
                release.wait();
                drop(cpu);
            })
        })
        .collect();
    let created = (0..room).filter(|_| ready_rx.recv().expect("a holder")).count();
    assert_eq!(created as u32, room, "every vCPU up to the limit is created");
    assert_eq!(vm_.live_vcpus(), max);
    let refused = Vcpu::create();
    assert!(
        matches!(refused, Err(HvError::VcpuLimit { live, max: m }) if live == max && m == max),
        "{refused:?}"
    );
    release.wait();
    for holder in holders {
        holder.join().expect("a holder thread");
    }
    assert!(Vcpu::create().is_ok(), "a vCPU is available again once a holder's thread has exited");
}
