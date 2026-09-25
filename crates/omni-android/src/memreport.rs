//! **`OMNI_MEM_REPORT`: whose this process's memory is, owner by owner.**
//!
//! `OMNI_PERF`'s line says the process holds, say, 3.9 GiB private and 3.5 GiB resident. This says
//! which of them: the engine's heap, the loaded library, the guest's thread stacks, the graphics
//! layer's shadows, the translator's code caches, host thread stacks, the heaps this runtime and
//! the drivers share, DLL images and mapped files -- each with what it has **committed** (the
//! private commit charge an instance costs the machine's commit limit) and what of it is
//! **resident** (in RAM now), split into private and shareable (a page another process mapping
//! the same file could be sharing).
//!
//! | switch | what it prints |
//! |---|---|
//! | `OMNI_MEM_REPORT=1` | a `MEMREPORT` table at +60 s and +180 s, then every 300 s |
//! | `OMNI_MEM_REPORT=<s>[,<s>...]` | one at each listed second of the process |
//! | `OMNI_MEM_REPORT=every:<s>` | one every `<s>` seconds |
//!
//! # Where each figure comes from -- measured, never estimated
//!
//! * **Inside the guest's address space**, the region map: every mapping with its committed bytes
//!   and the [`MapLabel`] of whoever mapped it ([`omni_mem::label_scope`]). The `mmap` handler
//!   labels a guest `mmap` with the guest's call site, so the engine's heap is broken down by the
//!   code that mapped it (as a link address when the embedding registered the library with
//!   [`register_image`]); this layer labels its own mappings (thread stacks, GLES shadows, Vulkan
//!   host-visible memory, asset buffers, audio buffers); anything unlabelled is this runtime's own
//!   structures. Residency per mapping comes from the OS's resident-page snapshot
//!   ([`omni_platform::vm::resident_set`]).
//! * **Outside it**, the OS's region walk ([`omni_platform::vm::process_regions`]), classified by
//!   what the OS says a region is: private executable memory is the translator's code caches
//!   (dynarmic's per-thread `BlockOfCode`s, or the shared cache of D38); a region the OS marks a
//!   stack is a host thread's stack; other private memory is heaps and allocations (the C heaps'
//!   own totals and this runtime's live Rust allocations, when counted, are printed beside it);
//!   images and mapped files are listed by name.
//! * **The engine's own count**, when the embedding said where its storage is
//!   ([`register_engine_profile_dir`]): the newest `memProfStorage<pid>.json` the engine writes,
//!   whose `CpuMem`, `GpuMem` and `SessionMemorySnapshot` are what the engine believes it holds.
//! * **This runtime's Rust heap**, when the embedding installed [`CountingAllocator`] as its global
//!   allocator: live bytes of every Rust allocation, counted only while `OMNI_MEM_REPORT` is set.
//!
//! **Cost**: nothing until a report is due. A report walks the address space (tens of
//! milliseconds; a `VirtualQuery` per region on Windows, `/proc/self/smaps` on Linux), copies the
//! working set, and holds the guest region map's lock for one pass over it. macOS has no
//! region walk yet (the census refuses by name there): the report prints the guest's side, the
//! totals, and says the host side is missing.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use omni_cpu::stats::CodeCacheTable;
use omni_mem::{GuestAddr, GuestSpace, MapLabel, RegionInfo, RegionKind};
use omni_platform::vm::{self, HeapTotals, HostRegion, HostRegionKind, Residency};
use parking_lot::Mutex;

use crate::boundary::Boundary;

// ------------------------------------------------------------------------------------ owners

/// The engine's anonymous `mmap`s: its allocator's heap (the library imports no allocator; its
/// `mmap` is where its heap comes from), and anything else it maps anonymously. Labelled with the
/// guest call site.
pub const ENGINE_MMAP: &str = "engine heap + anonymous mmap";
/// The engine's read-only file `mmap`s, which this layer serves as a private copy of the file's
/// bytes where a device would share the page cache.
pub const ENGINE_FILE_COPY: &str = "engine read-only file mmap (private copies)";
/// The engine's writable `MAP_SHARED` file views: the file's own pages.
pub const ENGINE_SHARED_FILE: &str = "engine MAP_SHARED file views";
/// Every guest thread's stack, the main one included.
pub const GUEST_THREAD_STACKS: &str = "guest thread stacks";
/// The GLES layer's guest shadows of mapped buffers.
pub const GLES_SHADOWS: &str = "GLES mapped-buffer shadows";
/// Vulkan `HOST_VISIBLE` memory: guest memory the driver imported, one copy.
pub const VULKAN_HOST_VISIBLE: &str = "Vulkan host-visible memory";
/// `AAsset_getBuffer`'s copies of assets.
pub const ASSET_BUFFERS: &str = "asset buffers (AAsset_getBuffer)";
/// AAudio stream buffers.
pub const AAUDIO_BUFFERS: &str = "AAudio stream buffers";
/// The loaded library's own mappings: its file views, `.bss`, and the tail copies. What an
/// embedding labels its `omni_elf::loader::load` with.
pub const LIBRARY: &str = "the loaded library (views, .bss)";
/// A guest mapping made outside any label scope: this runtime's own structures in guest memory
/// (TLS blocks, JNI objects, thunks, the `/proc` answers).
const UNLABELLED: &str = "runtime structures (unlabelled)";

/// Host rows.
const JIT: &str = "JIT code caches (translated code)";
const HOST_STACKS: &str = "host thread stacks";
const HOST_PRIVATE: &str = "other private: heaps, drivers, runtime";
const IMAGES: &str = "images (exe, DLLs / shared libraries)";
const MAPPED: &str = "mapped files and sections";

// ---------------------------------------------------------------------------------- schedule

/// When reports are due, from `OMNI_MEM_REPORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// Seconds into the process, in order.
    pub at: Vec<Duration>,
    /// Then one every this long after the last of [`at`](Self::at) (or from the start).
    pub every: Option<Duration>,
}

impl Schedule {
    /// Parse `OMNI_MEM_REPORT`'s value. See the module table.
    ///
    /// # Errors
    ///
    /// A message naming the value when it is none of the three shapes.
    pub fn parse(text: &str) -> Result<Schedule, String> {
        let text = text.trim();
        let seconds = |s: &str| -> Result<Duration, String> {
            s.trim()
                .parse::<u64>()
                .ok()
                .filter(|&n| n > 0)
                .map(Duration::from_secs)
                .ok_or_else(|| format!("OMNI_MEM_REPORT={text:?}: {s:?} is not a whole number of seconds above 0"))
        };
        if matches!(text, "1" | "on" | "yes" | "true") {
            return Ok(Schedule {
                at: vec![Duration::from_secs(60), Duration::from_secs(180)],
                every: Some(Duration::from_secs(300)),
            });
        }
        if let Some(every) = text.strip_prefix("every:") {
            return Ok(Schedule { at: Vec::new(), every: Some(seconds(every)?) });
        }
        let mut at = text.split(',').map(seconds).collect::<Result<Vec<_>, _>>()?;
        at.sort();
        at.dedup();
        Ok(Schedule { at, every: None })
    }

    /// The `n`th report's time (0-based), or `None` when there is no `n`th.
    #[must_use]
    pub fn due(&self, n: usize) -> Option<Duration> {
        if let Some(at) = self.at.get(n) {
            return Some(*at);
        }
        let every = self.every?;
        let after = n - self.at.len() + 1;
        let from = self.at.last().copied().unwrap_or(Duration::ZERO);
        Some(from + every * u32::try_from(after).ok()?)
    }
}

// ---------------------------------------------------------------------------------- registry

#[derive(Default)]
struct Registry {
    boundary: Option<Weak<Boundary>>,
    images: Vec<Image>,
    profile_dir: Option<PathBuf>,
    started: bool,
}

/// A loaded library, for turning a guest call site into a link address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its name, as a report prints it.
    pub name: String,
    /// Its load bias: link address 0 is here.
    pub base: GuestAddr,
    /// How far it spans from `base`.
    pub len: usize,
}

static REGISTRY: Mutex<Registry> =
    Mutex::new(Registry { boundary: None, images: Vec::new(), profile_dir: None, started: false });

fn schedule() -> Option<Schedule> {
    let text = std::env::var("OMNI_MEM_REPORT").ok().filter(|t| !t.trim().is_empty())?;
    Some(Schedule::parse(&text).unwrap_or_else(|why| panic!("{why}")))
}

/// Called by [`crate::boundary::BoundaryBuilder::finish`]: the first boundary of a process that
/// asked for `OMNI_MEM_REPORT` starts the reporter.
pub(crate) fn register_boundary(boundary: &Arc<Boundary>) {
    let Some(schedule) = schedule() else { return };
    let mut registry = REGISTRY.lock();
    registry.boundary = Some(Arc::downgrade(boundary));
    if registry.started {
        return;
    }
    registry.started = true;
    let started = Instant::now();
    let when = describe(&schedule);
    let spawned = std::thread::Builder::new()
        .name("omnidroid-memreport".to_string())
        .spawn(move || reporter(&schedule, started));
    match spawned {
        Ok(_) => say(&format!(
            "MEMREPORT: ON (OMNI_MEM_REPORT): a table at {when} -- by owner, committed and resident; \
             Rust heap {}",
            if rust_heap_live().is_some() { "counted" } else { "not counted (no CountingAllocator)" }
        )),
        Err(error) => say(&format!("MEMREPORT: the reporter thread could not start: {error}")),
    }
}

fn describe(schedule: &Schedule) -> String {
    let mut parts: Vec<String> = schedule.at.iter().map(|d| format!("+{}s", d.as_secs())).collect();
    if let Some(every) = schedule.every {
        parts.push(format!("then every {}s", every.as_secs()));
    }
    parts.join(", ")
}

/// Name a loaded library so a report prints guest call sites inside it as `name+0xlink`.
pub fn register_image(name: &str, base: GuestAddr, len: usize) {
    let mut registry = REGISTRY.lock();
    registry.images.retain(|image| image.base != base);
    registry.images.push(Image { name: name.to_string(), base, len });
}

/// Where the engine keeps `memProfStorage<pid>.json` (its app data's `LocalStorage`), so a report
/// can print the engine's own count beside this one.
pub fn register_engine_profile_dir(dir: PathBuf) {
    REGISTRY.lock().profile_dir = Some(dir);
}

fn say(text: &str) {
    let mut err = std::io::stderr().lock();
    for line in text.lines() {
        let _ = writeln!(err, "{line}");
    }
}

fn reporter(schedule: &Schedule, started: Instant) {
    for n in 0.. {
        let Some(due) = schedule.due(n) else { return };
        if let Some(wait) = due.checked_sub(started.elapsed()) {
            std::thread::sleep(wait);
        }
        let (boundary, images, profile_dir) = {
            let registry = REGISTRY.lock();
            (
                registry.boundary.as_ref().and_then(Weak::upgrade),
                registry.images.clone(),
                registry.profile_dir.clone(),
            )
        };
        let Some(boundary) = boundary else { return };
        let space = Arc::clone(boundary.mem().space());
        drop(boundary);
        let at = started.elapsed();
        let text = report(&space, &images, profile_dir.as_deref(), at);
        say(&text);
    }
}

// ------------------------------------------------------------------------------- measurement

/// One guest mapping-region, labelled and with its residency.
#[derive(Debug, Clone)]
pub struct GuestPiece {
    /// The region, as the region map reports it.
    pub region: RegionInfo,
    /// Whose it is.
    pub label: MapLabel,
    /// How much of it is resident.
    pub residency: Residency,
}

/// Everything one report is made from. The OS is asked in [`measure`]; [`attribute`] and
/// [`render`] are pure, so the attribution is tested on made-up inputs.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// When, into the process.
    pub at: Duration,
    /// `PrivateUsage` / `VM_ACCOUNT` total, working set, and its shareable part.
    pub process: Option<vm::ProcessMemory>,
    /// The guest's reservation: base and length.
    pub guest_span: (usize, usize),
    /// The guest's committed bytes, per the region map.
    pub guest_committed: usize,
    /// Every guest mapping-region.
    pub guest: Vec<GuestPiece>,
    /// The host's regions, or why there are none.
    pub host: Result<Vec<HostRegion>, String>,
    /// The C heaps' totals.
    pub heaps: Option<HeapTotals>,
    /// Live Rust heap bytes, when counted.
    pub rust_live: Option<u64>,
    /// The shared code cache's committed bytes and count, from `omni-cpu`'s stats.
    pub shared_code_cache: (u64, u64),
    /// What the shared code caches' per-block tables hold on the C heap (dynarmic's census).
    pub code_cache_tables: [CodeCacheTable; 5],
    /// Registered libraries.
    pub images: Vec<Image>,
    /// The engine's own figures, `(name, value)`, and the file they came from.
    pub engine: Option<(String, Vec<(String, String)>)>,
}

impl Default for Inputs {
    fn default() -> Self {
        Self {
            at: Duration::ZERO,
            process: None,
            guest_span: (0, 0),
            guest_committed: 0,
            guest: Vec::new(),
            host: Ok(Vec::new()),
            heaps: None,
            rust_live: None,
            shared_code_cache: (0, 0),
            code_cache_tables: Default::default(),
            images: Vec::new(),
            engine: None,
        }
    }
}

/// Ask the OS and the region map for one report's inputs.
#[must_use]
pub fn measure(space: &GuestSpace, images: &[Image], profile_dir: Option<&Path>, at: Duration) -> Inputs {
    let resident = vm::resident_set();
    let guest = space
        .labelled_regions()
        .into_iter()
        .map(|(region, label)| {
            let worth_asking = region.committed > 0 || matches!(region.kind, RegionKind::File { .. });
            let residency = match (&resident, worth_asking) {
                (Ok(set), true) => set.in_range(region.start, region.len).unwrap_or_default(),
                _ => Residency::default(),
            };
            GuestPiece { region, label, residency }
        })
        .collect();
    let caches = omni_cpu::stats::code_caches_with_tables();
    Inputs {
        at,
        process: vm::process_memory().ok(),
        guest_span: (space.base(), space.len()),
        guest_committed: space.stats().committed,
        guest,
        host: vm::process_regions().map_err(|e| e.to_string()),
        heaps: vm::heap_totals().ok(),
        rust_live: rust_heap_live(),
        shared_code_cache: (caches.committed_bytes, caches.caches),
        code_cache_tables: caches.tables,
        images: images.to_vec(),
        engine: profile_dir.and_then(engine_profile),
    }
}

/// One row of the table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    /// `true` for the guest's side, `false` for the host's.
    pub guest: bool,
    /// Whose.
    pub owner: String,
    /// Bytes mapped or reserved.
    pub mapped: u64,
    /// Private bytes committed.
    pub committed: u64,
    /// Resident, and how much of it is private.
    pub residency: Residency,
    /// How many mappings (guest) or allocations (host).
    pub count: usize,
    /// Detail lines: the largest call sites, allocations or files.
    pub notes: Vec<String>,
}

const MIB: f64 = (1u64 << 20) as f64;

fn mib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / MIB)
}

fn site_name(site: u64, images: &[Image]) -> String {
    let site = site as usize;
    images
        .iter()
        .find(|image| site >= image.base && site - image.base < image.len)
        .map_or_else(|| format!("{site:#x}"), |image| format!("{}+{:#x}", image.name, site - image.base))
}

/// Group the inputs by owner.
#[must_use]
pub fn attribute(inputs: &Inputs) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    // Per owner: the row, each mapping's (start, length, committed, resident), and each call
    // site's (committed, residency, mappings).
    type Mappings = BTreeMap<u64, (usize, usize, u64, u64)>;
    type Sites = HashMap<u64, (u64, Residency, BTreeSet<u64>)>;
    let mut guest_rows: BTreeMap<&str, (Row, Mappings, Sites)> = BTreeMap::new();
    for piece in &inputs.guest {
        let owner = if !piece.label.is_unlabelled() {
            piece.label.owner
        } else if matches!(piece.region.kind, RegionKind::File { .. }) {
            "file views (unlabelled)"
        } else {
            UNLABELLED
        };
        let (row, mappings, sites) = guest_rows.entry(owner).or_default();
        row.mapped += piece.region.len as u64;
        row.committed += piece.region.committed as u64;
        row.residency.add(piece.residency);
        let mapping = piece.region.mapping.map_or(0, |m| m.0);
        let whole = mappings
            .entry(mapping)
            .or_insert((piece.region.mapping_start, piece.region.mapping_len, 0, 0));
        whole.2 += piece.region.committed as u64;
        whole.3 += piece.residency.resident;
        if piece.label.site != 0 {
            let site = sites.entry(piece.label.site).or_default();
            site.0 += piece.region.committed as u64;
            site.1.add(piece.residency);
            site.2.insert(mapping);
        }
    }
    let order = [
        ENGINE_MMAP,
        ENGINE_FILE_COPY,
        ENGINE_SHARED_FILE,
        LIBRARY,
        GUEST_THREAD_STACKS,
        VULKAN_HOST_VISIBLE,
        GLES_SHADOWS,
        ASSET_BUFFERS,
        AAUDIO_BUFFERS,
    ];
    let mut owners: Vec<&str> = guest_rows.keys().copied().collect();
    owners.sort_by_key(|owner| (order.iter().position(|o| o == owner).unwrap_or(order.len()), *owner));
    for owner in owners {
        let (mut row, mappings, sites) = guest_rows.remove(owner).expect("a key just listed");
        row.guest = true;
        row.owner = owner.to_string();
        row.count = mappings.len();
        let mut sites: Vec<_> = sites.into_iter().collect();
        sites.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
        for (site, (committed, residency, mappings)) in sites.iter().take(6) {
            row.notes.push(format!(
                "from {}: {} MiB committed, {} MiB resident, {} mapping(s)",
                site_name(*site, &inputs.images),
                mib(*committed),
                mib(residency.resident),
                mappings.len()
            ));
        }
        if sites.len() > 6 {
            let rest: u64 = sites[6..].iter().map(|s| s.1 .0).sum();
            row.notes.push(format!("{} more call site(s): {} MiB committed", sites.len() - 6, mib(rest)));
        }
        // Where an owner has no call sites, its largest mappings say what holds it: a thread stack
        // a deep frame committed in full, one large file copy.
        if sites.is_empty() {
            let mut largest: Vec<_> = mappings.values().filter(|m| m.2 >= 1 << 20).collect();
            largest.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
            for (start, len, committed, resident) in largest.iter().take(4) {
                row.notes.push(format!(
                    "mapping at {start:#x}: {} MiB committed of {} MiB, {} MiB resident",
                    mib(*committed),
                    mib(*len as u64),
                    mib(*resident)
                ));
            }
        }
        rows.push(row);
    }

    let (base, len) = inputs.guest_span;
    let in_guest = |r: &HostRegion| r.start >= base && r.end() <= base + len;
    let Ok(host) = inputs.host.as_ref() else { return rows };
    let mut jit = Row { owner: JIT.into(), ..Row::default() };
    let mut stacks = Row { owner: HOST_STACKS.into(), ..Row::default() };
    let mut private = Row { owner: HOST_PRIVATE.into(), ..Row::default() };
    let mut images = Row { owner: IMAGES.into(), ..Row::default() };
    let mut mapped = Row { owner: MAPPED.into(), ..Row::default() };
    let mut groups: [BTreeSet<usize>; 5] = Default::default();
    let mut private_allocations: HashMap<usize, u64> = HashMap::new();
    // Per allocation: bytes reserved and resident, to say what a large one is doing.
    let mut private_extent: HashMap<usize, (u64, u64)> = HashMap::new();
    let mut named: [BTreeMap<String, Residency>; 2] = Default::default();
    let mut guest_os_committed = 0u64;
    for region in host {
        if in_guest(region) {
            if region.kind == HostRegionKind::Private {
                guest_os_committed += region.committed;
            }
            continue;
        }
        let residency = region.residency.unwrap_or_default();
        let (row, group) = match region.kind {
            HostRegionKind::Private if region.executable => (&mut jit, 0),
            HostRegionKind::Private if region.stack => (&mut stacks, 1),
            HostRegionKind::Private => {
                *private_allocations.entry(region.allocation_base).or_default() += region.committed;
                let extent = private_extent.entry(region.allocation_base).or_default();
                extent.0 += region.len as u64;
                extent.1 += residency.resident;
                (&mut private, 2)
            }
            HostRegionKind::Image => {
                named[0].entry(region.name.clone().unwrap_or_else(|| "?".into())).or_default().add(residency);
                (&mut images, 3)
            }
            HostRegionKind::Mapped => {
                named[1].entry(region.name.clone().unwrap_or_else(|| "(section)".into())).or_default().add(residency);
                (&mut mapped, 4)
            }
        };
        row.mapped += region.len as u64;
        // A view's or an image's committed pages are the file's, not this process's charge.
        if region.kind == HostRegionKind::Private {
            row.committed += region.committed;
        }
        row.residency.add(residency);
        groups[group].insert(region.allocation_base);
    }
    for (row, group) in [&mut jit, &mut stacks, &mut private, &mut images, &mut mapped].into_iter().zip(&groups) {
        row.count = group.len();
    }
    let (shared_bytes, shared_caches) = inputs.shared_code_cache;
    if shared_caches > 0 {
        jit.notes.push(format!(
            "of which the shared code cache (OMNI_JIT_SHARED_CACHE, D38): {} MiB committed",
            mib(shared_bytes)
        ));
    }
    if let Some(heaps) = inputs.heaps {
        private.notes.push(format!(
            "C heaps ({} heap(s): Rust's System allocator, C/C++ malloc -- dynarmic's tables -- and drivers'): {} MiB committed, {} MiB allocated",
            heaps.heaps,
            mib(heaps.committed),
            mib(heaps.allocated)
        ));
    }
    if let Some(live) = inputs.rust_live {
        private.notes.push(format!("of which this runtime's live Rust allocations: {} MiB", mib(live)));
    }
    let tables = &inputs.code_cache_tables;
    let table_bytes: u64 = tables.iter().map(|t| t.bytes).sum();
    if table_bytes > 0 {
        let each: Vec<String> = tables
            .iter()
            .filter(|t| t.bytes > 0)
            .map(|t| format!("{} {} MiB ({} entries)", t.name, mib(t.bytes), t.entries))
            .collect();
        private.notes.push(format!(
            "of which the shared code cache's per-block tables (dynarmic, census): {} MiB -- {}",
            mib(table_bytes),
            each.join(", ")
        ));
    }
    // Whose an allocation is, where a table's census names an address inside it.
    let mut owners: HashMap<usize, &str> = HashMap::new();
    for table in tables.iter().filter(|t| t.largest_address != 0) {
        let at = table.largest_address;
        if let Some(region) = host.iter().find(|r| r.start <= at && at < r.end()) {
            owners.insert(region.allocation_base, table.name);
        }
    }
    // The allocations worth naming: every one of 16 MiB or more, and at least the five largest of
    // a MiB or more. The rest is the long tail of small ones, which the row's own total holds.
    let mut largest: Vec<(usize, u64)> =
        private_allocations.into_iter().filter(|a| a.1 >= 1 << 20).collect();
    largest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let listed = largest.iter().filter(|a| a.1 >= 16 << 20).count().max(5);
    for (at, bytes) in largest.iter().take(listed) {
        let owner = owners
            .get(at)
            .map(|name| format!(" -- the shared code cache's {name} (dynarmic)"))
            .unwrap_or_default();
        let (reserved, resident) = private_extent.get(at).copied().unwrap_or_default();
        let reserved = if reserved > *bytes { format!(" of {} MiB reserved", mib(reserved)) } else { String::new() };
        private.notes.push(format!(
            "allocation at {at:#x}: {} MiB committed{reserved}, {} MiB resident{owner}",
            mib(*bytes),
            mib(resident)
        ));
    }
    for (row, names) in [(&mut images, &named[0]), (&mut mapped, &named[1])] {
        let mut by: Vec<_> = names.iter().collect();
        by.sort_by(|a, b| b.1.resident.cmp(&a.1.resident).then(a.0.cmp(b.0)));
        for (name, residency) in by.iter().take(6) {
            row.notes.push(format!(
                "{name}: {} MiB resident ({} private)",
                mib(residency.resident),
                mib(residency.private)
            ));
        }
    }
    rows.extend([jit, stacks, private, images, mapped]);
    if let Some(first) = rows.iter_mut().find(|r| r.guest) {
        first.notes.insert(
            0,
            format!(
                "(the whole guest space: {} MiB committed per its region map, {} MiB private committed per the OS)",
                mib(inputs.guest_committed as u64),
                mib(guest_os_committed)
            ),
        );
    }
    rows
}

/// The table, one `MEMREPORT` line each.
#[must_use]
pub fn render(inputs: &Inputs, rows: &[Row]) -> String {
    let mut out = String::new();
    let at = inputs.at.as_secs();
    match &inputs.process {
        Some(p) => {
            let _ = writeln!(
                out,
                "MEMREPORT +{at}s: private (commit charge) {} MiB, working set {} MiB ({} private, {} shareable)",
                mib(p.commit_charge),
                mib(p.resident),
                mib(p.resident.saturating_sub(p.resident_shared)),
                mib(p.resident_shared)
            );
        }
        None => {
            let _ = writeln!(out, "MEMREPORT +{at}s: the process's totals could not be read");
        }
    }
    let _ = writeln!(
        out,
        "MEMREPORT   {:<5} {:<46} {:>10} {:>10} {:>10} {:>10} {:>7}",
        "side", "owner", "mapped", "committed", "resident", "private", "count"
    );
    let mut total = Row::default();
    for row in rows {
        let _ = writeln!(
            out,
            "MEMREPORT   {:<5} {:<46} {:>10} {:>10} {:>10} {:>10} {:>7}",
            if row.guest { "guest" } else { "host" },
            row.owner,
            mib(row.mapped),
            mib(row.committed),
            mib(row.residency.resident),
            mib(row.residency.private),
            row.count
        );
        for note in &row.notes {
            let _ = writeln!(out, "MEMREPORT         {note}");
        }
        total.committed += row.committed;
        total.residency.add(row.residency);
    }
    let _ = writeln!(
        out,
        "MEMREPORT   {:<5} {:<46} {:>10} {:>10} {:>10} {:>10}",
        "", "total (MiB)", "", mib(total.committed), mib(total.residency.resident), mib(total.residency.private)
    );
    if let Some(p) = &inputs.process {
        let _ = writeln!(
            out,
            "MEMREPORT   unattributed: {} MiB of commit charge (page tables; copy-on-write views), {} MiB of the working set",
            mib(p.commit_charge.saturating_sub(total.committed)),
            mib(p.resident.saturating_sub(total.residency.resident))
        );
    }
    if let Err(why) = &inputs.host {
        let _ = writeln!(out, "MEMREPORT   host side missing: {why}");
    }
    match &inputs.engine {
        Some((file, figures)) => {
            let text: Vec<String> = figures.iter().map(|(k, v)| format!("{k} {v}")).collect();
            let _ = writeln!(out, "MEMREPORT   the engine's own count ({file}): {}", text.join(", "));
        }
        None => {
            let _ = writeln!(out, "MEMREPORT   the engine's own count: no memProfStorage file yet");
        }
    }
    out
}

/// Measure, attribute and render one report.
#[must_use]
pub fn report(space: &GuestSpace, images: &[Image], profile_dir: Option<&Path>, at: Duration) -> String {
    let started = Instant::now();
    let inputs = measure(space, images, profile_dir, at);
    let rows = attribute(&inputs);
    let mut text = render(&inputs, &rows);
    let _ = writeln!(text, "MEMREPORT   (measured in {} ms)", started.elapsed().as_millis());
    text
}

// ---------------------------------------------------------------------- the engine's own count

/// The newest `memProfStorage*.json` in `dir`, as `(file name, [(key, value)])` for the keys that
/// say what the engine holds. MiB where the value is bytes.
fn engine_profile(dir: &Path) -> Option<(String, Vec<(String, String)>)> {
    let newest = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("memProfStorage") && name.ends_with(".json")
        })
        .max_by_key(|entry| entry.metadata().and_then(|m| m.modified()).ok())?;
    let text = std::fs::read_to_string(newest.path()).ok()?;
    Some((newest.file_name().to_string_lossy().to_string(), engine_figures(&text)))
}

/// Pull the engine's own memory figures out of a `memProfStorage` document: flat string values,
/// and `SessionMemorySnapshot`'s `key=<n>u` list.
#[must_use]
pub fn engine_figures(text: &str) -> Vec<(String, String)> {
    let value = |key: &str| -> Option<String> {
        let at = text.find(&format!("\"{key}\":\""))? + key.len() + 4;
        let end = text[at..].find('"')? + at;
        Some(text[at..end].to_string())
    };
    let bytes = |v: &str| v.trim_end_matches('u').parse::<u64>().ok().map(|b| format!("{} MiB", mib(b)));
    let mut out = Vec::new();
    for key in ["CpuMem", "GpuMem", "TotalOsMem", "UsedOsMem", "FreeOsMem"] {
        if let Some(v) = value(key) {
            out.push((key.to_string(), bytes(&v).unwrap_or(v)));
        }
    }
    for key in ["AndroidMemoryClass", "AndroidLowRamDevice", "SessionType", "AppSessionTagL2a"] {
        if let Some(v) = value(key) {
            out.push((key.to_string(), v));
        }
    }
    if let Some(snapshot) = value("SessionMemorySnapshot") {
        for part in snapshot.split(',').filter(|p| !p.is_empty()) {
            if let Some((k, v)) = part.split_once('=') {
                out.push((k.to_string(), bytes(v).unwrap_or_else(|| v.to_string())));
            }
        }
    }
    out
}

// ------------------------------------------------------------------------------ the Rust heap

/// A global allocator that counts this runtime's live Rust heap bytes, **only when
/// `OMNI_MEM_REPORT` is set** -- decided once, at the first allocation, so an allocation and its
/// free are always counted alike. Otherwise it is `System` plus one relaxed load.
///
/// An embedding opts in with
///
/// ```ignore
/// #[global_allocator]
/// static ALLOCATOR: omni_android::memreport::CountingAllocator = omni_android::memreport::CountingAllocator;
/// ```
///
/// The count is kept in cache-line-sized shards, one per thread modulo [`SHARDS`], so counting does
/// not make every allocating thread write one shared line.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountingAllocator;

/// How many counters the live total is spread over.
pub const SHARDS: usize = 32;

#[repr(align(64))]
struct Shard(AtomicI64);

static COUNTED: [Shard; SHARDS] = [const { Shard(AtomicI64::new(0)) }; SHARDS];
/// 0: not decided yet; 1: counting; 2: not counting.
static COUNTING: AtomicU8 = AtomicU8::new(0);
static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static MY_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

#[inline]
fn counting() -> bool {
    match COUNTING.load(Ordering::Relaxed) {
        1 => true,
        2 => false,
        _ => {
            let on = omni_platform::process::env_is_set_raw(c"OMNI_MEM_REPORT");
            // Every thread that races here reads the same environment and stores the same answer.
            COUNTING.store(if on { 1 } else { 2 }, Ordering::Relaxed);
            on
        }
    }
}

#[inline]
fn count(delta: i64) {
    let shard = MY_SHARD
        .try_with(|slot| {
            let mut index = slot.get();
            if index == usize::MAX {
                index = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
                slot.set(index);
            }
            index
        })
        // A thread's locals are being torn down: any shard keeps the total right.
        .unwrap_or(0);
    COUNTED[shard].0.fetch_add(delta, Ordering::Relaxed);
}

/// This runtime's live Rust heap bytes, or `None` when they are not being counted (no
/// [`CountingAllocator`] installed, or `OMNI_MEM_REPORT` unset).
#[must_use]
pub fn rust_heap_live() -> Option<u64> {
    if COUNTING.load(Ordering::Relaxed) != 1 {
        return None;
    }
    let sum: i64 = COUNTED.iter().map(|shard| shard.0.load(Ordering::Relaxed)).sum();
    Some(sum.max(0) as u64)
}

// SAFETY: every call is forwarded to `System` with the caller's own arguments, unchanged; the
// counting touches only this module's atomics and a const thread-local with no destructor, neither
// of which allocates.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded as given; the caller upholds `alloc`'s contract.
        let block = unsafe { System.alloc(layout) };
        if !block.is_null() && counting() {
            count(layout.size() as i64);
        }
        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let block = unsafe { System.alloc_zeroed(layout) };
        if !block.is_null() && counting() {
            count(layout.size() as i64);
        }
        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: `block` came from this allocator with `layout`, which is `System`'s.
        unsafe { System.dealloc(block, layout) };
        if counting() {
            count(-(layout.size() as i64));
        }
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `dealloc`, and `new_size` is the caller's.
        let moved = unsafe { System.realloc(block, layout, new_size) };
        if !moved.is_null() && counting() {
            count(new_size as i64 - layout.size() as i64);
        }
        moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omni_mem::MappingId;
    use omni_platform::vm::Protection;

    #[test]
    fn the_schedule_reads_the_three_shapes_and_refuses_anything_else() {
        let on = Schedule::parse("1").unwrap();
        assert_eq!(on.due(0), Some(Duration::from_secs(60)));
        assert_eq!(on.due(1), Some(Duration::from_secs(180)));
        assert_eq!(on.due(2), Some(Duration::from_secs(480)));
        assert_eq!(on.due(3), Some(Duration::from_secs(780)));
        let list = Schedule::parse("300, 30,120").unwrap();
        assert_eq!(list.at, [30, 120, 300].map(Duration::from_secs));
        assert_eq!(list.due(3), None, "a list ends");
        let every = Schedule::parse("every:90").unwrap();
        assert_eq!((every.due(0), every.due(2)), (Some(Duration::from_secs(90)), Some(Duration::from_secs(270))));
        for bad in ["0", "abc", "every:", "10,,20", "-5"] {
            assert!(Schedule::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    fn anonymous(start: usize, len: usize, committed: usize, mapping: u64) -> RegionInfo {
        RegionInfo {
            start,
            len,
            protection: Protection::ReadWrite,
            kind: RegionKind::Anonymous,
            committed,
            mapping: Some(MappingId(mapping)),
            mapping_start: start,
            mapping_len: len,
        }
    }

    fn host(start: usize, len: usize, kind: HostRegionKind, committed: u64, resident: u64) -> HostRegion {
        HostRegion {
            start,
            len,
            kind,
            committed,
            executable: false,
            writable: true,
            stack: false,
            allocation_base: start,
            name: None,
            residency: Some(Residency { resident, private: resident }),
        }
    }

    const M: usize = 1 << 20;

    #[test]
    fn every_byte_lands_in_exactly_one_row_and_the_engine_heap_is_broken_down_by_call_site() {
        let base = 0x10_0000_0000usize;
        let images = vec![Image { name: "libroblox.so".into(), base: base + 0x100_0000, len: 0x800_0000 }];
        let site_a = (base + 0x100_0000 + 0x22_7000) as u64;
        let site_b = 0x7777u64;
        let piece = |region: RegionInfo, label: MapLabel, resident: usize| GuestPiece {
            region,
            label,
            residency: Residency { resident: resident as u64, private: resident as u64 },
        };
        let guest = vec![
            piece(anonymous(base, 64 * M, 40 * M, 1), MapLabel::at(ENGINE_MMAP, site_a), 30 * M),
            piece(anonymous(base + 64 * M, 64 * M, 20 * M, 2), MapLabel::at(ENGINE_MMAP, site_a), 10 * M),
            piece(anonymous(base + 128 * M, 8 * M, 8 * M, 3), MapLabel::at(ENGINE_MMAP, site_b), 8 * M),
            piece(anonymous(base + 136 * M, 10 * M, M, 4), MapLabel::new(GUEST_THREAD_STACKS), M / 2),
            piece(anonymous(base + 146 * M, M, M, 5), MapLabel::default(), M),
        ];
        let jit = HostRegion { executable: true, ..host(0x1000_0000, 32 * M, HostRegionKind::Private, 12 * M as u64, 12 * M as u64) };
        let stack = HostRegion { stack: true, ..host(0x2000_0000, M, HostRegionKind::Private, 64 * 1024, 64 * 1024) };
        let heap = host(0x3000_0000, 100 * M, HostRegionKind::Private, 100 * M as u64, 90 * M as u64);
        let dll = HostRegion {
            name: Some("vulkan-1.dll".into()),
            residency: Some(Residency { resident: 2 * M as u64, private: M as u64 / 4 }),
            ..host(0x4000_0000, 4 * M, HostRegionKind::Image, 4 * M as u64, 0)
        };
        // Inside the guest's span: counted by the guest's side, never again by the host's.
        let guest_committed = host(base, 40 * M, HostRegionKind::Private, 40 * M as u64, 30 * M as u64);
        let inputs = Inputs {
            guest_span: (base, 16 << 30),
            guest_committed: 70 * M,
            guest,
            host: Ok(vec![jit, stack, heap, dll, guest_committed]),
            rust_live: Some(5 * M as u64),
            images,
            ..Inputs::default()
        };
        let rows = attribute(&inputs);
        let row = |owner: &str| rows.iter().find(|r| r.owner == owner).unwrap_or_else(|| panic!("no {owner} row: {rows:#?}"));

        let heap_row = row(ENGINE_MMAP);
        assert_eq!((heap_row.committed, heap_row.residency.resident, heap_row.count), (68 * M as u64, 48 * M as u64, 3));
        assert!(heap_row.notes.iter().any(|n| n.contains("libroblox.so+0x227000: 60.0 MiB committed, 40.0 MiB resident, 2 mapping(s)")), "{:#?}", heap_row.notes);
        assert!(heap_row.notes.iter().any(|n| n.contains("from 0x7777: 8.0 MiB")), "{:#?}", heap_row.notes);
        assert_eq!(rows.iter().position(|r| r.owner == ENGINE_MMAP), Some(0), "the engine's heap leads");
        assert_eq!(row(GUEST_THREAD_STACKS).committed, M as u64);
        assert_eq!(
            row(GUEST_THREAD_STACKS).notes,
            [format!("mapping at {:#x}: 1.0 MiB committed of 10.0 MiB, 0.5 MiB resident", base + 136 * M)],
            "an owner without call sites names its largest mappings"
        );
        assert_eq!(row(UNLABELLED).committed, M as u64);
        assert_eq!(row(JIT).committed, 12 * M as u64);
        assert_eq!(row(HOST_STACKS).committed, 64 * 1024);
        assert_eq!(row(HOST_PRIVATE).committed, 100 * M as u64, "the guest's committed pages are not counted here too");
        assert!(row(HOST_PRIVATE).notes.iter().any(|n| n.contains("live Rust allocations: 5.0 MiB")));
        let images_row = row(IMAGES);
        assert_eq!(images_row.committed, 0, "an image's pages are not this process's commit charge");
        assert_eq!(images_row.residency, Residency { resident: 2 * M as u64, private: M as u64 / 4 });
        assert!(images_row.notes[0].starts_with("vulkan-1.dll: 2.0 MiB resident"), "{:#?}", images_row.notes);
        assert!(heap_row.notes[0].contains("70.0 MiB committed per its region map, 40.0 MiB private committed per the OS"));

        let text = render(&inputs, &rows);
        assert!(text.lines().all(|l| l.starts_with("MEMREPORT")), "{text}");
        assert!(text.contains("total (MiB)"), "{text}");
        assert!(text.contains("the engine's own count: no memProfStorage file yet"), "{text}");
    }

    /// The shared code cache's census names the heap allocations it owns, and every allocation of
    /// 16 MiB or more is listed -- M1 had four such tables and a report that stopped at five rows.
    #[test]
    fn a_large_heap_allocation_is_named_by_the_table_that_owns_it() {
        // Seven private allocations of 16..22 MiB and one of 2 MiB, each its own region; the
        // census puts the link targets' array inside the 20 MiB one, 4 KiB past its base.
        let mut regions: Vec<HostRegion> = (0..7)
            .map(|i| host(0x3000_0000 + i * 0x200_0000, (16 + i) * M, HostRegionKind::Private, ((16 + i) * M) as u64, M as u64))
            .collect();
        regions.push(host(0x5000_0000, 2 * M, HostRegionKind::Private, 2 * M as u64, M as u64));
        let mut tables: [CodeCacheTable; 5] = Default::default();
        tables[1] = CodeCacheTable {
            name: "link targets",
            entries: 1000,
            bytes: 20 * M as u64,
            largest_address: 0x3000_0000 + 4 * 0x200_0000 + 0x1000,
            largest_bytes: 20 * M as u64,
        };
        tables[0] = CodeCacheTable { name: "block map", entries: 900, bytes: M as u64 / 2, ..CodeCacheTable::default() };
        let inputs = Inputs { host: Ok(regions), code_cache_tables: tables, ..Inputs::default() };
        let rows = attribute(&inputs);
        let private = rows.iter().find(|r| r.owner == HOST_PRIVATE).expect("the host's private row");
        let allocations: Vec<&String> = private.notes.iter().filter(|n| n.starts_with("allocation at")).collect();
        assert_eq!(allocations.len(), 7, "every allocation of 16 MiB or more, and not the 2 MiB one: {allocations:#?}");
        assert!(
            allocations.iter().any(|n| *n == "allocation at 0x38000000: 20.0 MiB committed, 1.0 MiB resident -- the shared code cache's link targets (dynarmic)"),
            "{allocations:#?}"
        );
        assert_eq!(allocations.iter().filter(|n| n.contains("dynarmic")).count(), 1, "only the one it names: {allocations:#?}");
        assert!(
            private.notes.iter().any(|n| n
                == "of which the shared code cache's per-block tables (dynarmic, census): 20.5 MiB -- block map 0.5 MiB (900 entries), link targets 20.0 MiB (1000 entries)"),
            "{:#?}",
            private.notes
        );
    }

    #[test]
    fn a_host_without_a_census_still_gets_the_guest_side_and_says_what_is_missing() {
        let inputs = Inputs {
            guest: vec![GuestPiece {
                region: anonymous(0x1000_0000, M, M, 1),
                label: MapLabel::new(GLES_SHADOWS),
                residency: Residency::default(),
            }],
            host: Err("virtual-memory operation `process_regions` is not implemented on macos".into()),
            ..Inputs::default()
        };
        let rows = attribute(&inputs);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].owner, GLES_SHADOWS);
        assert!(render(&inputs, &rows).contains("host side missing: virtual-memory operation"));
    }

    #[test]
    fn the_engines_own_figures_are_read_out_of_its_profile_document() {
        let text = r#"{"CpuMem":"435502656","GpuMem":"149928482","AndroidMemoryClass":"256","SessionMemorySnapshot":"sessionend/totalknown=335844496u,sessionend/highwater=808145512u,","TotalOsMem":"8589934592"}"#;
        let figures = engine_figures(text);
        let get = |k: &str| figures.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
        assert_eq!(get("CpuMem"), Some("415.3 MiB"));
        assert_eq!(get("GpuMem"), Some("143.0 MiB"));
        assert_eq!(get("TotalOsMem"), Some("8192.0 MiB"));
        assert_eq!(get("AndroidMemoryClass"), Some("256"));
        assert_eq!(get("sessionend/totalknown"), Some("320.3 MiB"));
        assert_eq!(get("sessionend/highwater"), Some("770.7 MiB"));
    }

    #[test]
    fn the_counting_allocator_forwards_and_counts_only_when_switched_on() {
        // This test binary does not install it globally; use it directly.
        let allocator = CountingAllocator;
        let layout = Layout::from_size_align(4096, 16).unwrap();
        COUNTING.store(1, Ordering::Relaxed);
        let before = rust_heap_live().unwrap() as i64;
        // SAFETY: a valid non-zero layout; freed below with the same layout.
        let block = unsafe { allocator.alloc(layout) };
        assert!(!block.is_null());
        // SAFETY: `block` is 4096 writable bytes.
        unsafe { block.write_bytes(0xAB, 4096) };
        // SAFETY: `block` came from `allocator.alloc(layout)`.
        let grown = unsafe { allocator.realloc(block, layout, 8192) };
        assert!(!grown.is_null());
        // SAFETY: the first 4096 bytes were preserved by realloc.
        assert_eq!(unsafe { *grown.add(4095) }, 0xAB);
        // Other tests may allocate through it concurrently only if they use it, and none does.
        assert_eq!(rust_heap_live().unwrap() as i64 - before, 8192);
        // SAFETY: `grown` is the live block, now 8192 bytes of `layout`'s alignment.
        unsafe { allocator.dealloc(grown, Layout::from_size_align(8192, 16).unwrap()) };
        assert_eq!(rust_heap_live().unwrap() as i64, before);
        COUNTING.store(2, Ordering::Relaxed);
        assert_eq!(rust_heap_live(), None, "not counting: no figure, rather than a stale one");
    }
}
