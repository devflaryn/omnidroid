//! **Translation snapshots** (`OMNI_JIT_SNAPSHOT=<dir>`, off by default): a process's translated
//! code saved when it ends (or goes quiet) and installed into the next process of the same program,
//! so that it does not translate again what it ran last time (vendored dynarmic patch 0070).
//!
//! The snapshot of a process is keyed by its program and arguments ([`key`]) and kept as
//! `<dir>/<hash of the key>.odjs`. Installing one is safe by construction, and so is installing
//! the wrong one: inside the snapshot, everything that shapes the emitted code (the prelude, the
//! configuration, the live switches, the host's features and module addresses) is compared before
//! anything is installed, and every block is entered only once the guest code it was translated
//! from reads back the same at its first lookup -- a block whose code differs is dropped and
//! translated as usual.
//!
//! - `OMNI_JIT_SNAPSHOT=<dir>`: on. Each process loads its snapshot when its address space is made
//!   (`spawn`, `execve`), and saves one when it ends.
//! - `OMNI_JIT_SNAPSHOT_QUIET=<seconds>` (default 20; `0` off): also save a live process's once it
//!   has translated nothing new for that long (a service that never ends: system_server), or once
//!   it has *settled* -- at least `OMNI_JIT_SNAPSHOT_SETTLE_S` (default 45) seconds old, and fewer
//!   than 2% of its blocks translated in the last 20 s (the game, which always translates a little,
//!   and is killed rather than ending). A save holds the code cache's lock only while it copies the
//!   snapshot out (`[jit-snapshot]` prints how long); the file is written after.
//! - What is saved: the blocks entered this run (restored ones that verified, and the new ones).
//!   Restored blocks that never verified are left out, so a snapshot is one run's working set and
//!   does not grow across boots.
//! - `OMNI_JIT_SNAPSHOT_MAX_MB=<MiB>` (default 512): no snapshot past that much code.
//! - `OMNI_JIT_SNAPSHOT_FORGET=1` (default off; dynarmic patch 0076): once a process has settled
//!   (as for the settled save), forget the restored blocks it never entered, and their bookkeeping.
//! - `OMNI_JIT_SNAPSHOT_LAZY=1` (default off; dynarmic patch 0075): read a snapshot's code a page at
//!   a time, as blocks on the page are first entered, from the file kept open -- instead of all of it
//!   into private memory at load. Code restored and never entered then costs nothing; the save line
//!   says how many pages were read.
//!
//! **Library placement.** A block is reused only at the guest address it was translated at, and
//! Android's dynamic linker places each library at a random offset inside the range it reserves
//! for it (`ReserveWithAlignmentPadding`, `arc4random`): measured on `linkerconfig`, 30% of a run's
//! blocks were at addresses the previous run's snapshot did not have. With snapshots on, the
//! randomness the linker asks for (`getrandom` from inside `linker64`) is derived from the program,
//! its arguments and the call's order instead of drawn, so a program gets the same layout each run.
//! Everything else stays random: libc's `arc4random` (which reads `getrandom` from `libc.so`) and
//! `AT_RANDOM` (stack canaries).
//!
//! **Library placement, in a busy process** (`OMNI_JIT_SNAPSHOT_LIB_ZONE=1`, default off): the
//! address the linker's reservation for a library gets is the kernel's first fit, which in a
//! process whose threads map stacks and heaps meanwhile differs each boot. With the switch, a
//! reservation from `linker64` goes to a home derived from the library's file and length, in the
//! top 8 GiB of the space ([`linker_mmap`]). The same switch keeps ART's boot image (and boot.oat,
//! the framework's compiled code) at the address it was compiled for (`-Xnorelocate`, added by
//! `crate::props`; without it ART moves the image by a random delta each boot). ART's JIT is off
//! (`dalvik.vm.usejit=false`), and an app's `.oat` is `dlopen`ed through the linker.
//!
//! Logged as `[jit-snapshot]` lines.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use crate::process::Process;

/// Where a process's snapshot is read from and written to.
#[derive(Debug, Clone)]
pub struct Target {
    pub path: PathBuf,
    pub key: String,
    /// Blocks emitted when it was last saved (or loaded), for the quiet saver.
    saved_emitted: Arc<parking_lot::Mutex<(u64, Instant, u64)>>,
    /// The dynamic linker's `getrandom` calls so far ([`linker_random`]).
    linker_calls: Arc<std::sync::atomic::AtomicU64>,
    /// [`linker_mmap`]: the file the linker last mapped a piece of (thread, path, offset), and the
    /// reservations placed in the library zone / of them at their home address.
    linker_file: Arc<parking_lot::Mutex<Option<(i32, Vec<u8>, u64)>>>,
    zone_placed: Arc<std::sync::atomic::AtomicU64>,
    zone_at_home: Arc<std::sync::atomic::AtomicU64>,
    /// Saved just before code aging dropped its translations ([`save_before_trim`]): that save is
    /// its snapshot, and later ones -- of what it translated since, a fraction -- are not made.
    final_saved: Arc<std::sync::atomic::AtomicBool>,
}

/// `OMNI_JIT_SNAPSHOT_LIB_ZONE=1` (default off): the dynamic linker's library reservations placed
/// by the library ([`linker_mmap`]).
fn lib_zone() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_JIT_SNAPSHOT_LIB_ZONE").is_ok_and(|v| v.trim() == "1"))
}

/// Snapshots on and `OMNI_JIT_SNAPSHOT_LIB_ZONE=1`: a layout that is the same each boot -- library
/// reservations in the zone ([`linker_mmap`]) and ART's boot image not relocated (`crate::props`
/// adds `-Xnorelocate`).
#[must_use]
pub fn fixed_layout() -> bool {
    dir().is_some() && lib_zone()
}

/// The library zone: the top [`ZONE_BYTES`] of a process's guest space, which ordinary mappings
/// (first fit upward from 4 GiB) reach only when the space is nearly full.
const ZONE_BYTES: u64 = 8 << 30;
/// Homes are this far apart: a library's home is a whole number of these into the zone.
const ZONE_GRAIN: u64 = 2 << 20;

/// Where a library reservation of `len` bytes for the file piece `identity` (a file name, and the
/// piece's offset in it) goes in a zone of
/// [`ZONE_BYTES`] at `zone`: a home derived from the identity and the length alone -- the same
/// each run, whatever else the process mapped first. `None` for one too large for the zone.
#[must_use]
pub fn zone_home(zone: u64, identity: &[u8], offset: u64, len: u64) -> Option<u64> {
    let span = len.next_multiple_of(ZONE_GRAIN);
    if span > ZONE_BYTES / 4 {
        return None;
    }
    let homes = (ZONE_BYTES - span) / ZONE_GRAIN + 1;
    let h = fnv(identity) ^ offset.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ len.wrapping_mul(0xC2B2_AE3D_27D4_EB4F);
    // One more mixing round: fnv's low bits alone are weak for `% homes`.
    let h = (h ^ (h >> 31)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    Some(zone + (h ^ (h >> 29)) % homes * ZONE_GRAIN)
}

/// **Library placement, the reservation** (`OMNI_JIT_SNAPSHOT_LIB_ZONE=1`, snapshots on). The
/// linker reserves each library's whole span with `mmap(NULL, len, PROT_NONE, MAP_ANONYMOUS)`
/// and maps its segments over it; the kernel's answer is the first fit, which depends on every
/// thread stack and heap mapped before -- in a big multi-threaded process (the game, system_server)
/// a different address each boot for every library loaded after threads started (s22: 2.4% and
/// 9-11% of their restored blocks verified). Here such a call made from `linker64`'s code is given
/// a hint: a home in the library zone ([`zone_home`]) derived from the file the linker last mapped a
/// piece of on that thread (its headers: the library's file name -- not its directory, which an
/// app's reinstall randomizes -- and its offset inside an APK) and the length. Taken if free, else the first free range above it, as for any hint. A file-backed
/// mapping the linker makes is only noted.
pub(crate) fn linker_mmap(p: &Process, tid: i32, pc: u64, a: &mut [u64; 6]) {
    const PROT_NONE: u64 = 0;
    const MAP_ANONYMOUS: u64 = 0x20;
    const MAP_FIXED_ANY: u64 = 0x10 | 0x10_0000;
    if !lib_zone() {
        return;
    }
    let Some(target) = p.jit_snapshot.get() else { return };
    let anonymous = a[3] & MAP_ANONYMOUS != 0;
    // Only the calls that matter, before the name lookup.
    if anonymous && (a[0] != 0 || a[2] != PROT_NONE || a[3] & MAP_FIXED_ANY != 0) {
        return;
    }
    let Some((name, _)) = p.mm.name_at(crate::guest::untag(pc)) else { return };
    if !name.ends_with(b"/linker64") {
        return;
    }
    if !anonymous {
        let Ok(file) = p.fds.get(a[4] as i64 as i32) else { return };
        let path = match &*file.kind.lock() {
            crate::fd::FileKind::Host { guest, .. } | crate::fd::FileKind::Synth { guest, .. } => guest.clone(),
            _ => return,
        };
        // The file's name, not its directory: an app installed again (each boot of the device, by
        // the harness) lands in a new `/data/app/~~<random>/<package>-<random>/`, and its
        // `lib/arm64/libroblox.so` must keep its home.
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(&path).to_vec();
        *target.linker_file.lock() = Some((tid, name, a[5]));
        return;
    }
    let identity = match target.linker_file.lock().take() {
        Some((t, path, offset)) if t == tid => (path, offset),
        // No file seen on this thread: the call's order, as for the linker's randomness.
        _ => (b"#".to_vec(), target.linker_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    };
    let space = p.mem.space();
    let end = space.end() as u64;
    if ((space.end() - space.base()) as u64) < 4 * ZONE_BYTES {
        return;
    }
    let Some(home) = zone_home(end - ZONE_BYTES, &identity.0, identity.1, a[1]) else { return };
    a[0] = home;
    target.zone_placed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// After [`linker_mmap`] gave a hint: whether it was taken.
pub(crate) fn linker_mmap_placed(p: &Process, hint: u64, got: &Result<u64, crate::errno::Errno>) {
    if let (Some(target), Ok(at)) = (p.jit_snapshot.get(), got) {
        if *at == hint {
            target.zone_at_home.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// With snapshots on, the bytes `getrandom` gives a call made from `linker64`'s code (`pc`): derived
/// from the process's snapshot key and the call's order. False -- and `buf` untouched -- for any
/// other caller, or with snapshots off.
pub(crate) fn linker_random(p: &Process, pc: u64, buf: &mut [u8]) -> bool {
    let Some(target) = p.jit_snapshot.get() else { return false };
    let Some((name, _)) = p.mm.name_at(crate::guest::untag(pc)) else { return false };
    if !name.ends_with(b"/linker64") {
        return false;
    }
    let call = target.linker_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut state = fnv(target.key.as_bytes()) ^ call.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for chunk in buf.chunks_mut(8) {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
    }
    true
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// The snapshot directory, if snapshots are on.
#[must_use]
pub fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let d = PathBuf::from(std::env::var_os("OMNI_JIT_SNAPSHOT").filter(|v| !v.is_empty())?);
        std::fs::create_dir_all(&d).ok()?;
        Some(d)
    })
    .as_ref()
}

fn max_bytes() -> u64 {
    std::env::var("OMNI_JIT_SNAPSHOT_MAX_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(512) << 20
}

fn lazy() -> bool {
    static LAZY: OnceLock<bool> = OnceLock::new();
    *LAZY.get_or_init(|| std::env::var("OMNI_JIT_SNAPSHOT_LAZY").is_ok_and(|v| v.trim() == "1"))
}

/// `OMNI_JIT_SNAPSHOT_FORGET=1` (default off; dynarmic patch 0076): once a process has settled,
/// forget the restored blocks it has not entered ([`forget_unentered`]).
fn forget_switch() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_JIT_SNAPSHOT_FORGET").is_ok_and(|v| v.trim() == "1"))
}

/// Forget `p`'s restored blocks never entered: a block restored and never verified costs its
/// bookkeeping (block map entry, links, fastmem sites, guest-range index entries, what verifying it
/// needs -- ~125 bytes a block; s22: ~80 MB for the game's ~640k unentered blocks) for as long as
/// the cache keeps it. Once the process has settled it is unlikely to enter them; one it does
/// enter later is translated as usual.
fn forget_unentered(p: &Process) {
    let Some(backend) = p.backend() else { return };
    where_unentered(p);
    let t0 = Instant::now();
    let n = backend.forget_unverified_translations();
    let stats = backend.code_cache_stats().unwrap_or_default();
    eprintln!(
        "[jit-snapshot] pid {} settled: {n} restored blocks never entered forgotten ({:.0} ms; {} restored, {} verified)",
        p.sys.pid,
        t0.elapsed().as_secs_f64() * 1e3,
        stats.snapshot_blocks_restored,
        stats.snapshot_blocks_verified
    );
}

/// **Diagnostic** (`OMNI_JIT_SNAPSHOT_WHY=1`, off by default): where the restored blocks not
/// entered yet are, by the mapping their guest PC is in now -- printed once a process has settled
/// (before any forgetting). A library at its last run's address shows its own name (code simply
/// not run yet); one that moved shows `(unmapped)` or another mapping's name. One line, the eight
/// largest.
fn where_unentered(p: &Process) {
    if !why() {
        return;
    }
    let Some(backend) = p.backend() else { return };
    let pcs = backend.unverified_translation_pcs();
    let mut by: std::collections::HashMap<Vec<u8>, u64> = std::collections::HashMap::new();
    for pc in &pcs {
        let name = match p.mm.name_at(*pc) {
            Some((name, _)) => name.rsplit(|&b| b == b'/').next().unwrap_or(&name).to_vec(),
            None if p.mem.space().region_at(*pc as usize).is_some() => b"(anonymous)".to_vec(),
            None => b"(unmapped)".to_vec(),
        };
        *by.entry(name).or_default() += 1;
    }
    let mut top: Vec<_> = by.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1));
    let list: Vec<String> = top.iter().take(8).map(|(n, c)| format!("{c} {}", String::from_utf8_lossy(n))).collect();
    eprintln!("[jit-snapshot] pid {} never entered: {} restored blocks: {}", p.sys.pid, pcs.len(), list.join(", "));
}

fn why() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("OMNI_JIT_SNAPSHOT_WHY").is_ok_and(|v| v.trim() == "1"))
}

fn quiet_seconds() -> u64 {
    std::env::var("OMNI_JIT_SNAPSHOT_QUIET").ok().and_then(|v| v.parse().ok()).unwrap_or(20)
}

/// What names a process's snapshot: its program and arguments, and whatever of this host's own
/// switches decides what the guest's code is read as (the native implementations of libc's hot
/// functions, `jit_hle`, which replace guest code at their entry points).
#[must_use]
pub fn key(exe: &[u8], argv: &[Vec<u8>]) -> String {
    let mut k = String::from("omni-jit-snapshot 1\nexe=");
    k.push_str(&String::from_utf8_lossy(exe));
    for a in argv {
        k.push_str("\narg=");
        // An app's host process runs `... android.app.ActivityThread seq=<n>`, `n` the zygote's
        // launch count -- how many launches after boot the app started, not what it runs.
        match a.strip_prefix(b"seq=") {
            Some(n) if !n.is_empty() && n.iter().all(u8::is_ascii_digit) => k.push_str("seq=*"),
            _ => k.push_str(&String::from_utf8_lossy(a)),
        }
    }
    k.push_str(&format!("\nhle={}", omni_cpu::dynarmic::hle_enabled()));
    k.push_str(&format!("\nhost={}", host_build()));
    k
}

/// This host executable, as built: its path, size and modification time. Translated code calls
/// the host's helper functions by absolute address; a snapshot made by another build of the host
/// is not even opened (inside the snapshot, sample addresses are compared as well).
fn host_build() -> &'static str {
    static BUILD: OnceLock<String> = OnceLock::new();
    BUILD.get_or_init(|| {
        let exe = std::env::current_exe().unwrap_or_default();
        let meta = std::fs::metadata(&exe).ok();
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        format!("{} {} {modified}", exe.display(), meta.map_or(0, |m| m.len()))
    })
}

/// Where a process with snapshot key `key` asks for its guest space of `size` bytes when the low
/// range is taken (`crate::process::reserve_space_for`): slots of `size` rounded up to 64 GiB in
/// `[64 TiB, 96 TiB)` -- above where Windows and Linux place anything of their own by default, below
/// the 128 TiB a 47-bit user space ends at -- the first chosen by the key's hash, then the next
/// fifteen (a slot another program of this host process holds, or the host does).
pub(crate) fn space_slots(key: &str, size: usize) -> impl Iterator<Item = usize> {
    const LOW: usize = 0x4000_0000_0000;
    const HIGH: usize = 0x6000_0000_0000;
    let stride = size.next_multiple_of(64 << 30);
    let slots = (HIGH - LOW) / stride;
    let first = (fnv(key.as_bytes()) % slots as u64) as usize;
    (0..16).map(move |i| LOW + ((first + i) % slots) * stride)
}

fn file_for(dir: &std::path::Path, key: &str) -> PathBuf {
    dir.join(format!("{:016x}.odjs", fnv(key.as_bytes())))
}

/// A process's address space was just made for `exe` (`spawn`, `execve`): turn snapshots on for
/// its code cache and install its snapshot if there is one. Before it runs anything.
pub(crate) fn attach(p: &Arc<Process>, exe: &[u8], argv: &[Vec<u8>]) {
    let Some(dir) = dir() else { return };
    let Some(backend) = p.backend() else { return };
    if !backend.enable_translation_snapshots() {
        return;
    }
    let key = key(exe, argv);
    let path = file_for(dir, &key);
    let mut loaded = 0;
    if path.exists() {
        let t0 = Instant::now();
        let n = backend.load_translation_snapshot(&path, &key, lazy());
        eprintln!(
            "[jit-snapshot] pid {} {}: {} ({:.0} ms)",
            p.sys.pid,
            String::from_utf8_lossy(exe),
            if n >= 0 { format!("{n} blocks installed") } else { format!("not installed (error {n})") },
            t0.elapsed().as_secs_f64() * 1e3
        );
        loaded = n.max(0) as u64;
    }
    let _ = p.jit_snapshot.set(Target {
        path,
        key,
        saved_emitted: Arc::new(parking_lot::Mutex::new((0, Instant::now(), loaded))),
        linker_calls: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        linker_file: Arc::default(),
        zone_placed: Arc::default(),
        zone_at_home: Arc::default(),
        final_saved: Arc::default(),
    });
    start_quiet_saver();
}

/// Save `p`'s snapshot, if it has a target and has translated anything since it was last saved
/// or loaded. Its guest threads wait on the code cache's lock meanwhile.
/// Save the process's snapshot now, before code aging drops its translations
/// (`crate::code_trim`), and make it the last: system_server's first trim comes ~60 s into a boot,
/// and a snapshot saved after it held only what it translated since (72-78k blocks of ~550k,
/// 2026-10-10 s5), so the next boot translated its start again.
pub(crate) fn save_before_trim(p: &Process) {
    let Some(target) = p.jit_snapshot.get() else { return };
    if target.final_saved.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    save(p, "before its translations are dropped");
    target.final_saved.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn save(p: &Process, why: &str) {
    let (Some(target), Some(backend)) = (p.jit_snapshot.get(), p.backend()) else { return };
    if target.final_saved.load(std::sync::atomic::Ordering::Relaxed) {
        return;  // saved before a trim: that is its snapshot
    }
    let Some(stats) = backend.code_cache_stats() else { return };
    {
        let last = target.saved_emitted.lock();
        if stats.blocks_emitted == last.0 && last.0 != 0 {
            return;  // nothing new since the last save
        }
        if stats.blocks_emitted == 0 {
            return;  // nothing translated at all (everything came from the snapshot, or nothing ran)
        }
    }
    let t0 = Instant::now();
    // What it entered this run: the restored blocks that verified and the ones it translated --
    // not the restored ones it never needed, which would otherwise ride along boot after boot.
    let n = backend.save_translation_snapshot(&target.path, &target.key, max_bytes(), true);
    let size = std::fs::metadata(&target.path).map(|m| m.len()).unwrap_or(0);
    let placed = target.zone_placed.load(std::sync::atomic::Ordering::Relaxed);
    let zone = if lib_zone() { format!(", {placed} libraries placed, {} at home", target.zone_at_home.load(std::sync::atomic::Ordering::Relaxed)) } else { String::new() };
    let held_ms = backend.code_cache_stats().map_or(0.0, |s| s.snapshot_save_lock_ns as f64 / 1e6);
    eprintln!(
        "[jit-snapshot] pid {} {why}: {} ({:.1} MiB, {:.0} ms, {held_ms:.0} ms of it holding the code cache; {} blocks translated, {} restored, {} verified, {} rejected, {} pages read in{zone})",
        p.sys.pid,
        if n >= 0 { format!("{n} blocks saved") } else { format!("not saved (error {n})") },
        size as f64 / (1u64 << 20) as f64,
        t0.elapsed().as_secs_f64() * 1e3,
        stats.blocks_emitted,
        stats.snapshot_blocks_restored,
        stats.snapshot_blocks_verified,
        stats.snapshot_blocks_rejected,
        stats.snapshot_pages_read
    );
    *target.saved_emitted.lock() = (stats.blocks_emitted, Instant::now(), stats.blocks_emitted);
}

fn settle_seconds() -> u64 {
    std::env::var("OMNI_JIT_SNAPSHOT_SETTLE_S").ok().and_then(|v| v.parse().ok()).unwrap_or(45)
}

/// Whether a process whose translated-block count was `then` 20 s ago and is `now` has settled:
/// fewer than 2% of its blocks are that new.
#[must_use]
pub fn settled(then: u64, now: u64) -> bool {
    now > 0 && (now - then.min(now)) * 50 < now
}

/// The quiet saver: every few seconds, each live process with a snapshot target that has
/// translated something since its last save is saved once it has translated nothing for
/// `OMNI_JIT_SNAPSHOT_QUIET` seconds -- or, once, when it has settled ([`settled`]) after
/// `OMNI_JIT_SNAPSHOT_SETTLE_S` seconds of life.
fn start_quiet_saver() {
    static STARTED: OnceLock<()> = OnceLock::new();
    let quiet = quiet_seconds();
    if quiet == 0 {
        return;
    }
    STARTED.get_or_init(|| {
        let settle = settle_seconds();
        let _ = std::thread::Builder::new().name("jit-snapshot".into()).spawn(move || {
            struct Seen {
                /// Blocks emitted at the last look, and when that count last changed.
                count: u64,
                changed: Instant,
                /// When the process was first seen, and the counts of the last five looks (20 s).
                first: Instant,
                history: std::collections::VecDeque<u64>,
                settled_saved: bool,
                forgot: bool,
            }
            let mut seen: std::collections::HashMap<i32, Seen> = std::collections::HashMap::new();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let live = crate::process::all_live();
                seen.retain(|pid, _| live.iter().any(|p| p.sys.pid == *pid));
                for p in live {
                    let (Some(target), Some(backend)) = (p.jit_snapshot.get(), p.backend()) else { continue };
                    let Some(stats) = backend.code_cache_stats() else { continue };
                    let now = stats.blocks_emitted;
                    let e = seen.entry(p.sys.pid).or_insert_with(|| Seen {
                        count: now,
                        changed: Instant::now(),
                        first: Instant::now(),
                        history: std::collections::VecDeque::new(),
                        settled_saved: false,
                        forgot: false,
                    });
                    e.history.push_back(now);
                    if e.history.len() > 5 {
                        e.history.pop_front();
                    }
                    if e.count != now {
                        e.count = now;
                        e.changed = Instant::now();
                    }
                    if (forget_switch() || why())
                        && !e.forgot
                        && e.first.elapsed().as_secs() >= settle
                        && e.history.len() == 5
                        && settled(e.history[0], now)
                        && stats.snapshot_blocks_restored > 0
                    {
                        e.forgot = true;
                        if forget_switch() {
                            forget_unentered(&p);
                        } else {
                            where_unentered(&p);
                        }
                    }
                    let saved = target.saved_emitted.lock().0;
                    if now <= saved {
                        continue;
                    }
                    if e.changed.elapsed().as_secs() >= quiet {
                        save(&p, "quiet");
                    } else if !e.settled_saved
                        && e.first.elapsed().as_secs() >= settle
                        && e.history.len() == 5
                        && settled(e.history[0], now)
                    {
                        e.settled_saved = true;
                        save(&p, "settled");
                    }
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_has_settled_when_under_2_percent_of_its_blocks_are_new() {
        assert!(settled(99_000, 100_000));
        assert!(!settled(97_000, 100_000));
        assert!(!settled(0, 0));
        assert!(settled(100_000, 100_000));
    }

    #[test]
    fn a_program_s_space_slot_is_the_same_each_time_and_in_the_high_range() {
        let k = key(b"/system/bin/apexd", &[b"/system/bin/apexd".to_vec()]);
        let a: Vec<usize> = space_slots(&k, 64 << 30).collect();
        let b: Vec<usize> = space_slots(&k, 64 << 30).collect();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.iter().all(|&s| s >= 0x4000_0000_0000 && s + (64 << 30) <= 0x6000_0000_0000 && s % (64 << 30) == 0), "{a:x?}");
        let other: Vec<usize> = space_slots(&key(b"/system/bin/idmap2", &[]), 64 << 30).collect();
        assert_ne!(a[0], other[0]);
    }

    #[test]
    fn a_library_s_home_is_its_own_whatever_came_before() {
        let zone = 0x10_0000_0000 - ZONE_BYTES;
        let home = |path: &[u8], offset, len| zone_home(zone, path, offset, len).unwrap();
        let libc = home(b"/apex/com.android.runtime/lib64/bionic/libc.so", 0, 0x10_0000);
        assert_eq!(libc, home(b"/apex/com.android.runtime/lib64/bionic/libc.so", 0, 0x10_0000));
        assert_ne!(libc, home(b"/system/lib64/libm.so", 0, 0x10_0000));
        // Two libraries inside one APK: told apart by their offsets in it.
        assert_ne!(home(b"/data/app/x/base.apk", 0x1000, 0x40_0000), home(b"/data/app/x/base.apk", 0x90_0000, 0x40_0000));
        for (path, len) in [(b"a".as_slice(), 1u64), (b"b", 0x1234_5000), (b"c", ZONE_BYTES / 4)] {
            let h = home(path, 0, len);
            assert!(h >= zone && h + len <= zone + ZONE_BYTES && h % ZONE_GRAIN == 0, "{h:#x}");
        }
        assert_eq!(zone_home(zone, b"huge", 0, ZONE_BYTES / 4 + 1), None);
        // Spread over the zone: 200 libraries, few homes shared.
        let homes: std::collections::HashSet<u64> = (0..200).map(|i| home(format!("/system/lib64/lib{i}.so").as_bytes(), 0, 0x20_0000)).collect();
        assert!(homes.len() >= 195, "{}", homes.len());
    }

    #[test]
    fn a_key_names_the_program_and_its_arguments() {
        let a = key(b"/system/bin/app_process64", &[b"app_process64".to_vec(), b"com.roblox.client".to_vec()]);
        let b = key(b"/system/bin/app_process64", &[b"app_process64".to_vec(), b"com.other".to_vec()]);
        assert_ne!(a, b);
        assert_ne!(file_for(std::path::Path::new("d"), &a), file_for(std::path::Path::new("d"), &b));
        assert!(a.contains("exe=/system/bin/app_process64") && a.contains("arg=com.roblox.client"), "{a}");
    }

    #[test]
    fn an_app_s_launch_count_is_not_part_of_its_key() {
        let exe = b"/system/bin/app_process64";
        let argv = |last: &str| [b"--nice-name=com.roblox.client".to_vec(), last.as_bytes().to_vec()];
        assert_eq!(key(exe, &argv("seq=17")), key(exe, &argv("seq=4")));
        assert_ne!(key(exe, &argv("seq=17")), key(exe, &argv("seqx=17")));
        assert_ne!(key(exe, &argv("seq=1a")), key(exe, &argv("seq=17")));
    }
}
