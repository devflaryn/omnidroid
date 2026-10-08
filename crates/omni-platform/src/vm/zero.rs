//! **Resident pages of zeros, given back to the OS without changing what they read** (Windows).
//!
//! On Linux a read of anonymous memory nobody wrote maps the shared zero page, and costs no RAM;
//! on Windows the first touch of a committed page -- a read as much as a write -- gives the process
//! a private page of zeros, which stays in its private working set until the process ends or the
//! memory manager trims it. MEASURED (PS99 in-world, 2026-10-08, `wsscan.ps1`): the game's host
//! process held 169 MiB of resident all-zero 4 KiB pages in guest memory, the system's host
//! process 10 MiB more in guest granules besides its other classes.
//!
//! [`reset_zero_run`] takes a run of pages that read zero and moves them out of the working set
//! with their contents declared disposable (`MEM_RESET`), so the memory manager may reuse the
//! frames without writing them anywhere; the next touch reads zeros again (the old frame from the
//! standby list, or a fresh demand-zero page). Commit charge is not returned -- the pages stay
//! committed, as they must for the guest's next access to need nothing of ours -- only RAM.
//!
//! # Why it is safe against a concurrent writer
//!
//! The guest may write a page at any moment; a write that `MEM_RESET` discarded would be lost. The
//! run is **locked in the working set** (`VirtualLock`) before the reset and checked for zeros
//! again **after** it:
//!
//! * a write before the second check is seen by it -- a locked page is never trimmed, so its frame
//!   still holds every write -- and every page of the run found written is written again, still
//!   locked, with an atomic `or` of zero into its first word: a store that changes no byte (atomic, so
//!   it loses no concurrent store either) but marks the page dirty, which is all the reset took
//!   from it. (`MEM_RESET_UNDO` is no use here: on a locked range it fails, `ERROR_BUSY`.);
//! * a write after the second check lands on a page whose dirty state the reset cleared, so the
//!   hardware marks it dirty again and the memory manager keeps its contents like any other
//!   modified page (the same rule a heap's `MEM_RESET` and then reuse by plain writes relies on).
//!
//! MEASURED (this host, 256 MiB, `scratchpad/ram/resetexp.ps1`): pages written and removed from
//! the working set went to the **modified** list (+256 MiB, written to the pagefile); with the
//! lock-reset-unlock above they went to the standby list (modified list +0), and read back their
//! contents while still there.
//!
//! Only for memory nothing but the CPU writes: a device writing by DMA marks no page dirty. Its
//! one caller is `omni-mem`, for a guest's private anonymous memory -- which the host's GPU never
//! writes (`VK_EXT_external_memory_host` is not forwarded) and no I/O is done into directly.

use super::{VmError, VmResult};

/// What [`reset_zero_run`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroRun {
    /// Every page still read zero after the reset: they are out of the working set.
    Reset,
    /// A page was written meanwhile: the run was marked dirty again and left in the working set.
    Written,
    /// The run could not be locked (working-set quota) or reset: nothing was done.
    Skipped,
}

/// Whether each of the `pages` 4 KiB pages from `start` is in this process's working set, by one
/// `QueryWorkingSetEx` -- which asks, and touches nothing.
///
/// # Errors
///
/// [`VmError::Os`] if the OS refuses; [`VmError::Unsupported`] off Windows.
pub fn resident_pages(start: usize, pages: usize) -> VmResult<Vec<bool>> {
    os::resident_pages(start, pages)
}

/// Whether the 4 KiB page at `page` reads all zeros. A plain read: the page must be committed and
/// readable, and nothing may unmap it meanwhile.
///
/// # Safety
///
/// `[page, page + 4096)` must be committed, readable memory of this process for the whole call.
#[must_use]
pub unsafe fn page_is_zero(page: *const u8) -> bool {
    // SAFETY: the caller's contract; the words are read volatile because another thread may be
    // writing them -- a torn or stale read only makes a page look non-zero (or zero, which the
    // check after the reset then settles).
    unsafe { os::page_is_zero(page) }
}

/// Take the `pages` pages from `start`, which read zero a moment ago, out of the working set
/// without losing a concurrent write. See the module documentation.
///
/// # Errors
///
/// [`VmError::Unsupported`] off Windows.
///
/// # Safety
///
/// `[start, start + pages * 4096)` must be committed, readable, private anonymous memory of this
/// process, written by nothing but the CPU, and must stay mapped for the whole call.
pub unsafe fn reset_zero_run(start: *mut u8, pages: usize) -> VmResult<ZeroRun> {
    // SAFETY: the caller's contract.
    unsafe { os::reset_zero_run(start, pages) }
}

#[cfg(windows)]
mod os {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::GetLastError;
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualLock, VirtualUnlock, MEM_RESET, PAGE_NOACCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::{VmError, VmResult, ZeroRun};
    use crate::vm::OsError;

    const PAGE: usize = 4096;

    fn os(operation: &'static str, address: usize, size: usize) -> VmError {
        // SAFETY: reads this thread's last-error value; no preconditions.
        let code = unsafe { GetLastError() };
        VmError::Os { operation, address, size, source: OsError(code) }
    }

    pub(super) fn resident_pages(start: usize, pages: usize) -> VmResult<Vec<bool>> {
        if pages == 0 {
            return Ok(Vec::new());
        }
        let mut q: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..pages)
            .map(|i| PSAPI_WORKING_SET_EX_INFORMATION { VirtualAddress: (start + i * PAGE) as *mut c_void, ..Default::default() })
            .collect();
        let bytes = u32::try_from(pages * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()).map_err(|_| VmError::Os {
            operation: "QueryWorkingSetEx",
            address: start,
            size: pages * PAGE,
            source: OsError(87),
        })?;
        // SAFETY: `q` is writable for `bytes`; the call reads the addresses and writes the
        // attributes, touching no page.
        if unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), q.as_mut_ptr().cast(), bytes) } == 0 {
            return Err(os("QueryWorkingSetEx", start, pages * PAGE));
        }
        // SAFETY: `Flags` is the union's plain word; bit 0 is `Valid` (resident).
        Ok(q.iter().map(|e| unsafe { e.VirtualAttributes.Flags } & 1 != 0).collect())
    }

    pub(super) unsafe fn page_is_zero(page: *const u8) -> bool {
        let words = page.cast::<u64>();
        // Most pages that are not zero say so in their first words; read in order and stop there.
        (0..PAGE / 8).all(|i| {
            // SAFETY: the caller's contract: the page is committed and readable.
            unsafe { words.add(i).read_volatile() == 0 }
        })
    }

    pub(super) unsafe fn reset_zero_run(start: *mut u8, pages: usize) -> VmResult<ZeroRun> {
        let size = pages * PAGE;
        let at = start.cast::<c_void>();
        // SAFETY (all four calls): the caller's contract -- committed private memory of this
        // process for the whole call. None of them reads or writes the pages' contents.
        if unsafe { VirtualLock(at, size) } == 0 {
            return Ok(ZeroRun::Skipped);
        }
        if unsafe { VirtualAlloc(at, size, MEM_RESET, PAGE_NOACCESS) }.is_null() {
            unsafe { VirtualUnlock(at, size) };
            return Ok(ZeroRun::Skipped);
        }
        // Every write made before this point is in the locked frames, so this sees it.
        // SAFETY: the caller's contract, page by page.
        let written: Vec<usize> = (0..pages).filter(|&i| !unsafe { page_is_zero(start.add(i * PAGE)) }).collect();
        if !written.is_empty() {
            for i in written {
                // SAFETY: the page is committed private memory that was just written, so it is
                // writable, and its first word is 8-aligned. An atomic `or` of zero changes nothing
                // and is a store: the page is dirty again, as the write before the reset made it.
                // (A page that reads zero again needs nothing: losing it loses only zeros.)
                let word = unsafe { &*start.add(i * PAGE).cast::<std::sync::atomic::AtomicU64>() };
                word.fetch_or(0, std::sync::atomic::Ordering::SeqCst);
            }
            unsafe { VirtualUnlock(at, size) };
            return Ok(ZeroRun::Written);
        }
        // Unlock, then -- the pages no longer locked -- `VirtualUnlock` again, which takes them out
        // of the working set (and reports ERROR_NOT_LOCKED, as documented for exactly this use).
        unsafe {
            VirtualUnlock(at, size);
            VirtualUnlock(at, size);
        }
        Ok(ZeroRun::Reset)
    }
}

#[cfg(not(windows))]
mod os {
    use super::{VmError, VmResult, ZeroRun};

    fn unsupported(operation: &'static str) -> VmError {
        VmError::Unsupported { operation, platform: std::env::consts::OS }
    }

    pub(super) fn resident_pages(_start: usize, _pages: usize) -> VmResult<Vec<bool>> {
        Err(unsupported("resident_pages"))
    }

    pub(super) unsafe fn page_is_zero(page: *const u8) -> bool {
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(page, 4096) }.iter().all(|&b| b == 0)
    }

    pub(super) unsafe fn reset_zero_run(_start: *mut u8, _pages: usize) -> VmResult<ZeroRun> {
        Err(unsupported("reset_zero_run"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::vm::{commit, release, reserve, Protection};

    const PAGE: usize = 4096;

    fn fresh(pages: usize) -> (crate::vm::Reservation, *mut u8) {
        let r = reserve(pages * PAGE, 65536).expect("reserve");
        let p = r.as_ptr();
        // SAFETY: inside the fresh reservation.
        unsafe { commit(p, pages * PAGE, Protection::ReadWrite) }.expect("commit");
        (r, p)
    }

    #[test]
    fn a_zero_page_leaves_the_working_set_and_still_reads_zero() {
        let (r, p) = fresh(16);
        // Touch every page (a write of zero dirties it, as a guest's memset does).
        for i in 0..16 {
            // SAFETY: committed above.
            unsafe { p.add(i * PAGE).write_volatile(0) };
        }
        assert!(resident_pages(p as usize, 16).expect("query").iter().all(|&r| r));
        // SAFETY: committed, private, only this test writes it.
        assert_eq!(unsafe { reset_zero_run(p, 16) }.expect("reset"), ZeroRun::Reset);
        let after = resident_pages(p as usize, 16).expect("query");
        assert!(after.iter().all(|&r| !r), "still resident: {after:?}");
        // SAFETY: still committed.
        assert!((0..16).all(|i| unsafe { page_is_zero(p.add(i * PAGE)) }));
        // A write afterwards is kept like any other.
        // SAFETY: still committed and writable.
        unsafe { p.add(5 * PAGE + 8).write_volatile(0xAB) };
        assert!(resident_pages(p as usize + 5 * PAGE, 1).expect("query")[0]);
        // SAFETY: as above.
        assert_eq!(unsafe { p.add(5 * PAGE + 8).read_volatile() }, 0xAB);
        release(r).expect("release");
    }

    #[test]
    fn a_page_written_since_the_first_look_is_kept_resident_and_intact() {
        let (r, p) = fresh(4);
        // SAFETY: committed above.
        unsafe { p.add(2 * PAGE + 100).write_volatile(7) };
        // The caller thought the run was zero; the check after the reset finds the write.
        // SAFETY: committed, private.
        assert_eq!(unsafe { reset_zero_run(p, 4) }.expect("reset"), ZeroRun::Written);
        // SAFETY: still committed.
        assert_eq!(unsafe { p.add(2 * PAGE + 100).read_volatile() }, 7);
        assert!(resident_pages(p as usize, 4).expect("query").iter().all(|&r| r), "the run was left in the working set");
        release(r).expect("release");
    }

    /// Writers race the reset on every page; every value written is read back. (It cannot force
    /// the memory manager to reuse a frame in the window that matters -- the lock is what rules
    /// that out -- but it runs the protocol against real concurrent stores many thousands of times.)
    #[test]
    fn concurrent_writes_are_never_lost() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        const PAGES: usize = 256;
        let (r, p) = fresh(PAGES);
        let base = p as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let written = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (stop, written) = (Arc::clone(&stop), Arc::clone(&written));
            std::thread::spawn(move || {
                let mut n = 0usize;
                while !stop.load(Ordering::Relaxed) && n < PAGES * 64 {
                    // Page n % PAGES, slot n / PAGES: each slot written once, with a non-zero value.
                    let at = base + (n % PAGES) * PAGE + (n / PAGES) * 8;
                    // SAFETY: inside the committed run.
                    unsafe { (at as *mut u64).write_volatile(n as u64 + 1) };
                    n += 1;
                    if n % 97 == 0 {
                        std::thread::yield_now();
                    }
                }
                written.store(n, Ordering::SeqCst);
            })
        };
        let mut resets = 0;
        for _ in 0..200 {
            for i in 0..PAGES {
                let page = (base + i * PAGE) as *mut u8;
                // SAFETY: committed.
                if unsafe { page_is_zero(page) } {
                    // SAFETY: committed, private; the writer only stores.
                    if unsafe { reset_zero_run(page, 1) }.expect("reset") == ZeroRun::Reset {
                        resets += 1;
                    }
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer");
        let n = written.load(Ordering::SeqCst);
        for k in 0..n {
            let at = base + (k % PAGES) * PAGE + (k / PAGES) * 8;
            // SAFETY: committed.
            let v = unsafe { (at as *const u64).read_volatile() };
            assert_eq!(v, k as u64 + 1, "write {k} lost");
        }
        assert!(resets > 0, "the race was never run");
        release(r).expect("release");
    }
}
