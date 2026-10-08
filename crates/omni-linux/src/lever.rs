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
//!   in fewer passes, its buffers kept from frame to frame). On by default.
//! - `compose_zero=0|1`: the composer's layers read where they are in their gralloc regions and
//!   composed straight into the framebuffer's next frame, by the run path
//!   (`crate::hal::compose::ZERO`): two to three fewer 5.6 MB copies a frame. Off by default.
//! - `present_bgra=0|1`: the frame composed as BGRA, and the Win32 window takes it shared rather
//!   than swizzling a copy (`crate::hal::compose::BGRA_OUT`). Off by default.
//! - `fence_poll=<microseconds>`: a guest `vkWaitForFences` polled with that period instead of the
//!   host driver's own (spinning, on NVIDIA) wait (`crate::gpu::FENCE_POLL_US`). 0 is the driver's.
//!
//! `OMNI_JIT_UNSAFE_FP=<hex mask>` sets the same flags from the start, without a file.
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
        "compose_zero" | "present_bgra" => {
            let on = match value.trim() {
                "1" => true,
                "0" => false,
                _ => return None,
            };
            crate::hal::compose::levers_from_env();
            let lever = if name.trim() == "compose_zero" { &crate::hal::compose::ZERO } else { &crate::hal::compose::BGRA_OUT };
            lever.store(on, std::sync::atomic::Ordering::Relaxed);
            Some(format!("{}={}", name.trim(), u8::from(on)))
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
    fn the_zero_copy_and_bgra_levers_switch() {
        use std::sync::atomic::Ordering;
        use crate::hal::compose::{BGRA_OUT, ZERO};
        assert_eq!(apply("compose_zero=1").as_deref(), Some("compose_zero=1"));
        assert!(ZERO.load(Ordering::Relaxed));
        assert_eq!(apply("present_bgra=1").as_deref(), Some("present_bgra=1"));
        assert!(BGRA_OUT.load(Ordering::Relaxed));
        apply("compose_zero=0").expect("understood");
        apply("present_bgra=0").expect("understood");
        assert!(!ZERO.load(Ordering::Relaxed) && !BGRA_OUT.load(Ordering::Relaxed));
        assert_eq!(apply("compose_zero=on"), None);
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
    fn the_fp_lever_keeps_only_the_unsafe_fp_bits() {
        let done = apply("jit_fp=0xffffffff").expect("understood");
        let want = if cfg!(target_arch = "x86_64") { "jit_fp=0xf0000" } else { "jit_fp=0x0" };
        assert!(done.starts_with(want), "{done}");
        apply("jit_fp=0").expect("understood");
    }
}
