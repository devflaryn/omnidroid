//! **The duplication census** (arm64, each guest thread's own translation cache; vendored patch 0106,
//! `OMNI_JIT_CENSUS=1`): how much of what a process's threads hold, and have emitted, is the same
//! blocks translated once per thread -- what one cache shared by the process would hold once. The
//! measurement the arm64 shared-cache plan's gate G0 decides by. Off by default; while it is off
//! nothing is joined and nothing counted.
//!
//! A block is its dynarmic location (guest PC and the FPCR bits that shape its code). One group per
//! guest process (the backend's), since a location means the same code only within one address
//! space. Each thread's live blocks are read at a safe point -- by the thread itself at the end of
//! a run, or under its lock while it is parked in a system call -- never by stopping it.
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use dynarmic_sys::{OD_CENSUS_EMITTED, OD_CENSUS_LIVE};

static ON: AtomicBool = AtomicBool::new(false);

/// Turn the census on for backends made from now on. False where it has nothing to count (x64:
/// one cache per process already).
pub fn enable() -> bool {
    if !cfg!(target_arch = "aarch64") {
        return false;
    }
    // SAFETY: `sink` may be called from any thread: it locks the group it is handed.
    unsafe { dynarmic_sys::od_set_census_sink(Some(sink)) };
    ON.store(true, Ordering::Relaxed);
    true
}

/// Whether [`enable`] has been called.
pub(crate) fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// What one look found for one guest process ([`super::DynarmicBackend::census_end`]).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Census {
    /// Threads (jits) whose live blocks came in: T.
    pub threads: u64,
    /// Live blocks summed over them: S.
    pub blocks: u64,
    /// Distinct locations among them: U.
    pub distinct_blocks: u64,
    /// Host code bytes of those blocks, summed.
    pub bytes: u64,
    /// Host code bytes of one copy of each distinct block.
    pub distinct_bytes: u64,
    /// Blocks emitted since the process's backend was made, summed over its threads.
    pub emitted_blocks: u64,
    /// Distinct locations among them.
    pub emitted_distinct_blocks: u64,
    /// Host code bytes emitted, summed over its threads.
    pub emitted_bytes: u64,
    /// Host code bytes emitted with each location counted once (its first emission's size).
    pub emitted_distinct_bytes: u64,
    /// Distinct bytes emitted per minute since the previous look (0 at the first).
    pub distinct_growth_per_min: u64,
    /// What the census's own maps hold, roughly: charged to the host process it measures.
    pub own_bytes: u64,
}

impl Census {
    /// `D_mem = S / U`: copies held of each live block, on average (0 with none).
    #[must_use]
    pub fn d_mem(&self) -> f64 {
        ratio(self.blocks, self.distinct_blocks)
    }

    /// `D_emit`: host code bytes emitted per distinct byte -- translation done more than once.
    #[must_use]
    pub fn d_emit(&self) -> f64 {
        ratio(self.emitted_bytes, self.emitted_distinct_bytes)
    }
}

fn ratio(a: u64, b: u64) -> f64 {
    if b == 0 {
        0.0
    } else {
        a as f64 / b as f64
    }
}

/// One guest process's counts: what its jits send ([`sink`]).
#[derive(Default)]
pub(crate) struct Group(parking_lot::Mutex<Counts>);

#[derive(Default)]
struct Counts {
    /// The look being gathered: each reporting jit's live blocks, by location.
    live: HashMap<u64, u32>,
    threads: u64,
    blocks: u64,
    bytes: u64,
    /// Every location emitted since the start, with its first emission's size.
    emitted: HashMap<u64, u32>,
    emitted_blocks: u64,
    emitted_bytes: u64,
    emitted_distinct_bytes: u64,
    /// Distinct bytes emitted at the previous look, and when.
    last: Option<(Instant, u64)>,
}

/// A map's allocation, roughly: hashbrown's buckets -- a power of two, `capacity()` 7/8 of them from
/// 8 up and one fewer below -- each a (key, value) slot, padded, and a control byte. MEASURED (M4,
/// 2026-10-20, a counting allocator, `HashMap<u64, u32>`, 1 to 2M keys): 8 bytes under at every
/// size; the 13 bytes per unit of capacity counted before read 1.5x low.
fn map_bytes<V>(m: &HashMap<u64, V>) -> u64 {
    let capacity = m.capacity();
    let buckets = match capacity {
        0 => 0,
        1..8 => capacity + 1,
        _ => capacity / 7 * 8,
    };
    (buckets * (std::mem::size_of::<(u64, V)>() + 1)) as u64
}

impl Group {
    /// Start a look: the previous one's live blocks are forgotten.
    pub(crate) fn begin(&self) {
        let mut c = self.0.lock();
        c.live = HashMap::new();
        c.threads = 0;
        c.blocks = 0;
        c.bytes = 0;
    }

    /// End a look: the live blocks reported since [`begin`](Self::begin), and the emission so far.
    pub(crate) fn end(&self) -> Census {
        let mut guard = self.0.lock();
        let c = &mut *guard;
        let now = Instant::now();
        let growth = c.last.map_or(0, |(at, bytes)| {
            let minutes = now.duration_since(at).as_secs_f64() / 60.0;
            if minutes > 0.0 {
                (c.emitted_distinct_bytes.saturating_sub(bytes) as f64 / minutes) as u64
            } else {
                0
            }
        });
        c.last = Some((now, c.emitted_distinct_bytes));
        let census = Census {
            threads: c.threads,
            blocks: c.blocks,
            distinct_blocks: c.live.len() as u64,
            bytes: c.bytes,
            distinct_bytes: c.live.values().map(|&b| u64::from(b)).sum(),
            emitted_blocks: c.emitted_blocks,
            emitted_distinct_blocks: c.emitted.len() as u64,
            emitted_bytes: c.emitted_bytes,
            emitted_distinct_bytes: c.emitted_distinct_bytes,
            distinct_growth_per_min: growth,
            own_bytes: map_bytes(&c.live) + map_bytes(&c.emitted),
        };
        // What came in is a look's alone: not kept between looks.
        c.live = HashMap::new();
        (c.threads, c.blocks, c.bytes) = (0, 0, 0);
        census
    }
}

/// The shim's census sink (`od_census_sink`).
unsafe extern "C" fn sink(group: *mut c_void, kind: u32, locations: *const u64, host_bytes: *const u32, count: u64) {
    if group.is_null() || count == 0 {
        return;
    }
    // SAFETY: `group` is a `Group` of a backend, which outlives every jit joined to it (it is freed
    // with the backend's shared state, after its contexts); the shim passes `count` elements in each
    // array.
    let (group, locations, host_bytes) = unsafe {
        (
            &*group.cast::<Group>(),
            std::slice::from_raw_parts(locations, count as usize),
            std::slice::from_raw_parts(host_bytes, count as usize),
        )
    };
    let mut guard = group.0.lock();
    let c = &mut *guard;
    match kind {
        OD_CENSUS_LIVE => {
            c.threads += 1;
            c.blocks += count;
            for (&location, &bytes) in locations.iter().zip(host_bytes) {
                c.bytes += u64::from(bytes);
                c.live.entry(location).or_insert(bytes);
            }
        }
        OD_CENSUS_EMITTED => {
            for (&location, &bytes) in locations.iter().zip(host_bytes) {
                c.emitted_blocks += 1;
                c.emitted_bytes += u64::from(bytes);
                if let std::collections::hash_map::Entry::Vacant(e) = c.emitted.entry(location) {
                    e.insert(bytes);
                    c.emitted_distinct_bytes += u64::from(bytes);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`map_bytes`] rests on hashbrown's shape -- `capacity()` 7/8 of a power of two of buckets, one
    /// fewer below 8 -- which std does not promise: a change to it would leave the census's own
    /// cost wrong in silence.
    #[test]
    fn a_map_s_bytes_are_its_buckets() {
        let m: HashMap<u64, u32> = HashMap::with_capacity(1000);
        assert_eq!((m.capacity(), map_bytes(&m)), (1792, 2048 * 17));
        let small: HashMap<u64, u32> = HashMap::with_capacity(3);
        assert_eq!((small.capacity(), map_bytes(&small)), (3, 4 * 17));
        assert_eq!(map_bytes(&HashMap::<u64, u32>::new()), 0);
    }
}
