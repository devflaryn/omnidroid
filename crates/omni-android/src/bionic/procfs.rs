//! `/proc/meminfo` and `/proc/self/statm`: the two `/proc` files the engine reads its memory from,
//! generated from facts rather than looked for under the root.
//!
//! # MEASURED: who reads them, how, and which fields
//!
//! Every gate run before this logged, many times a second on several threads,
//! `Failed to open /proc/meminfo` and `Failed to open /proc/self/statm`. The readers, decoded from
//! `libroblox.so` (link addresses):
//!
//! * **`/proc/self/statm`, at `0x2282674`** (34 call sites). Opens the file **once** with
//!   `__open_2(path, O_RDONLY)`, keeps the descriptor in a global (`0x6826354`) under a mutex, and
//!   for every reading does `pread(fd, buf, 63, 0)`, NUL-terminates, and
//!   `sscanf(buf, "%zu %zu %zu", &size, &resident, &shared)`. Fewer than three conversions logs
//!   `Failed to scan size, resident, and shared` and closes the descriptor. It returns
//!   **`resident * sysconf(_SC_PAGESIZE)`** -- the resident set in bytes -- and nothing else.
//!   `0x22b2fb0` turns that into headroom: `max(0, limit_MiB << 20 - resident)` whenever a limit
//!   (a flag at `0x6eda910`) is set.
//! * **`/proc/meminfo`, at `0x228296c`.** The same shape: opened once (`0x6826350`), then
//!   `pread(fd, buf, 4095, 0)` per reading. The parse is by name, line by line: `strchr(line,
//!   ':')`, the name NUL-terminated there and skipped if it is 16 bytes or longer, `strncmp(name,
//!   key, 16)` against **`MemTotal`, `MemFree`, `Buffers`, `Cached`, `MemAvailable`, `SwapTotal`
//!   and `SwapFree`**, and the value `strtoul(colon + 1, NULL, 10)` -- which skips the padding and
//!   stops at ` kB`. A name it does not know is skipped; a name that is absent reads as 0. Its
//!   callers: `0x2282900` answers "available" as `MemAvailable` when the line is present and
//!   `MemFree + Buffers + Cached` when it is not, `0x2282f70` and `0x2282fc4` answer `SwapTotal`
//!   and `SwapFree`, and `0x23e5d60` copies all seven. Each is `kB << 10`.
//!
//! Both readers keep the descriptor and `pread` at offset 0 for each new reading, which is why
//! these are *generated* files in `omni-platform`'s sense rather than snapshots: see
//! `omni_platform::fs::Filesystem::serve_generated`, which regenerates a file on every read that
//! starts at 0 and on no other.
//!
//! # `/proc/meminfo` is the embedding's machine, which is what `sysinfo` already says
//!
//! **Linux fills `sysinfo(2)` from the same accounting `/proc/meminfo` prints**: `totalram` is
//! `MemTotal`, `freeram` is `MemFree`, `bufferram` is `Buffers`, `totalswap` and `freeswap` are
//! `SwapTotal` and `SwapFree`. This layer already answers `sysinfo` -- and
//! `sysconf(_SC_PHYS_PAGES)` -- from
//! [`Bionic::set_memory_budget`](super::Bionic::set_memory_budget) and this process's
//! commit charge, and deliberately **not** from the host's RAM (`procenv`'s documentation, and the
//! mutation row `procenv-A2`, which pins that the host's physical memory is the wrong answer in
//! the one field a guest sizes caches from). So this file says the same things, and a guest that
//! asks both ways is told one story:
//!
//! | line | value | why it is a fact |
//! |---|---|---|
//! | `MemTotal` | the budget, in whole pages | the embedding's statement, with no default -- the same number as `sysinfo.totalram` and `_SC_PHYS_PAGES` |
//! | `MemFree` | the budget less this process's commit charge, in whole pages, never below 0 | `sysinfo.freeram`'s computation: D10's commit charge is the quantity that measures memory taken |
//! | `MemAvailable` | `MemFree` | available is free plus what the kernel could reclaim, and this guest's machine has no page cache or reclaimable slab to add |
//! | `Buffers`, `Cached` | 0 | `sysinfo.bufferram` is 0 because this guest has no page cache; `Cached` is that page cache |
//! | `SwapTotal`, `SwapFree` | 0 | `sysinfo.totalswap` and `freeswap`: this guest has no swap |
//!
//! Every other `/proc/meminfo` line (`Active`, `Slab`, `Committed_AS`, ...) is **omitted**: nothing
//! here measures it, and the engine's parser skips what it does not ask for. Absent is the honest
//! spelling of "not measured"; a zero would be a claim.
//!
//! No budget set is a **refusal**, naming `Bionic::set_memory_budget`, at the `open` -- the answer
//! `sysinfo` and `_SC_PHYS_PAGES` give, for the same reason.
//!
//! # `/proc/self/statm` is this process, as the host measures it
//!
//! `/proc/self` is the process `getpid` names, and that is the **host** process: several guest
//! instances share it, as threads of one Android process share one (`omni_platform::process::pid`
//! and `cpu_time` make the same decision). So each field is this process's own, from
//! [`omni_mem::process_memory`], in the guest's page size -- which is 4096, `AT_PAGESZ`:
//!
//! | field | value | Linux's definition it answers |
//! |---|---|---|
//! | `size` | address space in use: reserved, committed or mapped | `total_vm`, every mapping including `PROT_NONE` ones |
//! | `resident` | the working set | resident pages -- **the one field the engine uses** |
//! | `shared` | the shareable part of the working set: image, file and section pages no write privatised | `MM_FILEPAGES + MM_SHMEMPAGES` |
//! | `text` | the executable image's executable sections, page-aligned span | `PAGE_ALIGN(end_code) - (start_code & PAGE_MASK)` |
//! | `lib` | 0 | always 0 since Linux 2.6: `proc_pid_statm` prints a literal 0 |
//! | `data` | the commit charge | `data_vm + stack_vm`: private writable mappings, which are exactly what Linux charges against its commit limit (`accountable_mapping`); Windows' commit charge is that accounting's own measurement |
//! | `dt` | 0 | always 0 since Linux 2.6, as `lib` |
//!
//! **Why the working set and not the commit charge for `resident`**, since the commit charge is
//! the number this project budgets against: the field is named for residency, and the two differ by
//! exactly the thing residency means -- D10 measured 1024 MB committed and untouched at a 4.68 MB
//! working set. The commit charge *is* served, as `data`, whose definition it matches.
//!
//! **Why not the guest's own address space** (`GuestSpace`'s committed bytes): that is one guest
//! instance's accounting, and `/proc/self` is the process. It would also leave out the resident
//! file pages -- the engine's own text among them -- that `resident` counts on a device.
//!
//! **The seam between the two files, stated rather than hidden.** `meminfo` describes the
//! embedding's machine and `statm` this process as the host sees it, so a guest that compared
//! `statm`'s `shared` pages with `meminfo`'s `Cached: 0` would find resident file pages on a
//! machine with no page cache. Neither number is invented; they are answers about two different things,
//! and this layer has no page cache of the guest's to put in `Cached`.

use std::fmt::Write as _;
use std::sync::Arc;

use omni_mem::ProcessMemory;
use omni_platform::fs::{Filesystem, FsError, FsResult, Generator};
use parking_lot::Mutex;

/// The one `/proc/meminfo` there is.
pub(super) const MEMINFO: &str = "/proc/meminfo";
/// This process's `statm`, by the name the engine opens it with.
///
/// Only `/proc/self/...`: `/proc/<pid>/statm` names the same file on a device, and nothing has
/// asked for it by number.
pub(super) const STATM: &str = "/proc/self/statm";

/// Serve both files from `fs`, reading the budget from `budget` each time `/proc/meminfo` is
/// generated -- so a budget set after the root, or changed while the guest runs, is what the next
/// reading says.
///
/// # Errors
///
/// As `Filesystem::serve_generated`: refused if either path is already served.
pub(super) fn serve(
    fs: &Filesystem,
    budget: Arc<Mutex<Option<u64>>>,
    page: usize,
) -> FsResult<()> {
    let page = page as u64;
    let meminfo: Arc<Generator> = Arc::new(move || generate_meminfo(&budget, page));
    fs.serve_generated(MEMINFO.as_bytes(), meminfo)?;
    let statm: Arc<Generator> = Arc::new(move || generate_statm(page));
    fs.serve_generated(STATM.as_bytes(), statm)
}

fn refused(path: &str, why: impl Into<String>) -> FsError {
    FsError::Refused { operation: "generate", path: path.to_string(), why: why.into() }
}

fn generate_meminfo(budget: &Mutex<Option<u64>>, page: u64) -> FsResult<Vec<u8>> {
    // Copied out rather than held: the lock is the embedding's setter's too, and nothing below
    // needs it.
    let held = *budget.lock();
    let Some(total) = held else {
        return Err(refused(
            MEMINFO,
            "the guest opened /proc/meminfo, and nothing has told this instance how much memory \
             the guest has. MemTotal is the embedding's budget -- the number `sysinfo` reports as \
             `totalram` and `sysconf(_SC_PHYS_PAGES)` as pages -- and the host's physical memory \
             is the wrong number in the one field a guest sizes a cache from. Call \
             Bionic::set_memory_budget",
        ));
    };
    let charged = omni_mem::process_commit_charge().map_err(|error| {
        refused(
            MEMINFO,
            format!(
                "this process's commit charge, which MemFree is the budget less, could not be \
                 read: {error}. Reporting the whole budget as free would be a believable number \
                 with nothing behind it"
            ),
        )
    })?;
    Ok(Meminfo::of(total, charged, page).render().into_bytes())
}

fn generate_statm(page: u64) -> FsResult<Vec<u8>> {
    let memory = omni_mem::process_memory().map_err(|error| {
        refused(
            STATM,
            format!("this process's memory counters could not be read: {error}"),
        )
    })?;
    Ok(Statm::of(&memory, page).render().into_bytes())
}

/// The `/proc/meminfo` lines this layer serves, in kB. See the module documentation for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Meminfo {
    pub(super) mem_total: u64,
    pub(super) mem_free: u64,
    pub(super) mem_available: u64,
    pub(super) buffers: u64,
    pub(super) cached: u64,
    pub(super) swap_total: u64,
    pub(super) swap_free: u64,
}

impl Meminfo {
    /// The lines for a budget of `total` bytes with `charged` bytes of it taken.
    ///
    /// **In whole pages**, as Linux's are -- it prints `pages << (PAGE_SHIFT - 10)` -- so every
    /// value is a multiple of the page in kB, and `MemTotal` agrees with `_SC_PHYS_PAGES`, which
    /// also rounds the budget down to whole pages.
    pub(super) fn of(total: u64, charged: u64, page: u64) -> Meminfo {
        let kb = |bytes: u64| bytes / page * page / 1024;
        // Saturating for `sysinfo`'s reason: the charge is the whole process's, and a budget set
        // below what is already taken reports nothing free rather than wrapping to sixteen
        // exabytes.
        let free = kb(total.saturating_sub(charged));
        Meminfo {
            mem_total: kb(total),
            mem_free: free,
            mem_available: free,
            buffers: 0,
            cached: 0,
            swap_total: 0,
            swap_free: 0,
        }
    }

    /// The lines in the order Linux prints them.
    pub(super) fn lines(&self) -> [(&'static str, u64); 7] {
        [
            ("MemTotal", self.mem_total),
            ("MemFree", self.mem_free),
            ("MemAvailable", self.mem_available),
            ("Buffers", self.buffers),
            ("Cached", self.cached),
            ("SwapTotal", self.swap_total),
            ("SwapFree", self.swap_free),
        ]
    }

    /// The file's text, byte for byte in Linux's format.
    ///
    /// `fs/proc/meminfo.c`'s `show_val_kb`: the name and its colon padded to **16** columns
    /// (`"MemTotal:       "`), the value right-aligned in **8**
    /// (`seq_put_decimal_ull_width(.., 8)`, wider values simply widen), then `" kB\n"`.
    pub(super) fn render(&self) -> String {
        let mut text = String::new();
        for (name, kb) in self.lines() {
            let label = format!("{name}:");
            // Writing to a `String` cannot fail.
            let _ = writeln!(text, "{label:<16}{kb:>8} kB");
        }
        text
    }
}

/// The seven `/proc/self/statm` fields, in pages. See the module documentation for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Statm {
    pub(super) size: u64,
    pub(super) resident: u64,
    pub(super) shared: u64,
    pub(super) text: u64,
    pub(super) lib: u64,
    pub(super) data: u64,
    pub(super) dt: u64,
}

impl Statm {
    /// The fields for `memory`, in pages of `page` bytes, rounded down as Linux's page counts are
    /// whole pages -- except `text`, which is Linux's page-aligned span of the code.
    pub(super) fn of(memory: &ProcessMemory, page: u64) -> Statm {
        let pages = |bytes: u64| bytes / page;
        let code_start = memory.executable_code.start as u64 / page * page;
        let code_end = (memory.executable_code.end as u64).div_ceil(page).saturating_mul(page);
        Statm {
            size: pages(memory.address_space),
            resident: pages(memory.resident),
            shared: pages(memory.resident_shared),
            text: code_end.saturating_sub(code_start) / page,
            lib: 0,
            data: pages(memory.commit_charge),
            dt: 0,
        }
    }

    /// The file's text: `proc_pid_statm`'s seven decimals, one space apart, and a newline.
    pub(super) fn render(&self) -> String {
        format!(
            "{} {} {} {} {} {} {}\n",
            self.size, self.resident, self.shared, self.text, self.lib, self.data, self.dt
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: u64 = 4096;
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    /// **Linux's exact bytes**, against a transcription of a real device's `/proc/meminfo` lines
    /// rather than against this file's own format string.
    #[test]
    fn meminfo_is_linuxs_format_to_the_column() {
        let lines = Meminfo {
            mem_total: 7_869_916,
            mem_free: 166_548,
            mem_available: 5_519_300,
            buffers: 7_924,
            cached: 1_519_292,
            swap_total: 2_097_148,
            swap_free: 1_561_112,
        };
        assert_eq!(
            lines.render(),
            "MemTotal:        7869916 kB\n\
             MemFree:          166548 kB\n\
             MemAvailable:    5519300 kB\n\
             Buffers:            7924 kB\n\
             Cached:          1519292 kB\n\
             SwapTotal:       2097148 kB\n\
             SwapFree:        1561112 kB\n"
        );
        // A value wider than eight columns widens the line rather than being cut, as
        // `seq_put_decimal_ull_width` does.
        let wide = Meminfo { mem_total: 123_456_789_012, ..lines };
        let text = wide.render();
        assert!(text.starts_with("MemTotal:       123456789012 kB\n"), "{text}");
    }

    /// **The numbers are the budget and the budget less the charge, in whole pages** -- and not
    /// the host's memory, which is the over-correction a "true of this host" reading invites.
    #[test]
    fn meminfo_is_the_budget_and_what_the_commit_charge_leaves_of_it() {
        let lines = Meminfo::of(2 * GIB, 300 * MIB + 5, PAGE);
        assert_eq!(lines.mem_total, 2 * GIB / 1024, "MemTotal is the budget");
        // 2 GiB - (300 MiB + 5 bytes) is one page short of a whole number of pages: rounded down.
        assert_eq!(lines.mem_free, (2 * GIB - 300 * MIB) / 1024 - PAGE / 1024);
        assert_eq!(lines.mem_available, lines.mem_free, "no page cache to reclaim");
        assert_eq!(
            (lines.buffers, lines.cached, lines.swap_total, lines.swap_free),
            (0, 0, 0, 0),
            "sysinfo's zeros"
        );
        // Whole pages: every value is a multiple of the page in kB.
        for (name, kb) in lines.lines() {
            assert_eq!(kb % (PAGE / 1024), 0, "{name} is not a whole number of pages");
        }
        // A charge past the budget leaves nothing free rather than wrapping.
        let over = Meminfo::of(GIB, 3 * GIB, PAGE);
        assert_eq!((over.mem_total, over.mem_free), (GIB / 1024, 0));
    }

    /// **Seven fields, the two Linux fixes at zero, each the quantity it names, in pages.**
    #[test]
    fn statm_is_seven_page_counts_from_this_process() {
        let base = 0x7ff6_0000_0000usize;
        let memory = ProcessMemory {
            address_space: 4 * GIB,
            resident: 200 * MIB,
            resident_shared: 30 * MIB,
            commit_charge: 150 * MIB,
            // A code span that starts and ends mid-page: `text` is the page-aligned span.
            executable_code: base + 0x1000 + 0x10..base + 0x9_f4a5,
        };
        let fields = Statm::of(&memory, PAGE);
        assert_eq!(fields.size, 4 * GIB / PAGE);
        assert_eq!(fields.resident, 200 * MIB / PAGE, "resident is the working set");
        assert_eq!(fields.shared, 30 * MIB / PAGE);
        assert_eq!(fields.data, 150 * MIB / PAGE, "data is the commit charge");
        // From the page holding 0x1010 to the end of the page holding 0x9f4a4: 0x1000..0xa0000.
        assert_eq!(fields.text, (0xa_0000 - 0x1000) / PAGE);
        assert_eq!((fields.lib, fields.dt), (0, 0), "Linux has printed 0 for both since 2.6");
        // 4 GiB, 200 MiB, 30 MiB, 159 pages of code, 150 MiB, in 4 KiB pages.
        assert_eq!(fields.render(), "1048576 51200 7680 159 0 38400 0\n");
    }
}
