//! The per-thread cache behind `admit` and `region_at`: that it is taken, that it answers exactly
//! what the locked walk answers, and that **every kind of change to the map** -- protect, unmap,
//! map over an unmapped hole, lazy commit, idle marks, reclaim -- is seen by a thread that cached
//! the entry before the change, including when another thread made it.
//!
//! The oracle is never a second implementation of the rules (`VERIFICATION.md` entry 7). It is the
//! *same* `admit`, run on a **thread that has never cached anything**: a fresh thread's first
//! lookup always takes the lock, so what it returns is by construction what the locked walk says.
//!
//! Portable: every host whose `omni-platform` memory backend is implemented runs it, and nothing
//! here names a host.
//!
//! The benchmark at the bottom is `#[ignore]`d:
//!
//! ```text
//! cargo test -p omni-mem --release --test admit_cache -- --ignored --nocapture
//! ```

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use common::{TempFile, KIB, MIB};
use omni_mem::access::cache_answers;
use omni_mem::{
    admit, Admitted, Backing, CommitPolicy, FaultAccess, GuestSpace, GuestSpaceConfig,
    MapExecutability, Placement, Protection, Refusal,
};

fn space(size: usize) -> GuestSpace {
    GuestSpace::with_config(GuestSpaceConfig { size, ..GuestSpaceConfig::default() })
        .expect("reserve a guest address space")
}

/// Run `body` on a new thread, which has never cached anything, and wait for it.
fn elsewhere<T: Send>(body: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(body).join().expect("the other thread panicked"))
}

/// `admit` on a thread with an empty cache: the locked walk, and nothing else.
fn locked(
    space: &GuestSpace,
    address: usize,
    len: usize,
    access: FaultAccess,
) -> Result<Admitted, Refusal> {
    elsewhere(|| admit(space, address, len, access))
}

/// How many answers this thread's `admit` has given from its cache.
fn fast_admits() -> u64 {
    cache_answers().0
}

/// How many answers this thread's `region_at` has given from its cache.
fn fast_regions() -> u64 {
    cache_answers().1
}

fn eager(space: &GuestSpace, len: usize, protection: Protection) -> usize {
    space
        .map_anonymous(
            Placement::Anywhere { align: space.commit_granule() },
            len,
            protection,
            CommitPolicy::Eager,
        )
        .expect("an eager mapping")
}

fn lazy(space: &GuestSpace, len: usize) -> usize {
    space
        .map_anonymous(
            Placement::Anywhere { align: space.commit_granule() },
            len,
            Protection::ReadWrite,
            CommitPolicy::Lazy,
        )
        .expect("a lazy mapping")
}

/// Warm this thread's cache for `address` and prove the warm answer came from it.
fn warm(space: &GuestSpace, address: usize, access: FaultAccess) -> Admitted {
    let first = admit(space, address, 8, access).expect("the warming access is admitted");
    let before = fast_admits();
    let second = admit(space, address, 8, access).expect("the warmed access is admitted");
    assert_eq!(fast_admits(), before + 1, "the second admit at {address:#x} was not a cache hit");
    assert_eq!(first, second, "a cache hit answered differently from the walk it followed");
    second
}

// ---- it is taken, and only where it may be -------------------------------------------------------

/// The fast path is actually taken for a committed anonymous entry and for a file view, and
/// `region_at` answers from the cache for the first and not the second.
///
/// A fast path that silently stopped being taken would pass every other test in this file, and
/// cost the 60x this cache exists to recover. So it is asserted, not assumed.
#[test]
fn the_fast_paths_are_taken_where_they_may_be_and_not_elsewhere() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let anon = eager(&space, 256 * KIB, Protection::ReadWrite);
    warm(&space, anon + 8, FaultAccess::Write);

    let before = fast_regions();
    let region = space.region_at(anon + 8).expect("mapped");
    assert_eq!(fast_regions(), before + 1, "region_at of a cached anonymous entry took the lock");
    assert_eq!(Some(region), elsewhere(|| space.region_at(anon + 8)));

    let file = TempFile::new("view.bin", MIB, page);
    let backing = Backing::open(file.path(), MapExecutability::NonExecutable).expect("open");
    let anywhere = Placement::Anywhere { align: space.commit_granule() };
    let view = space
        .map_file(&backing, 0, anywhere, 256 * KIB, Protection::Read)
        .expect("map the file");
    let admitted = warm(&space, view + 8, FaultAccess::Read);
    assert!(!admitted.anonymous && !admitted.fully_committed && admitted.committed == 0);

    let before = fast_regions();
    let region = space.region_at(view + 8).expect("mapped");
    assert_eq!(
        fast_regions(),
        before,
        "region_at answered a file region from the cache, which would clone the file's name"
    );
    assert_eq!(Some(region), elsewhere(|| space.region_at(view + 8)));

    // Refusals are never answered from the cache: a write to the read-only view walks.
    let before = fast_admits();
    assert_eq!(admit(&space, view + 8, 8, FaultAccess::Write), Err(Refusal::Protection));
    assert_eq!(fast_admits(), before);
}

/// Over a layout with every shape the walk distinguishes -- adjacent mappings of different
/// protection, free space, a partially committed lazy mapping, an inaccessible mapping, a file
/// view -- a warm thread answers exactly what a fresh one does, access for access.
#[test]
fn a_warm_thread_answers_exactly_what_a_fresh_one_does() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let page = space.page_size();

    let base = space.base() + 4 * MIB;
    let at = |offset: usize| base + offset;
    let fixed = |offset: usize, len: usize, protection: Protection, commit: CommitPolicy| {
        space
            .map_anonymous(Placement::Fixed(at(offset)), len, protection, commit)
            .expect("fixed mapping")
    };
    // [0, 4g) eager RW, then [4g, 5g) eager R butted against it: a seam between two mappings.
    fixed(0, 4 * granule, Protection::ReadWrite, CommitPolicy::Eager);
    fixed(4 * granule, granule, Protection::Read, CommitPolicy::Eager);
    // [5g, 6g) free. [6g, 14g) lazy RW with granules 0, 1 and 3 committed.
    let lazy_at = fixed(6 * granule, 8 * granule, Protection::ReadWrite, CommitPolicy::Lazy);
    for g in [0, 1, 3] {
        space.ensure_committed(lazy_at + g * granule, 1).expect("commit a granule");
    }
    // [14g, 15g) inaccessible.
    fixed(14 * granule, granule, Protection::None, CommitPolicy::Lazy);
    // [16g, 20g) a read-only file view.
    let file = TempFile::new("layout.bin", MIB, page);
    let backing = Backing::open(file.path(), MapExecutability::NonExecutable).expect("open");
    space
        .map_file(&backing, 0, Placement::Fixed(at(16 * granule)), 4 * granule, Protection::Read)
        .expect("map the file");

    let mut addresses = Vec::new();
    for g in 0..21 {
        for delta in [0isize, 8, -8, -1, 16, page as isize, -(page as isize)] {
            let offset = (g * granule) as isize + delta;
            if offset >= 0 {
                addresses.push(at(offset as usize));
            }
        }
    }
    let lengths = [0usize, 1, 8, 16, page, granule + 8];
    let accesses = [FaultAccess::Read, FaultAccess::Write, FaultAccess::Execute];

    let mut compared = 0;
    let mut from_cache = 0;
    for &address in &addresses {
        for &len in &lengths {
            for access in accesses {
                // The first fresh call may commit (rule 4); after it, the state is settled and
                // everything below must agree.
                let _ = locked(&space, address, len, access);
                let warm_first = admit(&space, address, len, access);
                let before = fast_admits();
                let warm_second = admit(&space, address, len, access);
                from_cache += fast_admits() - before;
                let fresh = locked(&space, address, len, access);
                assert_eq!(
                    warm_second, fresh,
                    "{access:?} of {len} at base+{:#x}: the warm thread and a fresh one disagree",
                    address - base
                );
                assert_eq!(
                    warm_first,
                    warm_second,
                    "{access:?} of {len} at base+{:#x}",
                    address - base
                );
                compared += 1;
            }
        }
    }
    // Both halves were exercised: answers from the cache, and answers that had to walk.
    assert!(from_cache > 100, "only {from_cache} of {compared} answers came from the cache");
    assert!(from_cache < compared, "every answer came from the cache, so nothing walked");
}

// ---- spanning, partial, and rule 4 go through the walk exactly as before ----------------------

#[test]
fn spanning_and_partially_committed_accesses_still_walk() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = lazy(&space, 4 * granule);
    space.ensure_committed(base, 1).expect("commit granule 0");
    space.ensure_committed(base + granule, 1).expect("commit granule 1");

    warm(&space, base + granule - 16, FaultAccess::Read);
    // Straddling two committed granules of one mapping: admitted, reported to the end of granule 1.
    let before = fast_admits();
    let straddle = admit(&space, base + granule - 8, 16, FaultAccess::Read).expect("straddle");
    assert_eq!(fast_admits(), before, "a straddling access was answered from one entry");
    assert_eq!(straddle.end, base + 2 * granule);
    assert!(straddle.fully_committed);
    assert_eq!(straddle.committed, 0);

    // Straddling from granule 1 into the uncommitted granule 2: the walk commits granule 2.
    warm(&space, base + 2 * granule - 16, FaultAccess::Write);
    let owed = admit(&space, base + 2 * granule - 8, 16, FaultAccess::Write).expect("owed");
    assert_eq!(owed.committed, granule, "the straddled uncommitted granule was not committed");
    assert!(!owed.fully_committed);

    // Off the end of an eager mapping into free space: refused, however warm the thread is.
    let solo = space
        .map_anonymous(
            Placement::Fixed(space.base() + 32 * MIB),
            granule,
            Protection::ReadWrite,
            CommitPolicy::Eager,
        )
        .expect("a mapping with free space after it");
    assert_eq!(space.region_at(solo + granule), None, "the precondition: free space follows");
    warm(&space, solo + granule - 16, FaultAccess::Read);
    assert_eq!(
        admit(&space, solo + granule - 8, 16, FaultAccess::Read),
        Err(Refusal::NotMapped),
        "an access running into free space was admitted from the cached entry"
    );
    // The last byte, exactly: still inside, still from the cache.
    let before = fast_admits();
    assert!(admit(&space, solo + granule - 8, 8, FaultAccess::Read).is_ok());
    assert_eq!(fast_admits(), before + 1);
    let len_past = admit(&space, solo + granule - 8, usize::MAX, FaultAccess::Read);
    assert_eq!(len_past, Err(Refusal::NotMapped), "a length that overflows was admitted");
}

/// An anonymous entry the thread has looked at but that is still owed a commit is committed by
/// `admit`, not answered from the cache with nothing committed.
#[test]
fn a_cached_entry_still_owed_a_commit_is_committed_by_admit() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = lazy(&space, 4 * granule);

    // `region_at` caches the uncommitted entry; a second call proves it is cached.
    let region = space.region_at(base + 2 * granule).expect("mapped");
    assert_eq!(region.committed, 0);
    let before = fast_regions();
    assert_eq!(space.region_at(base + 2 * granule), Some(region));
    assert_eq!(fast_regions(), before + 1);

    let admitted = admit(&space, base + 2 * granule + 8, 8, FaultAccess::Write).expect("admit");
    assert_eq!(admitted.committed, granule, "rule 4 was skipped for a cached uncommitted entry");
    // SAFETY: admitted for writing, and the granule was committed just now.
    unsafe { std::ptr::write_volatile((base + 2 * granule + 8) as *mut u64, 0x5eed) };
}

// ---- every kind of change is seen, from this thread and from another -------------------------

#[test]
fn a_protect_is_seen_by_a_thread_that_cached_the_old_protection() {
    let space = space(64 * MIB);
    let page = space.page_size();
    let base = eager(&space, 256 * KIB, Protection::ReadWrite);

    warm(&space, base + 8, FaultAccess::Write);
    elsewhere(|| space.protect(base, page, Protection::Read).expect("protect"));
    assert_eq!(
        admit(&space, base + 8, 8, FaultAccess::Write),
        Err(Refusal::Protection),
        "a write was admitted to a page another thread made read-only"
    );
    let read = admit(&space, base + 8, 8, FaultAccess::Read).expect("still readable");
    assert_eq!(read.end, base + page, "the protect split the entry; the old extent was reported");
    assert_eq!(space.region_at(base + 8).map(|r| r.protection), Some(Protection::Read));

    // And on the same thread.
    warm(&space, base + page + 8, FaultAccess::Write);
    space.protect(base + page, page, Protection::Read).expect("protect");
    assert_eq!(admit(&space, base + page + 8, 8, FaultAccess::Write), Err(Refusal::Protection));
}

#[test]
fn an_unmap_is_seen_by_a_thread_that_cached_the_mapping() {
    let space = space(64 * MIB);
    let base = eager(&space, 256 * KIB, Protection::ReadWrite);

    warm(&space, base + 64 * KIB + 8, FaultAccess::Read);
    warm(&space, base + 8, FaultAccess::Read);
    elsewhere(|| space.unmap(base + 64 * KIB, 64 * KIB).expect("unmap"));
    assert_eq!(
        admit(&space, base + 64 * KIB + 8, 8, FaultAccess::Read),
        Err(Refusal::NotMapped),
        "an address another thread unmapped was admitted"
    );
    assert_eq!(space.region_at(base + 64 * KIB + 8), None);
    let head = admit(&space, base + 8, 8, FaultAccess::Read).expect("the head survives");
    assert_eq!(head.end, base + 64 * KIB, "the survivor was reported with the old extent");

    // Mapped again at the same address, read-only, by another thread: a write is refused.
    elsewhere(|| {
        space
            .map_anonymous(
                Placement::Fixed(base + 64 * KIB),
                64 * KIB,
                Protection::Read,
                CommitPolicy::Eager,
            )
            .expect("map again")
    });
    assert_eq!(
        admit(&space, base + 64 * KIB + 8, 8, FaultAccess::Write),
        Err(Refusal::Protection)
    );
    assert!(admit(&space, base + 64 * KIB + 8, 8, FaultAccess::Read).is_ok());
}

#[test]
fn a_commit_is_seen_by_a_thread_that_cached_the_uncommitted_entry() {
    let space = space(64 * MIB);
    let granule = space.commit_granule();
    let base = lazy(&space, 4 * granule);

    let before = space.region_at(base).expect("mapped");
    assert_eq!((before.committed, before.len), (0, 4 * granule));
    assert_eq!(space.region_at(base), Some(before), "cached");

    elsewhere(|| space.ensure_committed(base, 1).expect("commit"));
    let after = space.region_at(base).expect("mapped");
    assert_eq!(
        (after.committed, after.len),
        (granule, granule),
        "region_at answered from an entry cached before another thread committed it"
    );
    let admitted = admit(&space, base + 8, 8, FaultAccess::Write).expect("admitted");
    assert_eq!(admitted.committed, 0, "already committed");
    assert!(admitted.fully_committed);
    assert_eq!(admitted.end, base + granule);
}

#[test]
fn a_reclaim_is_seen_by_a_thread_that_cached_the_committed_entry() {
    let space = space(64 * MIB);
    let base = eager(&space, 256 * KIB, Protection::ReadWrite);
    // SAFETY: an eager read-write mapping, committed in full.
    unsafe { std::ptr::write_volatile((base + 8) as *mut u64, 0x1234) };

    // Marked idle first, and the cache warmed again *after* the mark: an idle entry is still
    // committed, so it is served from the cache -- and it is the reclaim alone that has to say
    // otherwise. Warming only before the mark would let the mark's own bump hide a reclaim that
    // forgot its.
    elsewhere(|| space.advise_idle(base, 256 * KIB).expect("advise idle"));
    let warmed = warm(&space, base + 8, FaultAccess::Write);
    assert!(warmed.fully_committed);
    elsewhere(|| {
        let reclaimed = space.reclaim_idle().expect("reclaim");
        assert_eq!(reclaimed.bytes, 256 * KIB);
    });
    let again = admit(&space, base + 8, 8, FaultAccess::Write).expect("admitted");
    assert!(
        again.committed > 0,
        "admit answered `already committed` for memory another thread decommitted: {again:?}"
    );
    // SAFETY: admit just committed the granule for writing. Written only after the assertion, so
    // a stale answer fails the test instead of faulting it.
    unsafe {
        std::ptr::write_volatile((base + 8) as *mut u64, 0x5678);
        assert_eq!(std::ptr::read_volatile((base + 8) as *const u64), 0x5678);
    }
}

/// `advise_idle` alone changes nothing a guest can see, but it splits the entry, and the extent
/// `admit` reports is the entry's.
#[test]
fn an_idle_mark_is_seen_by_a_thread_that_cached_the_whole_entry() {
    let space = space(64 * MIB);
    let base = eager(&space, 256 * KIB, Protection::ReadWrite);
    let warmed = warm(&space, base + 8, FaultAccess::Read);
    assert_eq!(warmed.end, base + 256 * KIB);

    elsewhere(|| space.advise_idle(base + 64 * KIB, 64 * KIB).expect("advise idle"));
    let admitted = admit(&space, base + 8, 8, FaultAccess::Read).expect("admitted");
    assert_eq!(Ok(admitted), locked(&space, base + 8, 8, FaultAccess::Read));
    assert_eq!(admitted.end, base + 64 * KIB, "the extent from before the idle mark was reported");
}

/// Many threads with warm caches, one thread changing the protection under them over and over:
/// once it stops, every one of them sees where it stopped.
#[test]
fn every_thread_sees_the_last_of_a_storm_of_changes() {
    const READERS: usize = 8;
    let space = space(64 * MIB);
    let page = space.page_size();
    let base = eager(&space, 256 * KIB, Protection::ReadWrite);
    let stop = AtomicBool::new(false);
    let start = Barrier::new(READERS + 1);
    let settled = Barrier::new(READERS + 1);

    std::thread::scope(|scope| {
        for lane in 0..READERS {
            let (space, stop, start, settled) = (&space, &stop, &start, &settled);
            scope.spawn(move || {
                let address = base + (lane % 4) * page + 8;
                start.wait();
                while !stop.load(Ordering::Acquire) {
                    let _ = std::hint::black_box(admit(space, address, 8, FaultAccess::Write));
                }
                settled.wait();
                // The writer finished with every page read-only.
                assert_eq!(
                    admit(space, address, 8, FaultAccess::Write),
                    Err(Refusal::Protection),
                    "lane {lane} still admits a write after the last protect"
                );
            });
        }
        start.wait();
        for round in 0..400 {
            let protection = if round % 2 == 0 { Protection::Read } else { Protection::ReadWrite };
            space.protect(base, 4 * page, protection).expect("protect");
        }
        space.protect(base, 4 * page, Protection::Read).expect("protect");
        stop.store(true, Ordering::Release);
        settled.wait();
    });
}

// ---- the measurement --------------------------------------------------------------------------

fn hammer(space: &GuestSpace, base: usize, lane: usize, iterations: usize) -> Duration {
    let started = Instant::now();
    for i in 0..iterations {
        let address = base + ((lane * 4096 + (i % 512) * 8) % MIB);
        let access = if i & 1 == 0 { FaultAccess::Read } else { FaultAccess::Write };
        let admitted = admit(std::hint::black_box(space), address, 8, access);
        std::hint::black_box(admitted).expect("admitted");
    }
    started.elapsed()
}

/// What one `admit` costs on one thread, and with eight threads hammering one mapping -- the shape
/// of the in-world profile, where every import handler's pointer check went through one lock.
///
/// MEASURED on the Windows development machine (24 logical processors), release, median of 5:
///
/// | | 1 thread | 8 threads, per admit per thread | 8 threads, aggregate |
/// |---|---|---|---|
/// | the lock, before the cache | 28-33 ns | 1,770-1,915 ns | 4.2-4.5 M/s |
/// | the cache | 10.9-11.1 ns | 12.4-12.9 ns | 467-523 M/s |
///
/// So one thread pays a third of what it did, and eight threads stop queueing on each other: the
/// per-admit cost under contention falls by about 140x, to within 2 ns of the uncontended cost,
/// which is what "no lock and no shared write" should look like.
#[test]
#[ignore = "a measurement: cargo test -p omni-mem --release --test admit_cache -- --ignored --nocapture"]
fn the_cost_of_admit_single_threaded_and_with_eight_threads() {
    let space = Arc::new(space(64 * MIB));
    let base = eager(&space, MIB, Protection::ReadWrite);
    const SINGLE: usize = 4_000_000;
    const PER_THREAD: usize = 1_000_000;
    const THREADS: usize = 8;
    const RUNS: usize = 5;

    hammer(&space, base, 0, SINGLE / 10);
    let mut single: Vec<f64> = (0..RUNS)
        .map(|_| hammer(&space, base, 0, SINGLE).as_secs_f64() * 1e9 / SINGLE as f64)
        .collect();
    single.sort_by(f64::total_cmp);

    let mut contended: Vec<(f64, f64)> = (0..RUNS)
        .map(|_| {
            let barrier = Arc::new(Barrier::new(THREADS));
            let started = Instant::now();
            let per: Vec<Duration> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..THREADS)
                    .map(|lane| {
                        let space = &space;
                        let barrier = Arc::clone(&barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            hammer(space, base, lane, PER_THREAD)
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("join")).collect()
            });
            let wall = started.elapsed().as_secs_f64();
            let mean_per_thread =
                per.iter().map(Duration::as_secs_f64).sum::<f64>() / THREADS as f64;
            (
                mean_per_thread * 1e9 / PER_THREAD as f64,
                (THREADS * PER_THREAD) as f64 / wall / 1e6,
            )
        })
        .collect();
    contended.sort_by(|a, b| a.0.total_cmp(&b.0));

    println!(
        "admit, 1 thread: median {:.1} ns/admit (min {:.1}, max {:.1}) over {RUNS} runs of {SINGLE}",
        single[RUNS / 2],
        single[0],
        single[RUNS - 1]
    );
    println!(
        "admit, {THREADS} threads on one mapping: median {:.1} ns/admit per thread, {:.1} M admits/s \
         in aggregate (min {:.1}, max {:.1} ns) over {RUNS} runs of {PER_THREAD} per thread",
        contended[RUNS / 2].0,
        contended[RUNS / 2].1,
        contended[0].0,
        contended[RUNS - 1].0
    );
}
