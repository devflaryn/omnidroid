//! **Measurement levers switched while an instance runs**, so an A/B is taken inside one live
//! session -- the same world, the same players, the same host load -- phase against phase, rather
//! than across boots minutes apart (docs/NIGHT-2026-10-02.md, `tools/perf_live.ps1`).
//!
//! `OMNI_LEVER_FILE=<path>`: every host process of the instance reads the file four times a second;
//! when its text changes, each `name=value` line is applied and said on stderr (`[lever]`). Levers:
//!
//! - `jit_fp=<hex mask>`: dynarmic's unsafe floating-point flags
//!   (`omni_cpu::dynarmic::set_live_fp_optimizations`), then every process's translations are
//!   dropped so what runs next is translated with them. 0 is the accurate default.
//!
//! - `compose_fast=0|1`: the composer's fast path (`crate::hal::compose::FAST`: the same pixels
//!   in fewer passes, its buffers kept from frame to frame). Off by default.
//! - `fence_poll=<microseconds>`: a guest `vkWaitForFences` polled with that period instead of the
//!   host driver's own (spinning, on NVIDIA) wait (`crate::gpu::FENCE_POLL_US`). 0 is the driver's.
//!
//! - `zero_reclaim=<seconds>`: sweep every guest process's resident pages of zeros out of the
//!   working set that often (`crate::zero_reclaim`; RAM, not commit). 0, the default, stops it.
//! - `madv_free=0|1`: a guest `madvise(MADV_FREE)` carried out as `MADV_DONTNEED` -- the range
//!   decommitted at once, which Linux's "old contents or zeros" allows -- rather than ignored
//!   (`crate::mm::MADV_FREE_DISCARDS`). Off by default.
//!
//! `OMNI_JIT_UNSAFE_FP=<hex mask>` sets the same flags from the start, without a file;
//! `OMNI_ZERO_RECLAIM=<seconds>` and `OMNI_MADV_FREE=1` the last two.
use std::path::PathBuf;
use std::time::Duration;

/// How often the file is read.
pub const POLL: Duration = Duration::from_millis(250);

fn parse_hex(v: &str) -> Option<u32> {
    u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
}

/// Apply one `name=value` line; what was done, for the log, or `None` for a line not understood.
pub fn apply(line: &str) -> Option<String> {
    let (name, value) = line.split_once('=')?;
    match name.trim() {
        "jit_fp" => {
            let kept = omni_cpu::dynarmic::set_live_fp_optimizations(parse_hex(value)?);
            let live = crate::process::all_live();
            for p in &live {
                p.trim_code();
            }
            Some(format!("jit_fp={kept:#x}: {} processes' translations dropped", live.len()))
        }
        "compose_fast" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::compose::FAST.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("compose_fast={}", u8::from(on)))
        }
        "zero_reclaim" => {
            let seconds: u64 = value.trim().parse().ok()?;
            crate::zero_reclaim::set_period(seconds);
            Some(format!("zero_reclaim={seconds}: {}", if seconds == 0 { "no sweeps" } else { "zero pages swept out of the working set" }))
        }
        "madv_free" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::mm::MADV_FREE_DISCARDS.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("madv_free={}: MADV_FREE {}", u8::from(on), if on { "discards the range" } else { "is a hint" }))
        }
        "fence_poll" => {
            let us: u32 = value.trim().parse().ok()?;
            crate::gpu::FENCE_POLL_US.store(us, std::sync::atomic::Ordering::Relaxed);
            Some(format!("fence_poll={us}: a guest vkWaitForFences {}", if us == 0 { "is the host driver's wait" } else { "is polled" }))
        }
        _ => None,
    }
}

/// Start the lever reader for this host process (once), and apply `OMNI_JIT_UNSAFE_FP`.
pub fn start() {
    crate::mm::madv_free_from_env();
    if let Some(mask) = std::env::var("OMNI_JIT_UNSAFE_FP").ok().as_deref().and_then(parse_hex) {
        let kept = omni_cpu::dynarmic::set_live_fp_optimizations(mask);
        eprintln!("[lever] OMNI_JIT_UNSAFE_FP: jit_fp={kept:#x}");
    }
    let Some(path) = std::env::var_os("OMNI_LEVER_FILE").map(PathBuf::from) else { return };
    let _ = std::thread::Builder::new().name("omni-lever".into()).spawn(move || {
        let mut seen = String::new();
        loop {
            std::thread::sleep(POLL);
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            if text == seen {
                continue;
            }
            seen = text;
            for line in seen.lines().filter(|l| !l.trim().is_empty()) {
                match apply(line) {
                    Some(done) => eprintln!("[lever] pid {}: {done}", std::process::id()),
                    None => eprintln!("[lever] not understood: {line:?}"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_masks_parse_with_or_without_the_prefix() {
        assert_eq!(parse_hex("0x40000"), Some(0x40000));
        assert_eq!(parse_hex("f0000\n"), Some(0xf0000));
        assert_eq!(parse_hex("zz"), None);
    }

    #[test]
    fn a_line_without_a_known_lever_is_not_understood() {
        assert_eq!(apply("nope=1"), None);
        assert_eq!(apply("no equals sign"), None);
    }

    #[test]
    fn the_compose_lever_switches_the_fast_path() {
        use std::sync::atomic::Ordering;
        apply("compose_fast=0").expect("understood");
        assert!(!crate::hal::compose::FAST.load(Ordering::Relaxed));
        apply("compose_fast=1").expect("understood");
        assert!(crate::hal::compose::FAST.load(Ordering::Relaxed));
        assert_eq!(apply("compose_fast=yes"), None);
    }

    #[test]
    fn the_fence_poll_lever_sets_the_period() {
        use std::sync::atomic::Ordering;
        assert!(apply("fence_poll=250").expect("understood").starts_with("fence_poll=250"));
        assert_eq!(crate::gpu::FENCE_POLL_US.load(Ordering::Relaxed), 250);
        assert_eq!(apply("fence_poll=x"), None);
        apply("fence_poll=0").expect("understood");
        assert_eq!(crate::gpu::FENCE_POLL_US.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_memory_levers_switch() {
        use std::sync::atomic::Ordering;
        assert!(apply("zero_reclaim=30").expect("understood").starts_with("zero_reclaim=30"));
        assert_eq!(crate::zero_reclaim::PERIOD_S.load(Ordering::Relaxed), 30);
        apply("zero_reclaim=0").expect("understood");
        assert_eq!(crate::zero_reclaim::PERIOD_S.load(Ordering::Relaxed), 0);
        assert_eq!(apply("zero_reclaim=soon"), None);
        apply("madv_free=1").expect("understood");
        assert!(crate::mm::MADV_FREE_DISCARDS.load(Ordering::Relaxed));
        apply("madv_free=0").expect("understood");
        assert!(!crate::mm::MADV_FREE_DISCARDS.load(Ordering::Relaxed));
        assert_eq!(apply("madv_free=yes"), None);
    }

    #[test]
    fn the_fp_lever_keeps_only_the_unsafe_fp_bits() {
        let done = apply("jit_fp=0xffffffff").expect("understood");
        let want = if cfg!(target_arch = "x86_64") { "jit_fp=0xf0000" } else { "jit_fp=0x0" };
        assert!(done.starts_with(want), "{done}");
        apply("jit_fp=0").expect("understood");
    }
}
