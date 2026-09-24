//! **An invalidation costs what it covers of translated code, not what it covers of address space**
//! (patch 0015).
//!
//! `omni-android` hands every guest `mmap`, `munmap`, `mprotect` and `MADV_DONTNEED` range to every
//! guest thread's jit as a code invalidation, because any of them could have held translated code.
//! Almost none did: they are heap. Patch 0011's page index answered such a range with one hash
//! lookup per 4 KiB page, on every thread -- MEASURED in a game on this Mac (`sample`, 5 s): two
//! guest threads spent 64-69% of their time in `A64AddressSpace::InvalidateCacheRanges`, 4,018
//! samples in all, more than a core. Patch 0015 walks only the 2 MiB chunks that hold translated
//! pages. `od_invalidation_page_probes` counts the page lookups, over every jit in the process.
#![cfg(target_arch = "aarch64")]

mod harness;

use dynarmic_sys::od_jit_invalidate_range;
use harness::{a64, Vm, VmOptions, CODE_BASE, HALT_DONE};

extern "C" {
    fn od_invalidation_page_probes() -> u64;
}

const PAGE: u64 = 4096;
const WORDS_PER_PAGE: usize = (PAGE / 4) as usize;
/// Code pages, each holding one block (`B` to the next page).
const CODE_PAGES: usize = 300;

/// One block per 4 KiB guest page: the page's first word branches to the next page's; the last
/// page's first word is the `SVC`.
fn one_block_per_page() -> Vec<u32> {
    let mut code = vec![a64::NOP; CODE_PAGES * WORDS_PER_PAGE + 1];
    for page in 0..CODE_PAGES {
        code[page * WORDS_PER_PAGE] = a64::b(WORDS_PER_PAGE as i32);
    }
    code[CODE_PAGES * WORDS_PER_PAGE] = a64::svc(0);
    code
}

fn run(vm: &Vm) {
    vm.start(u64::MAX);
    assert_eq!(vm.run_to_completion(16) & HALT_DONE, HALT_DONE, "the chain reached its SVC");
}

/// Probes an invalidation of `[addr, addr + len)` made, applied by running the jit again.
fn probes_for(vm: &Vm, addr: u64, len: u64) -> u64 {
    // SAFETY: no arguments; reads an atomic counter. Tests in this binary run on one thread.
    let before = unsafe { od_invalidation_page_probes() };
    // SAFETY: a live jit that is not running.
    unsafe { od_jit_invalidate_range(vm.raw(), addr, len) };
    run(vm);
    // SAFETY: as above.
    let after = unsafe { od_invalidation_page_probes() };
    after - before
}

#[test]
fn a_range_with_no_translated_code_in_it_is_not_walked_page_by_page() {
    let vm = Vm::new(one_block_per_page(), VmOptions { code_cache_size: 16 << 20, ..VmOptions::default() });
    run(&vm);

    // The instrument, shown to see something: a range over two translated pages probes them.
    let inside = probes_for(&vm, CODE_BASE + 5 * PAGE, 2 * PAGE);
    assert!(inside >= 2, "an invalidation over two translated pages probed {inside} pages");

    // 256 pages of address space with no code in it, 256 MiB above the code: fewer pages than the
    // index holds, so patch 0011 walked every one of them.
    let data = probes_for(&vm, CODE_BASE + (256 << 20), 256 * PAGE);
    println!("probes: {inside} for two code pages, {data} for 256 pages of data");
    assert!(data <= 1, "an invalidation of 256 pages with no code in them probed {data} pages");
}
