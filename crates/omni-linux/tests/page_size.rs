//! What bionic tells a program its page is: 4 KiB on every host that can give it (D42, spec
//! 2026-09-29-4k-guest-pages) -- the real AOSP toybox, through the real linker64 and libc.so.
mod common;

use common::run;
use omni_linux::ExitStatus;

fn four_kib_expected() -> bool {
    omni_platform::vm::page_size() == 4096 || omni_platform::vm::supports_alias()
}

#[test]
fn getconf_pagesize_is_4096() {
    let Some((status, out, err)) = run(&["/system/bin/toybox", "getconf", "PAGESIZE"]) else { return };
    assert_eq!(status, ExitStatus::Exited(0), "stderr: {err}");
    let want = if four_kib_expected() { 4096 } else { omni_platform::vm::page_size() };
    assert_eq!(out.trim(), want.to_string(), "sysconf(_SC_PAGESIZE)");
}

// AT_PAGESZ is `Mm::page_size()` (process.rs, the auxv), pinned at 4096 by tests/mm.rs; bionic's
// sysconf(_SC_PAGESIZE) is getpagesize(), which reads it -- the test above. (No /proc/self/auxv
// here to read it back directly.)
