//! **What this process's memory is, region by region**: the census behind a memory report.
//!
//! [`super::process_memory`] gives the totals -- commit charge, resident, shareable -- and nothing
//! about *whose* they are. A process running a guest holds the guest's address space, the
//! translator's code caches, host thread stacks, heaps, DLL images and driver mappings side by
//! side, and a total cannot say which of them grew. This is the walk that can:
//!
//! * [`process_regions`](super::process_regions) lists every region of the address space the OS
//!   has, with what kind of memory it is, how much of it is committed and how much is resident;
//! * [`resident_set`](super::resident_set) answers "how much of `[start, start + len)` is in RAM,
//!   and how much of that is private" for any range -- finer than an OS region, which is what a
//!   caller with its own map of a range (a guest address space) needs.
//!
//! | | Windows | Linux | macOS |
//! |---|---|---|---|
//! | regions | `VirtualQuery` over the whole user range; names from `K32GetMappedFileNameW` | `/proc/self/smaps` | refused ([`VmError::Unsupported`]) |
//! | committed | `MEM_COMMIT` regions (`PrivateUsage` counts the `MEM_PRIVATE` ones) | the VMA's size when it carries `VM_ACCOUNT` (`ac`) -- what `Committed_AS` is charged | -- |
//! | resident | one `K32QueryWorkingSet`, binned by address | the VMA's `Rss`, private = `Private_Clean + Private_Dirty` | -- |
//! | a range's residency | the same working-set snapshot | `/proc/self/pagemap`: present (bit 63), and not file/shared-anon (bit 61) for private | -- |
//! | thread stacks | an allocation holding a `PAGE_GUARD` page | `[stack]`; the VMA holding a blocked thread's stack pointer (`/proc/self/task/*/syscall`) or the caller's own; or, older glibc, a writable anonymous VMA directly above a `PROT_NONE` one of at most 64 KiB | -- |
//!
//! **A measurement call, never on a hot path.** A walk costs a `VirtualQuery` per region (tens of
//! thousands in a guest's placeholder-split space: tens of milliseconds) and a working-set copy
//! of 8 bytes per resident page; on Linux, the kernel walks every VMA's page tables for `smaps`.
//! It is for a report taken every few minutes, not for a counter.

use super::{VmError, VmResult};

/// What kind of memory a [`HostRegion`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HostRegionKind {
    /// Anonymous memory this process allocated: heaps, stacks, code caches, a guest's committed
    /// pages. Windows `MEM_PRIVATE`; a Linux VMA with no file.
    Private,
    /// A view of a file or of a shared section. Windows `MEM_MAPPED`; a Linux VMA of a file that is
    /// not a loaded library, a memfd, or a device.
    Mapped,
    /// An executable image: the executable and its DLLs / shared libraries. Windows `MEM_IMAGE`;
    /// on Linux a VMA of the executable or of a file whose name has `.so` in it.
    Image,
}

/// How much of a range is in RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Residency {
    /// Bytes resident.
    pub resident: u64,
    /// Bytes of [`resident`](Self::resident) that are this process's own: not shareable with any
    /// other mapping of the same object. Anonymous pages, and file pages a write has copied.
    pub private: u64,
}

impl Residency {
    /// Bytes resident that another mapping of the same file or section could be sharing.
    #[must_use]
    pub fn shareable(&self) -> u64 {
        self.resident.saturating_sub(self.private)
    }

    /// Add `other` into this one.
    pub fn add(&mut self, other: Residency) {
        self.resident += other.resident;
        self.private += other.private;
    }
}

/// One region of this process's address space, as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRegion {
    /// Start address.
    pub start: usize,
    /// Length in bytes.
    pub len: usize,
    /// What kind of memory it is.
    pub kind: HostRegionKind,
    /// Bytes committed. On Windows the whole region when it is `MEM_COMMIT`, else 0 (reserved);
    /// on Linux the VMA's size when it is charged to `Committed_AS` (`VM_ACCOUNT`), else 0.
    pub committed: u64,
    /// Whether the pages are executable.
    pub executable: bool,
    /// Whether the pages are writable (copy-on-write counts).
    pub writable: bool,
    /// Whether the region is, as far as the OS says, part of a thread's stack. See the module
    /// table for the rule on each host; on Linux it is a heuristic.
    pub stack: bool,
    /// The allocation this region is part of: Windows `AllocationBase` (a `VirtualAlloc`
    /// reservation or a view, which `VirtualQuery` reports as several regions when its pages
    /// differ); on Linux the region's own start.
    pub allocation_base: usize,
    /// The file an image or a mapped view comes from -- its last path component -- when the OS
    /// names one; on Linux also an anonymous mapping's own name (`[heap]`, `[stack]`,
    /// `[anon:<name>]`) and the kernel's (`[vdso]`).
    pub name: Option<String>,
    /// How much of it is resident, when the OS could say.
    pub residency: Option<Residency>,
}

impl HostRegion {
    /// End address, exclusive.
    #[must_use]
    pub fn end(&self) -> usize {
        self.start.saturating_add(self.len)
    }
}

/// What this process's C heaps hold: the allocator Rust's `System` and C/C++ `malloc` share.
///
/// Windows: every heap `GetProcessHeaps` lists, each through `HeapSummary` -- the process heap
/// (Rust, the C runtime, and so everything C++ in the process) and any heap a DLL created for
/// itself. Linux: glibc's `mallinfo2`, whose arenas are what `malloc` hands out, Rust's included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeapTotals {
    /// How many heaps (Windows) or 1 (Linux: glibc's arenas, summed).
    pub heaps: usize,
    /// Bytes the heaps hold committed (Windows `cbCommitted`; Linux `arena + hblkhd`: the main
    /// and thread arenas' system bytes plus large blocks served by `mmap`).
    pub committed: u64,
    /// Bytes of that handed out to callers (Windows `cbAllocated`; Linux `uordblks + hblkhd`).
    pub allocated: u64,
}

/// Which pages of this process are resident, for [`ResidentSet::in_range`].
pub struct ResidentSet(Inner);

impl ResidentSet {
    /// How much of `[start, start + len)` is resident, and how much of that is private.
    ///
    /// # Errors
    ///
    /// [`VmError::Os`] if the OS refuses to report it (Linux: reading `/proc/self/pagemap`).
    pub fn in_range(&self, start: usize, len: usize) -> VmResult<Residency> {
        self.0.in_range(start, len)
    }
}

impl core::fmt::Debug for ResidentSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ResidentSet")
    }
}

// ----------------------------------------------------------------------- the page-list shape

/// Resident pages as a sorted list, each with whether it is shareable: the shape a working-set
/// snapshot comes in. Pure, so the binning is tested on every host.
#[derive(Debug, Default)]
// Built on Windows, where the working set arrives as a page list; its unit test runs everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct PageList {
    /// Page addresses, sorted ascending.
    pages: Vec<usize>,
    /// `shared_before[i]` = how many of `pages[..i]` are shareable. One longer than `pages`.
    shared_before: Vec<u32>,
    page: usize,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl PageList {
    /// Build from `(page address, shareable)` pairs in any order.
    pub(crate) fn new(mut entries: Vec<(usize, bool)>, page: usize) -> Self {
        entries.sort_unstable_by_key(|&(address, _)| address);
        entries.dedup_by_key(|&mut (address, _)| address);
        let mut shared_before = Vec::with_capacity(entries.len() + 1);
        let mut shared = 0u32;
        shared_before.push(0);
        for &(_, is_shared) in &entries {
            shared += u32::from(is_shared);
            shared_before.push(shared);
        }
        Self { pages: entries.into_iter().map(|(address, _)| address).collect(), shared_before, page }
    }

    /// The residency of `[start, start + len)`: every listed page whose address lies in it.
    pub(crate) fn in_range(&self, start: usize, len: usize) -> Residency {
        let end = start.saturating_add(len);
        // A page that begins before `start` but reaches into the range counts too: ranges here are
        // page-aligned in practice, and rounding the start down keeps a misaligned one honest.
        let from = start - start % self.page.max(1);
        let lo = self.pages.partition_point(|&p| p < from);
        let hi = self.pages.partition_point(|&p| p < end);
        let pages = (hi - lo) as u64;
        let shared = u64::from(self.shared_before[hi] - self.shared_before[lo]);
        let page = self.page as u64;
        Residency { resident: pages * page, private: (pages - shared) * page }
    }
}

// ----------------------------------------------------------------------------------- Windows

#[cfg(windows)]
mod os {
    use std::collections::HashMap;
    use std::ffi::c_void;

    use windows_sys::Win32::System::Memory::{
        VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_FREE, MEM_IMAGE, MEM_MAPPED,
        PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY,
        PAGE_GUARD, PAGE_READWRITE, PAGE_WRITECOPY,
    };
    use windows_sys::Win32::System::ProcessStatus::{K32GetMappedFileNameW, K32QueryWorkingSet};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    use super::{HostRegion, HostRegionKind, PageList, Residency, VmError, VmResult};
    use crate::vm::OsError;

    const ERROR_BAD_LENGTH: u32 = 24;
    const PAGE: usize = 4096;

    fn os(operation: &'static str) -> VmError {
        // SAFETY: reads this thread's last-error value; no preconditions.
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        VmError::Os { operation, address: 0, size: 0, source: OsError(code) }
    }

    pub(crate) struct Inner(PageList);

    impl Inner {
        pub(crate) fn in_range(&self, start: usize, len: usize) -> VmResult<Residency> {
            Ok(self.0.in_range(start, len))
        }
    }

    /// One `K32QueryWorkingSet`: every resident page, with its `Shared` bit.
    ///
    /// The buffer is `NumberOfEntries` followed by one `PSAPI_WORKING_SET_BLOCK` (a `ULONG_PTR`
    /// of flags) per page: bits 12.. the page's address, bit 8 `Shared`. A buffer that is too
    /// small fails with `ERROR_BAD_LENGTH` and says how many entries there are, which can grow
    /// before the next try -- so it is retried with headroom.
    fn working_set() -> VmResult<PageList> {
        let mut words: usize = 64 * 1024;
        for _ in 0..8 {
            let mut buffer = vec![0usize; words];
            let bytes = u32::try_from(words * core::mem::size_of::<usize>()).map_err(|_| VmError::Os {
                operation: "K32QueryWorkingSet",
                address: 0,
                size: words,
                source: OsError(ERROR_BAD_LENGTH),
            })?;
            // SAFETY: `buffer` is `bytes` bytes, writable, and outlives the call; the pseudo-handle
            // needs no closing.
            let ok = unsafe {
                K32QueryWorkingSet(GetCurrentProcess(), buffer.as_mut_ptr().cast::<c_void>(), bytes)
            };
            let count = buffer[0];
            if ok != 0 && count < words {
                let entries = buffer[1..=count]
                    .iter()
                    .map(|&flags| (flags & !(PAGE - 1), flags & 0x100 != 0))
                    .collect();
                return Ok(PageList::new(entries, PAGE));
            }
            // SAFETY: reads this thread's last-error value.
            let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if ok == 0 && code != ERROR_BAD_LENGTH {
                return Err(os("K32QueryWorkingSet"));
            }
            words = count.max(words) + count / 4 + 4096;
        }
        Err(VmError::Os {
            operation: "K32QueryWorkingSet (the working set kept outgrowing the buffer)",
            address: 0,
            size: words,
            source: OsError(ERROR_BAD_LENGTH),
        })
    }

    pub(crate) fn resident_set() -> VmResult<Inner> {
        Ok(Inner(working_set()?))
    }

    pub(crate) fn heap_totals() -> VmResult<super::HeapTotals> {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Memory::{GetProcessHeaps, HeapSummary, HEAP_SUMMARY};
        let mut heaps: Vec<HANDLE> = vec![core::ptr::null_mut(); 256];
        // SAFETY: `heaps` has room for the count passed; the call writes at most that many.
        let count = unsafe { GetProcessHeaps(heaps.len() as u32, heaps.as_mut_ptr()) } as usize;
        if count == 0 {
            return Err(os("GetProcessHeaps"));
        }
        // More heaps than room: the first 256 are summed and the count says how many there are.
        let mut totals = super::HeapTotals { heaps: count, ..Default::default() };
        for &heap in &heaps[..count.min(heaps.len())] {
            let mut summary =
                HEAP_SUMMARY { cb: core::mem::size_of::<HEAP_SUMMARY>() as u32, ..Default::default() };
            // SAFETY: `heap` came from GetProcessHeaps and `summary` is a live HEAP_SUMMARY whose
            // `cb` states its size. A heap destroyed in between fails the call, which is skipped.
            if unsafe { HeapSummary(heap, 0, &mut summary) } != 0 {
                totals.committed += summary.cbCommitted as u64;
                totals.allocated += summary.cbAllocated as u64;
            }
        }
        Ok(totals)
    }

    /// The last component of the file a view or image at `address` maps, if the OS names one.
    fn mapped_name(address: usize) -> Option<String> {
        let mut buffer = [0u16; 1024];
        // SAFETY: `buffer` is writable for its length; the call writes at most that many units.
        let written = unsafe {
            K32GetMappedFileNameW(
                GetCurrentProcess(),
                address as *const c_void,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
        };
        if written == 0 {
            return None;
        }
        let full = String::from_utf16_lossy(&buffer[..written as usize]);
        Some(full.rsplit('\\').next().unwrap_or(&full).to_string())
    }

    pub(crate) fn process_regions() -> VmResult<Vec<HostRegion>> {
        let pages = working_set()?;
        let mut out: Vec<HostRegion> = Vec::new();
        let mut names: HashMap<usize, Option<String>> = HashMap::new();
        let mut address: usize = 0;
        loop {
            let mut info = MEMORY_BASIC_INFORMATION::default();
            // SAFETY: `info` is a live MEMORY_BASIC_INFORMATION of the size passed; VirtualQuery
            // reads no memory at `address`, it only describes it.
            let got = unsafe {
                VirtualQuery(
                    address as *const c_void,
                    &mut info,
                    core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                )
            };
            if got == 0 {
                // Past the highest user address: ERROR_INVALID_PARAMETER ends the walk.
                break;
            }
            let start = info.BaseAddress as usize;
            let len = info.RegionSize;
            if info.State != MEM_FREE {
                let committed = info.State == MEM_COMMIT;
                // `Protect` is only meaningful for committed pages.
                let protect = if committed { info.Protect } else { 0 };
                let kind = match info.Type {
                    MEM_IMAGE => HostRegionKind::Image,
                    MEM_MAPPED => HostRegionKind::Mapped,
                    _ => HostRegionKind::Private,
                };
                let allocation_base = info.AllocationBase as usize;
                let name = match kind {
                    HostRegionKind::Private => None,
                    _ => names.entry(allocation_base).or_insert_with(|| mapped_name(start)).clone(),
                };
                out.push(HostRegion {
                    start,
                    len,
                    kind,
                    committed: if committed { len as u64 } else { 0 },
                    executable: protect
                        & (PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
                        != 0,
                    writable: protect
                        & (PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)
                        != 0,
                    // Marked per allocation below: the guard page is one region of the stack's
                    // allocation, and every region of it is the stack.
                    stack: protect & PAGE_GUARD != 0,
                    allocation_base,
                    name,
                    residency: Some(if committed { pages.in_range(start, len) } else { Residency::default() }),
                });
            }
            match start.checked_add(len) {
                Some(next) if next > address => address = next,
                _ => break,
            }
        }
        let stacks: std::collections::HashSet<usize> =
            out.iter().filter(|r| r.stack).map(|r| r.allocation_base).collect();
        for region in &mut out {
            region.stack = region.kind == HostRegionKind::Private && stacks.contains(&region.allocation_base);
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------------------------- Linux

#[cfg(target_os = "linux")]
mod os {
    use std::fs::File;
    use std::os::unix::fs::FileExt;

    use super::{HostRegion, HostRegionKind, Residency, VmError, VmResult};
    use crate::vm::OsError;

    /// The largest `PROT_NONE` VMA directly below a writable anonymous one that is taken for a
    /// thread stack's guard: glibc's default guard is one page, and a caller may ask for more.
    const GUARD_MAX: usize = 64 * 1024;

    fn io(operation: &'static str, error: &std::io::Error) -> VmError {
        VmError::Os {
            operation,
            address: 0,
            size: 0,
            source: OsError(error.raw_os_error().unwrap_or(libc::EPROTO) as u32),
        }
    }

    pub(crate) struct Inner {
        pagemap: File,
        page: usize,
    }

    impl Inner {
        /// Read the range's pagemap entries: bit 63 present, bit 61 a file page or shared
        /// anonymous page. Present and not bit 61 is private.
        pub(crate) fn in_range(&self, start: usize, len: usize) -> VmResult<Residency> {
            const CHUNK: usize = 16 * 1024;
            let page = self.page;
            let first = start / page;
            let last = start.saturating_add(len).div_ceil(page);
            let mut out = Residency::default();
            let mut buffer = vec![0u8; CHUNK * 8];
            let mut at = first;
            while at < last {
                let n = (last - at).min(CHUNK);
                let bytes = &mut buffer[..n * 8];
                self.pagemap
                    .read_exact_at(bytes, (at * 8) as u64)
                    .map_err(|e| io("/proc/self/pagemap", &e))?;
                for entry in bytes.chunks_exact(8) {
                    let value = u64::from_le_bytes(entry.try_into().expect("eight bytes"));
                    if value >> 63 & 1 == 1 {
                        out.resident += page as u64;
                        if value >> 61 & 1 == 0 {
                            out.private += page as u64;
                        }
                    }
                }
                at += n;
            }
            Ok(out)
        }
    }

    pub(crate) fn resident_set() -> VmResult<Inner> {
        let pagemap = File::open("/proc/self/pagemap").map_err(|e| io("/proc/self/pagemap", &e))?;
        Ok(Inner { pagemap, page: crate::vm::page_size() })
    }

    pub(crate) fn heap_totals() -> VmResult<super::HeapTotals> {
        // SAFETY: mallinfo2 takes no arguments and returns plain data.
        let info = unsafe { libc::mallinfo2() };
        Ok(super::HeapTotals {
            heaps: 1,
            committed: (info.arena + info.hblkhd) as u64,
            allocated: (info.uordblks + info.hblkhd) as u64,
        })
    }

    /// `start-end perms offset dev inode [path]`.
    fn header(line: &str) -> Option<(usize, usize, &str, &str)> {
        let mut fields = line.splitn(6, ' ');
        let range = fields.next()?;
        let perms = fields.next()?;
        let (_offset, _dev, _inode) = (fields.next()?, fields.next()?, fields.next()?);
        let path = fields.next().unwrap_or("").trim();
        let (start, end) = range.split_once('-')?;
        let start = usize::from_str_radix(start, 16).ok()?;
        let end = usize::from_str_radix(end, 16).ok()?;
        (perms.len() == 4 && end >= start).then_some((start, end, perms, path))
    }

    fn kilobytes(rest: &str) -> u64 {
        rest.trim().trim_end_matches("kB").trim().parse::<u64>().unwrap_or(0) * 1024
    }

    /// The stack pointer of every thread of this process that is blocked (`/proc/self/task/*/syscall`
    /// ends `<sp> <pc>` unless it says `running`), and the caller's own -- a local's address.
    ///
    /// **Why a thread's stack is found by its stack pointer**: nothing in `smaps` marks a thread
    /// stack. The main thread's is `[stack]`, and glibc used to leave a `PROT_NONE` guard VMA under
    /// each thread stack, but MEASURED on the Linux port host (7.0, 2026-09-25) the guard is no
    /// longer a VMA of its own, so a thread stack is a plain `rw-p` anonymous VMA. A running
    /// thread's pointer is not readable: that thread's stack counts as other private memory.
    fn stack_pointers() -> Vec<usize> {
        let local = 0u8;
        let mut out = vec![core::ptr::addr_of!(local) as usize];
        let Ok(tasks) = std::fs::read_dir("/proc/self/task") else { return out };
        for task in tasks.filter_map(Result::ok) {
            let Ok(text) = std::fs::read_to_string(task.path().join("syscall")) else { continue };
            let fields: Vec<&str> = text.split_whitespace().collect();
            if fields.len() >= 3 {
                let sp = fields[fields.len() - 2].trim_start_matches("0x");
                if let Ok(sp) = usize::from_str_radix(sp, 16) {
                    out.push(sp);
                }
            }
        }
        out
    }

    pub(crate) fn process_regions() -> VmResult<Vec<HostRegion>> {
        let text =
            std::fs::read_to_string("/proc/self/smaps").map_err(|e| io("/proc/self/smaps", &e))?;
        let exe = std::fs::read_link("/proc/self/exe").ok().map(|p| p.display().to_string());
        let mut out: Vec<HostRegion> = Vec::new();
        let mut private_bytes = 0u64;
        for line in text.lines() {
            if let Some((start, end, perms, path)) = header(line) {
                // Anonymous: no path, the heap, the main stack, or a named anonymous mapping
                // (`[anon:<name>]`, whose name is kept). The kernel's own `[vdso]`, `[vvar]` and
                // `[vsyscall]` are code and data it maps into every process: images.
                let anonymous = path.is_empty()
                    || path == "[heap]"
                    || path == "[stack]"
                    || path.starts_with("[anon:")
                    || path.starts_with("anon_inode:");
                let kind = if anonymous {
                    HostRegionKind::Private
                } else if path.starts_with('[') || path.contains(".so") || exe.as_deref() == Some(path) {
                    HostRegionKind::Image
                } else {
                    HostRegionKind::Mapped
                };
                let name =
                    (!path.is_empty()).then(|| path.rsplit('/').next().unwrap_or(path).to_string());
                let writable = perms.as_bytes()[1] == b'w';
                // Older glibc's shape, where it still holds: a writable anonymous VMA directly
                // above a small `PROT_NONE` one. The stack pointers below are the rule that works
                // on the port host.
                let stack = path == "[stack]"
                    || (kind == HostRegionKind::Private
                        && writable
                        && out.last().is_some_and(|below| {
                            below.end() == start
                                && below.kind == HostRegionKind::Private
                                && !below.writable
                                && !below.executable
                                && below.len <= GUARD_MAX
                                && below.name.is_none()
                        }));
                out.push(HostRegion {
                    start,
                    len: end - start,
                    kind,
                    committed: 0,
                    executable: perms.as_bytes()[2] == b'x',
                    writable,
                    stack,
                    allocation_base: start,
                    name,
                    residency: Some(Residency::default()),
                });
                private_bytes = 0;
                continue;
            }
            let Some(region) = out.last_mut() else { continue };
            let Some((key, rest)) = line.split_once(':') else { continue };
            match key {
                "Rss" => {
                    if let Some(r) = region.residency.as_mut() {
                        r.resident = kilobytes(rest);
                    }
                }
                "Private_Clean" | "Private_Dirty" => {
                    private_bytes += kilobytes(rest);
                    if let Some(r) = region.residency.as_mut() {
                        r.private = private_bytes;
                    }
                }
                "VmFlags" if rest.split_whitespace().any(|flag| flag == "ac") => {
                    region.committed = region.len as u64;
                }
                _ => {}
            }
        }
        for sp in stack_pointers() {
            let at = out.partition_point(|r| r.end() <= sp);
            if let Some(region) = out.get_mut(at) {
                if region.start <= sp && region.kind == HostRegionKind::Private && region.writable {
                    region.stack = true;
                }
            }
        }
        Ok(out)
    }
}

// --------------------------------------------------------------------------- everywhere else

#[cfg(not(any(windows, target_os = "linux")))]
mod os {
    use super::{HostRegion, Residency, VmError, VmResult};

    pub(crate) struct Inner;

    impl Inner {
        pub(crate) fn in_range(&self, _start: usize, _len: usize) -> VmResult<Residency> {
            Err(unsupported("ResidentSet::in_range"))
        }
    }

    fn unsupported(operation: &'static str) -> VmError {
        VmError::Unsupported { operation, platform: std::env::consts::OS }
    }

    pub(crate) fn resident_set() -> VmResult<Inner> {
        Err(unsupported("resident_set"))
    }

    pub(crate) fn heap_totals() -> VmResult<super::HeapTotals> {
        Err(unsupported("heap_totals"))
    }

    pub(crate) fn process_regions() -> VmResult<Vec<HostRegion>> {
        Err(unsupported("process_regions"))
    }
}

use os::Inner;

pub(crate) fn resident_set() -> VmResult<ResidentSet> {
    os::resident_set().map(ResidentSet)
}

pub(crate) fn process_regions() -> VmResult<Vec<HostRegion>> {
    os::process_regions()
}

pub(crate) fn heap_totals() -> VmResult<HeapTotals> {
    os::heap_totals()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_list_bins_resident_and_private_pages_by_address() {
        let page = 4096;
        let list = PageList::new(
            vec![(0x3000, false), (0x1000, true), (0x2000, false), (0x2000, false), (0x9000, true)],
            page,
        );
        // The duplicate 0x2000 counts once; order in does not matter.
        assert_eq!(list.in_range(0, 0x10000), Residency { resident: 4 * 4096, private: 2 * 4096 });
        assert_eq!(list.in_range(0x1000, 0x2000), Residency { resident: 2 * 4096, private: 4096 });
        assert_eq!(list.in_range(0x4000, 0x5000), Residency::default(), "nothing resident there");
        assert_eq!(list.in_range(0x9000, 0x1000), Residency { resident: 4096, private: 0 });
        // A misaligned start still counts the page it begins in.
        assert_eq!(list.in_range(0x3800, 0x10), Residency { resident: 4096, private: 4096 });
        assert_eq!(Residency { resident: 10, private: 4 }.shareable(), 6);
    }
}
