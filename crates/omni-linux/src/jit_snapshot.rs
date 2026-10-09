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
        let n = backend.load_translation_snapshot(&path, &key);
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
    });
    start_quiet_saver();
}

/// Save `p`'s snapshot, if it has a target and has translated anything since it was last saved
/// or loaded. Its guest threads wait on the code cache's lock meanwhile.
pub(crate) fn save(p: &Process, why: &str) {
    let (Some(target), Some(backend)) = (p.jit_snapshot.get(), p.backend()) else { return };
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
    let held_ms = backend.code_cache_stats().map_or(0.0, |s| s.snapshot_save_lock_ns as f64 / 1e6);
    eprintln!(
        "[jit-snapshot] pid {} {why}: {} ({:.1} MiB, {:.0} ms, {held_ms:.0} ms of it holding the code cache; {} blocks translated, {} restored, {} verified, {} rejected)",
        p.sys.pid,
        if n >= 0 { format!("{n} blocks saved") } else { format!("not saved (error {n})") },
        size as f64 / (1u64 << 20) as f64,
        t0.elapsed().as_secs_f64() * 1e3,
        stats.blocks_emitted,
        stats.snapshot_blocks_restored,
        stats.snapshot_blocks_verified,
        stats.snapshot_blocks_rejected
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
                    });
                    e.history.push_back(now);
                    if e.history.len() > 5 {
                        e.history.pop_front();
                    }
                    if e.count != now {
                        e.count = now;
                        e.changed = Instant::now();
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
