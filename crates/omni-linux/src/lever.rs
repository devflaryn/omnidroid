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
    fn the_fp_lever_keeps_only_the_unsafe_fp_bits() {
        let done = apply("jit_fp=0xffffffff").expect("understood");
        let want = if cfg!(target_arch = "x86_64") { "jit_fp=0xf0000" } else { "jit_fp=0x0" };
        assert!(done.starts_with(want), "{done}");
        apply("jit_fp=0").expect("understood");
    }
}
