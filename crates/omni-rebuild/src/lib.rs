//! The RE workbench's build helper (C3).
//!
//! Closing the reconstruction loop needs the agent's candidate C compiled back into an arm64
//! shared object the lab can load beside the original for differential testing. This crate finds an
//! arm64 `clang` and drives it; it does **not** itself implement a compiler, and it adds no OS
//! access beyond launching that external process.
//!
//! The toolchain is a host prerequisite, not something omnidroid ships. [`detect`] looks for it and
//! returns a named error when it is absent, so the layer above can tell the agent what to install
//! rather than producing a wrong-architecture artifact or panicking.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Anything that can go wrong building a candidate.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// No arm64 clang was found. `looked_for` lists, in order, the places that were tried.
    #[error("no arm64 clang toolchain found (looked for: {})", looked_for.join(", "))]
    ToolchainMissing {
        /// The env vars and PATH probes that were tried, for the message the agent sees.
        looked_for: Vec<String>,
    },
    /// A source file could not be written, or the output directory could not be created.
    #[error("filesystem error preparing the build: {0}")]
    Io(String),
    /// The compiler ran and failed; `diagnostics` is its stderr verbatim.
    #[error("the compiler failed:\n{diagnostics}")]
    Compile {
        /// The compiler's stderr, so the agent can fix its C.
        diagnostics: String,
    },
}

/// A located arm64 clang and the target triple to drive it with.
#[derive(Debug, Clone)]
pub struct Toolchain {
    /// Path to the `clang` executable.
    pub clang: PathBuf,
    /// The `--target=` triple (always contains `aarch64`).
    pub target: String,
}

/// The target triple used with an NDK clang.
const ANDROID_TARGET: &str = "aarch64-linux-android";
/// The target triple used with a generic LLVM clang found on `PATH`.
const GNU_TARGET: &str = "aarch64-linux-gnu";

/// Find an arm64 clang: first via the NDK environment variables, then a `clang` on `PATH` that
/// lists an `aarch64` target.
///
/// # Errors
///
/// [`BuildError::ToolchainMissing`] naming every place that was tried, when none is found.
pub fn detect() -> Result<Toolchain, BuildError> {
    let mut looked_for = Vec::new();

    for var in ["OMNI_NDK", "ANDROID_NDK_HOME", "ANDROID_NDK_ROOT"] {
        looked_for.push(format!("${var}"));
        if let Ok(ndk) = std::env::var(var) {
            if !ndk.is_empty() {
                if let Some(clang) = find_ndk_clang(Path::new(&ndk)) {
                    return Ok(Toolchain { clang, target: ANDROID_TARGET.to_string() });
                }
            }
        }
    }

    looked_for.push("clang on PATH with an aarch64 target".to_string());
    if clang_supports_aarch64() {
        return Ok(Toolchain { clang: PathBuf::from("clang"), target: GNU_TARGET.to_string() });
    }

    Err(BuildError::ToolchainMissing { looked_for })
}

/// Locate `clang` inside an NDK: `<ndk>/toolchains/llvm/prebuilt/<host>/bin/clang[.exe]`.
fn find_ndk_clang(ndk: &Path) -> Option<PathBuf> {
    let prebuilt = ndk.join("toolchains").join("llvm").join("prebuilt");
    for host in std::fs::read_dir(&prebuilt).ok()?.flatten() {
        let bin = host.path().join("bin");
        for name in ["clang.exe", "clang"] {
            let candidate = bin.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether a `clang` on `PATH` lists an `aarch64` target. Returns false (never panics) when clang
/// is absent or cannot be run.
fn clang_supports_aarch64() -> bool {
    match Command::new("clang").arg("--print-targets").output() {
        Ok(out) => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.contains("aarch64")
        }
        Err(_) => false,
    }
}

/// Compile `sources` (each `(filename, text)`) into a shared object at `out`, returning `out`.
///
/// Sources are written next to `out`. The compile is `clang --target=<triple> -shared -fPIC -o out
/// <sources> <extra_flags>`.
///
/// # Errors
///
/// [`BuildError::Io`] if a source or the output directory could not be prepared;
/// [`BuildError::Compile`] carrying the compiler's stderr if it could not be launched or exited
/// non-zero.
pub fn compile_shared(
    tc: &Toolchain,
    sources: &[(String, String)],
    out: &Path,
    extra_flags: &[String],
) -> Result<PathBuf, BuildError> {
    let dir = out
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).map_err(|e| BuildError::Io(e.to_string()))?;

    let mut src_paths = Vec::with_capacity(sources.len());
    for (name, text) in sources {
        let p = dir.join(name);
        std::fs::write(&p, text).map_err(|e| BuildError::Io(e.to_string()))?;
        src_paths.push(p);
    }

    let mut cmd = Command::new(&tc.clang);
    cmd.arg(format!("--target={}", tc.target))
        .arg("-shared")
        .arg("-fPIC")
        .arg("-o")
        .arg(out);
    for p in &src_paths {
        cmd.arg(p);
    }
    for f in extra_flags {
        cmd.arg(f);
    }

    let output = cmd.output().map_err(|e| BuildError::Compile {
        diagnostics: format!("could not launch {}: {e}", tc.clang.display()),
    })?;
    if !output.status.success() {
        return Err(BuildError::Compile {
            diagnostics: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(out.to_path_buf())
}
