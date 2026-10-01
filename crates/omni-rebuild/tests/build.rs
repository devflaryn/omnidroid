//! The build helper (C3): detect an arm64 clang toolchain and compile candidate C into a shared
//! object the lab can load. On a host with no toolchain (this dev/CI Windows box) detection must
//! fail cleanly, never panic, and the compile test self-skips.

use omni_rebuild::{compile_shared, detect, BuildError};

#[test]
fn detect_reports_missing_toolchain_cleanly() {
    match detect() {
        Ok(tc) => assert!(tc.target.contains("aarch64"), "a detected target must be aarch64: {}", tc.target),
        Err(BuildError::ToolchainMissing { looked_for }) => assert!(
            looked_for.iter().any(|s| s.contains("OMNI_NDK") || s.contains("clang")),
            "the missing-toolchain error must name where it looked: {looked_for:?}"
        ),
        Err(e) => panic!("unexpected error shape from detect(): {e:?}"),
    }
}

#[test]
fn compile_happy_path_when_toolchain_present() {
    let tc = match detect() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("no arm64 toolchain on this host; skipping the compile happy-path");
            return;
        }
    };
    let dir = std::env::temp_dir().join(format!("omni-rebuild-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("cand.so");
    let src = ("cand.c".to_string(), "int add(int a, int b) { return a + b; }".to_string());
    let so = compile_shared(&tc, &[src], &out, &[]).expect("compile a trivial shared object");
    assert!(so.exists(), "compile_shared must produce the .so");
    let _ = std::fs::remove_dir_all(&dir);
}
