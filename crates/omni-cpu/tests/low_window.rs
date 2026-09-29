//! D41: a space with a low window (macOS maps nothing below 4 GiB, and ART's heap must be there).
//! dynarmic's arm64 backend adds the window's base below 2^32 only (vendored patch 0030), so guest
//! loads, stores and exclusive pairs at a low guest address reach `W + g` on the direct path, and
//! those above 4 GiB stay the identity -- with no slow-path entry either way.
#![cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), feature = "dynarmic"))]

mod harness;

use std::sync::{Arc, Mutex};

use harness::a64::*;
use harness::x;
use omni_cpu::dynarmic::{DynarmicBackend, DynarmicOptions};
use omni_cpu::{ExitReason, GuestCpu, GuestRange, RunLimit};
use omni_mem::{CommitPolicy, GuestSpace, GuestSpaceConfig, Placement, Protection, LOW_WINDOW_END};

/// `omni_linux::process::GUEST_SPACE_LOW_BASE`.
const BASE: usize = 0x1000_0000;
/// ART's boot image address: guest data in the window.
const LOW_DATA: usize = 0x7000_0000;
/// A lazily committed range in the window, for demand paging there.
const LOW_LAZY: usize = 0x7100_0000;
const MIB: usize = 1 << 20;

/// Each space here reserves the same high range above 4 GiB.
static SERIAL: Mutex<()> = Mutex::new(());

fn windowed() -> Arc<GuestSpace> {
    Arc::new(
        GuestSpace::with_config(GuestSpaceConfig {
            base: Some(BASE),
            size: 8 << 30,
            around_host: true,
            low_window: true,
            ..GuestSpaceConfig::default()
        })
        .expect("a space with a low window"),
    )
}

fn map(space: &GuestSpace, placement: Placement, commit: CommitPolicy) -> usize {
    space.map_anonymous(placement, MIB, Protection::ReadWrite, commit).expect("mapped")
}

/// Store `x1` at `x0` (low), load it into `x2`; an exclusive pair at `x0 + 8` stores `x3` (status
/// in `x6`); store `x1` at `x7` (high) and load it into `x8`; store `x1` at `x9` (low, lazy).
fn program(low: u64, high: u64, lazy: u64) -> Vec<u32> {
    let mut p = mov64(0, low);
    p.extend(mov64(7, high));
    p.extend(mov64(9, lazy));
    p.extend([
        str_imm(1, 0, 0),
        ldr_imm(2, 0, 0),
        add_imm(4, 0, 8),
        ldaxr(5, 4),
        stlxr(6, 3, 4),
        str_imm(1, 7, 0),
        ldr_imm(8, 7, 0),
        str_imm(1, 9, 0),
        ret(30),
    ]);
    p
}

fn run(options: DynarmicOptions) {
    let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let space = windowed();
    let window = space.low_window().expect("the window");
    let code = map(&space, Placement::Anywhere { align: space.page_size() }, CommitPolicy::Eager);
    let high = map(&space, Placement::Anywhere { align: space.page_size() }, CommitPolicy::Eager);
    let low = map(&space, Placement::Fixed(LOW_DATA), CommitPolicy::Eager);
    let lazy = map(&space, Placement::Fixed(LOW_LAZY), CommitPolicy::Lazy);
    assert!(code >= LOW_WINDOW_END && high >= LOW_WINDOW_END, "unaddressed mappings stay out of the window");

    let backend = DynarmicBackend::new(Arc::clone(&space), options).expect("a backend");
    let words = program(low as u64, high as u64, (lazy + 2 * MIB / 4) as u64);
    let ptr = space.ptr(code, words.len() * 4).expect("the code");
    // SAFETY: committed read-write memory this test owns, `words.len() * 4` bytes of it.
    unsafe { core::ptr::copy_nonoverlapping(words.as_ptr(), ptr.cast::<u32>(), words.len()) };
    space.protect(code, MIB, Protection::ReadExecute).expect("executable");
    backend.invalidate_code(GuestRange::new(code, words.len() * 4).expect("a range"));

    let mut cpu = match backend.create_thread_with_tls() {
        Ok(cpu) => cpu,
        Err(e) if cfg!(target_arch = "x86_64") => {
            // The low window is the arm64 backend's (patch 0030); x64 refuses it rather than
            // running a guest whose low 4 GiB would be read at the wrong host addresses.
            eprintln!("x64 refuses the low window, as it must: {e}");
            return;
        }
        Err(e) => panic!("a thread on a windowed space: {e}"),
    };
    assert!(cfg!(target_arch = "aarch64"), "x64 must refuse the low window, and made a thread");
    let mapping = cpu.memory_mapping();
    assert!(mapping.low_window, "{mapping:?}");
    assert_eq!(mapping.host_base, window.delta as u64, "the window's base is the direct path's");

    let sentinel = code + MIB - 4;
    cpu.set_return_sentinel(sentinel).expect("the sentinel");
    cpu.set_x(x(30), sentinel as u64);
    cpu.set_x(x(1), 0x1122_3344_5566_7788);
    cpu.set_x(x(3), 0xAABB);
    let before = cpu.slow_path_entries();
    let exit = cpu.run(code, RunLimit::Unlimited).expect("the program runs");
    assert_eq!(exit, ExitReason::Returned { pc: sentinel }, "{exit}");
    assert_eq!(cpu.slow_path_entries() - before, 0, "every access stayed on the direct path");
    assert_eq!(cpu.x(x(2)), 0x1122_3344_5566_7788, "the load at the low address");
    assert_eq!(cpu.x(x(6)), 0, "the exclusive store at the low address succeeded");
    assert_eq!(cpu.x(x(8)), 0x1122_3344_5566_7788, "the load at the high address");

    let at = |g: usize| {
        // SAFETY: a committed, 8-byte-readable guest address of this test's; `host_addr` is where
        // it lives.
        unsafe { (space.host_addr(g) as *const u64).read_unaligned() }
    };
    assert_ne!(space.host_addr(low), low, "the low data is based");
    assert_eq!(at(low), 0x1122_3344_5566_7788, "the store landed at W + g");
    assert_eq!(at(low + 8), 0xAABB, "the exclusive store landed at W + g + 8");
    assert_eq!(space.host_addr(high), high, "the high data is the identity");
    assert_eq!(at(high), 0x1122_3344_5566_7788);
    assert_eq!(at(lazy + 2 * MIB / 4), 0x1122_3344_5566_7788, "demand paging in the window");
}

#[test]
fn guest_accesses_below_4_gib_reach_the_window_on_the_direct_path() {
    run(DynarmicOptions::default());
}

#[test]
fn and_with_top_byte_ignore_as_the_linux_personality_runs() {
    run(DynarmicOptions { top_byte_ignore: true, ..DynarmicOptions::default() });
}
