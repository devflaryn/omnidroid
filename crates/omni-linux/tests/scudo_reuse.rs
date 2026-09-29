//! Scudo's large blocks reused after a purge: calloc zeroes a block the allocator handed back to
//! the OS (`madvise(MADV_DONTNEED)`) and takes again. Roblox's engine died in that memset, 64 KiB into
//! a reused secondary block, with SEGV_ACCERR -- the guest space said the rest of the block could
//! not be written (Linux, run 2026-09-29 `omni-linux-r-858242`).
mod common;

#[test]
fn a_purged_large_block_is_writable_again_when_it_is_reused() {
    let Some((status, out, err)) = common::run_fixture("scudo_reuse", &[]) else { return };
    assert_eq!(status, omni_linux::ExitStatus::Exited(0), "stdout: {out}\nstderr: {err}");
    assert!(out.contains("scudo ok"), "stdout: {out}\nstderr: {err}");
}
