//! **The engine's heap's OS calls, from many threads at once**, against one guest address space
//! with the demand pager installed: every block has one owner, and no call another thread makes
//! changes a byte of it.
//!
//! DECODED (the modified 2.739.691 build's `libroblox.so`, a different binary from the stock
//! fixture's): the engine's allocator is mimalloc v3, compiled into the
//! library, and these are the only calls it makes to the OS:
//!
//! * `mmap(NULL, size, PROT_READ|PROT_WRITE or PROT_NONE,
//!   MAP_PRIVATE|MAP_ANONYMOUS[|MAP_NORESERVE], -1, 0)` (`unix_mmap`, `0x1db9770`, through
//!   `0x62cb62c`, which names the range with `prctl`);
//! * when that is not aligned to what it asked for, the over-allocation fallback
//!   (`0x1db93b4`): `mmap(size + alignment)`, then `munmap` of the head before the aligned start
//!   and of the tail after `aligned + size` -- the **trim** (`0x1db9480`/`0x1db94e0`/`0x1db9500`);
//! * commit, `mprotect(PROT_READ|PROT_WRITE)` over the range rounded **out** to pages
//!   (`0x62ca18c`);
//! * purge, `madvise(MADV_DONTNEED)` over the range rounded **in** to pages, after which it treats
//!   the range as committed and touches it again with no further call (`0x229de64`), or with
//!   `MIMALLOC_PURGE_DECOMMITS=0`, `madvise(MADV_FREE)` falling back to `MADV_DONTNEED`
//!   (`0x229d98c`);
//! * `munmap` of whole or trimmed ranges (`0x1dbb060`).
//!
//! Each thread here plays that allocator for its own blocks, with a pattern only it writes, while
//! the others do the same: an overlap between two owners, a trim or a purge reaching a byte past
//! its range, or a protection change losing contents, shows up as a wrong byte in somebody's
//! block. The pager commits every first touch, as it does under guest code. It runs at the host's
//! page and, on a 4 KiB host, at 16 KiB through `GuestSpace::with_page_size` as well.

mod common;

use std::sync::{Arc, Barrier};

use common::{KIB, MIB};
use omni_mem::{
    CommitPolicy, DemandPager, GuestSpace, GuestSpaceConfig, Placement, Protection, SMALL_PAGE,
};

/// A small deterministic generator, so a failure names a seed that reproduces it.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// The byte `owner`'s block `serial` holds at `offset`. Never zero, so a purged byte cannot pass.
fn byte_of(owner: usize, serial: u64, offset: usize) -> u8 {
    let mixed = (owner as u64 + 1)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(serial.wrapping_mul(0x85EB_CA6B))
        .wrapping_add((offset / 8) as u64);
    (mixed % 255) as u8 + 1
}

struct Block {
    at: usize,
    len: usize,
    serial: u64,
}

fn fill(owner: usize, block: &Block) {
    for offset in (0..block.len).step_by(8) {
        let value = u64::from_ne_bytes([byte_of(owner, block.serial, offset); 8]);
        // SAFETY: the block is this thread's, mapped read-write; the pager commits a first touch.
        unsafe { std::ptr::write_volatile((block.at + offset) as *mut u64, value) };
    }
}

fn check(owner: usize, block: &Block, zero: Option<(usize, usize)>, what: &str) {
    for offset in (0..block.len).step_by(8) {
        let address = block.at + offset;
        let zeroed = zero.is_some_and(|(from, n)| (from..from + n).contains(&address));
        let byte = if zeroed { 0 } else { byte_of(owner, block.serial, offset) };
        let expected = u64::from_ne_bytes([byte; 8]);
        // SAFETY: as in `fill`; the block is readable whenever this is called.
        let found = unsafe { std::ptr::read_volatile(address as *const u64) };
        assert_eq!(
            found, expected,
            "thread {owner}, {what}: block {:#x}+{:#x} (serial {}) has {found:#x} at +{offset:#x}",
            block.at, block.len, block.serial
        );
    }
}

/// One thread's allocator: map (aligned, by over-allocating and trimming when needed), fill,
/// purge part of a block, drop a block to no access and back, unmap -- checking its own bytes after
/// every step.
fn play(space: &GuestSpace, owner: usize, rounds: usize) {
    let page = space.page_size();
    let mut rng = Rng(0x5EED_0000 + owner as u64 * 7919);
    let mut blocks: Vec<Block> = Vec::new();
    let mut serial = 0u64;
    for _ in 0..rounds {
        match rng.below(10) {
            // Map: mimalloc asks for alignment and trims when the answer is not aligned.
            0..=3 if blocks.len() < 24 => {
                let len = (1 + rng.below(16) as usize) * page;
                let align = [page, 64 * KIB, 256 * KIB][rng.below(3) as usize].max(page);
                let over = len + align;
                let anywhere = Placement::Anywhere { align: page };
                let at = space
                    .map_anonymous(anywhere, over, Protection::ReadWrite, CommitPolicy::Lazy)
                    .expect("map");
                let aligned = at.next_multiple_of(align);
                let head = aligned - at;
                let tail = over - head - len;
                if head > 0 {
                    space.unmap(at, head).expect("trim the head");
                }
                if tail > 0 {
                    space.unmap(aligned + len, tail).expect("trim the tail");
                }
                serial += 1;
                let block = Block { at: aligned, len, serial };
                fill(owner, &block);
                blocks.push(block);
            }
            // Purge part of a block (MADV_DONTNEED): zero there, nothing else changes; then reuse
            // it by writing, with no call in between, as the allocator does.
            4 | 5 if !blocks.is_empty() => {
                let block = &blocks[rng.below(blocks.len() as u64) as usize];
                let pages = block.len / SMALL_PAGE;
                let first = rng.below(pages as u64) as usize;
                let n = 1 + rng.below((pages - first) as u64) as usize;
                let zero = (block.at + first * SMALL_PAGE, n * SMALL_PAGE);
                space.discard(zero.0, zero.1).expect("purge");
                check(owner, block, Some(zero), "after a purge");
                fill(owner, block);
            }
            // No access and back: the contents survive, as on Linux.
            6 if !blocks.is_empty() => {
                let block = &blocks[rng.below(blocks.len() as u64) as usize];
                space.protect(block.at, block.len, Protection::None).expect("protect none");
                space.protect(block.at, block.len, Protection::ReadWrite).expect("protect back");
                check(owner, block, None, "after PROT_NONE and back");
            }
            // Unmap, whole.
            7 if !blocks.is_empty() => {
                let block = blocks.swap_remove(rng.below(blocks.len() as u64) as usize);
                check(owner, &block, None, "before its unmap");
                space.unmap(block.at, block.len).expect("unmap");
            }
            _ => {
                if let Some(block) = blocks.get(rng.below(blocks.len().max(1) as u64) as usize) {
                    check(owner, block, None, "at rest");
                }
            }
        }
    }
    for block in blocks.drain(..) {
        check(owner, &block, None, "at the end");
        space.unmap(block.at, block.len).expect("unmap at the end");
    }
}

fn run(space: GuestSpace, threads: usize, rounds: usize) {
    let space = Arc::new(space);
    let pager = DemandPager::install(Arc::clone(&space)).expect("install the demand pager");
    let start = Arc::new(Barrier::new(threads));
    let workers: Vec<_> = (0..threads)
        .map(|owner| {
            let space = Arc::clone(&space);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                play(&space, owner, rounds);
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("a thread found a byte it did not write");
    }
    assert!(pager.stats_are_consistent(), "{:?}", pager.stats());
    assert!(pager.stats().resolved > 0, "the pager served nothing: nothing was touched lazily");
    drop(pager);
}

fn config() -> GuestSpaceConfig {
    GuestSpaceConfig { size: 512 * MIB, ..GuestSpaceConfig::default() }
}

#[test]
fn the_heaps_os_calls_from_eight_threads_leave_every_block_its_owners() {
    run(GuestSpace::with_config(config()).expect("reserve"), 8, 3000);
}

/// The same at 16 KiB pages, on a host whose own are smaller: the Mac's page, with its sub-page
/// purges zeroed in place, run everywhere.
#[test]
fn the_heaps_os_calls_at_16k_pages_leave_every_block_its_owners() {
    let host = GuestSpace::with_config(GuestSpaceConfig { size: 64 * MIB, ..Default::default() })
        .expect("reserve")
        .page_size();
    if host > 16 * KIB {
        return;
    }
    run(GuestSpace::with_page_size(config(), 16 * KIB).expect("reserve"), 8, 3000);
}
