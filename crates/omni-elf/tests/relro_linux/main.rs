//! **Sealed `PT_GNU_RELRO` is read-only on Linux**: the Linux half of `loader_m1.rs`'s
//! `writing_to_sealed_relro_faults`.
//!
//! That test is portable in name and Windows-shaped in its verdict: it asserts the child died with
//! `STATUS_ACCESS_VIOLATION` (`0xC0000005`), which is an exit *code*. A Linux process killed by a
//! store to a read-only page has no exit code at all -- it was killed by `SIGSEGV` -- so the same
//! (correct) behaviour fails that assertion here. This asserts the Linux form of the same verdict:
//! the child must die, and by `SIGSEGV`, rather than exit. A missing fixture fails.
//!
//! A directory target so that `tests/windows_only.rs`, which accounts for every `tests/*.rs` file by
//! name, does not see it.
#![cfg(target_os = "linux")]

#[path = "../common/mod.rs"]
mod common;

use std::os::unix::process::ExitStatusExt;

use common::fixture::{fixture, load};
use omni_elf::loader::LoaderConfig;

const CHILD: &str = "OMNI_ELF_RELRO_WRITE_CHILD_LINUX";
const NO_FAULT: i32 = 7;
/// `SIGSEGV`, 11 on every Linux architecture.
const SIGSEGV: i32 = 11;

#[test]
fn writing_to_sealed_relro_kills_the_process_with_sigsegv() {
    if std::env::var_os(CHILD).is_some() {
        let f = fixture().expect("the libroblox.so fixture: a missing fixture is a failure");
        let object = load(&f, &LoaderConfig::default());
        let relro = object.relro.expect("PT_GNU_RELRO");
        let at = relro.start + relro.sealed_bytes() / 2;
        // SAFETY: none. The store is expected to raise SIGSEGV and kill this process, because the
        // relro region is PROT_READ after sealing.
        unsafe { std::ptr::write_volatile(f.space.ptr(at, 1).expect("in the space"), 0x5a) };
        std::process::exit(NO_FAULT);
    }
    let status = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["writing_to_sealed_relro_kills_the_process_with_sigsegv", "--exact", "--nocapture"])
        .env(CHILD, "1")
        .status()
        .expect("run the child");
    assert_ne!(
        status.code(),
        Some(NO_FAULT),
        "the child wrote into PT_GNU_RELRO after sealing: relro is not actually read-only"
    );
    assert_eq!(status.signal(), Some(SIGSEGV), "expected death by SIGSEGV, got {status}");
}
