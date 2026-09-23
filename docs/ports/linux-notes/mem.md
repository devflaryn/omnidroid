# Linux port: memory (`vm`) and guest faults (`fault`)

Worker: memory, branch `lnx-mem`. Host: Ubuntu 26.04, Linux 7.0.0-30, i5-4460 (4 cores, AVX2, no
AVX-512, no LA57: 47-bit user address space), 7 GB RAM, `vm.overcommit_memory = 0`,
`vm.max_map_count = 1048576`, `vm.memfd_noexec = 0`. The owner's GNOME session was live during every
measurement, so timings are from a busy desktop, not a quiet machine. Every timing below is from a
`--release` build. Nothing here is graphics.

## What is implemented

| file | what |
|---|---|
| `crates/omni-platform/src/vm/linux.rs` | the whole vm seam on Linux, with the ledger (below) |
| `crates/omni-platform/src/vm/unix.rs` | macOS's structural seam unchanged; new `posix` module: `mmap`/`mprotect`/`munmap`/`msync`/`fstat`/`fcntl` wrappers, pure POSIX |
| `crates/omni-platform/src/fault/linux.rs` | `SIGSEGV`/`SIGBUS` backend, first place re-asserted over dynarmic, the chain |
| `crates/omni-platform/src/fault/mod.rs` | (shared, additive) Linux selection, two `FaultError` variants, `reassert_precedence` |
| `crates/omni-mem/src/pager.rs` | (shared, additive) `DemandPager::reassert_precedence` pass-through |
| `crates/omni-cpu/src/dynarmic/mod.rs` | (shared, additive) call it after every `od_jit_new` |

### The seam's words on Linux, decided by measurement

| seam | Linux call | why (measured: a C probe first, then the `tests/*_linux` suites) |
|---|---|---|
| reserve / reserve_placeholder | `mmap(PROT_NONE, MAP_PRIVATE\|MAP_ANONYMOUS)` -- **not** `MAP_NORESERVE` | see "MAP_NORESERVE" below |
| commit (plain) | `mprotect` | charged at the call (`VM_ACCOUNT`), contents kept on re-commit (a racing fault handler relies on that) |
| commit_placeholder | `mmap(MAP_FIXED, MAP_PRIVATE\|MAP_ANONYMOUS)` | charged at the call, zero-filled |
| decommit / decommit_to_placeholder / unmap | `mmap(MAP_FIXED, PROT_NONE)` over the range | the only call that returns RSS **and** `VM_ACCOUNT` |
| split / coalesce placeholders | no kernel call; ledger only | `MAP_FIXED` replaces any page-aligned sub-range |
| map_file | `mmap(MAP_FIXED, MAP_PRIVATE)`; `MAP_SHARED` for `share_file_for_mapping` | a writable private view is charged in full at map time, as `PAGE_WRITECOPY` is |
| open_file_for_mapping(Executable) | `open(O_RDONLY)` + a one-page `PROT_EXEC` probe mapping | D11: execute is decided at open; Linux has no open-time execute right, so the kernel is asked |
| share_file_for_mapping | keep the fd; refuse unless `O_RDWR` (`EACCES`) | Windows refuses at section creation; Linux would refuse later, at map/protect |
| create_shared_section / map_section | `memfd_create(MFD_CLOEXEC\|MFD_EXEC)` + `mmap(MAP_SHARED)` x2 | D12; neither view is `VM_ACCOUNT`; memfd pages charged to `Committed_AS` on first touch |
| process_commit_charge | sum of VMAs with `ac` in `/proc/self/smaps` VmFlags | exactly this process's own contribution to `Committed_AS` |
| process_working_set | `/proc/self/statm` resident (= `VmRSS`) | |
| process_memory | `/proc/self/statm` size/resident/shared + the `ac` sum + `/proc/self/stat` start_code/end_code | one statm read, so `shared <= resident` |
| placeholder_api_available | `true`; symbols: empty | nothing is resolved at runtime |

**MAP_NORESERVE, measured and rejected.** 16 GiB `PROT_NONE` reserved: `Committed_AS` +0 kB with or
without it (a `PROT_NONE` private mapping is not accountable either way: `accountable_mapping()`
needs `VM_WRITE`). 64 MiB then `mprotect`ed read-write: **+65,536 kB** of `Committed_AS` and of `ac`
VMAs without it, **+0 kB** with it (`mprotect_fixup()` skips accounting for a `VM_NORESERVE` VMA).
So `MAP_NORESERVE` buys nothing for the reservation and switches off D10's central asymmetry
(commit is charged at commit). Under `vm.overcommit_memory = 2` the kernel ignores the flag and the
two agree (from `mm/mmap.c`; not measured, needs root). Mutation row `lnx-vm-B1` is that flag.

**Decommit, measured** (64 MiB touched, C probe, n = 1, deterministic kernel counters):

| primitive | RSS | `Committed_AS` |
|---|---|---|
| `madvise(MADV_DONTNEED)` | -65,536 kB | **0** |
| then `mprotect(PROT_NONE)` | 0 | **0** |
| `mmap(MAP_FIXED, PROT_NONE)` over it | -65,536 kB | **-65,536 kB** |

`MADV_DONTNEED` is Linux's `MEM_RESET` trap. (`mprotect(PROT_NONE)` does drop `VM_ACCOUNT`, but only
from a VMA never touched -- `!vma->anon_vma` -- where there was nothing to return.) Rows
`lnx-vm-A1`/`A2`.

### The ledger

The kernel refuses none of the calls Windows refuses. Worst case: `munmap` of an address released a
moment ago succeeds and unmaps whatever the kernel handed out there since. So `vm/linux.rs` keeps a
process-wide `Mutex<BTreeMap>` of every range the seam has handed out and what it is (plain
reservation / placeholder / private commit / file view / section view), and checks each call first:
exact-size placeholder for `commit_placeholder` and `map_file` (`PlaceholderNotExactSize`), whole
allocation for `release` (`ReleaseExtentMismatch`, double release refused), whole view for `unmap`
(`NotViewBase` / `ViewSizeMismatch`), `commit` only into a plain reservation, `protect` never on a
placeholder, and `PROT_EXEC` refused (`EACCES`) on a view of a file opened `NonExecutable`. Ledger
refusals carry `EINVAL`. It is never stricter than Windows on anything `omni-mem` does (the region
map is written against Windows); where it is more permissive it says so in the code (a split or
coalesce of a single placeholder succeeds; `decommit_to_placeholder` may span adjacent private
pieces).

Deliberately **not** reproduced: Windows refuses to raise a view *created* read-only to r-x even
from an executable section (87). Linux allows it and nothing relies on the refusal. And
`protect` on an uncommitted page of a *plain* reservation commits it on Linux (the ledger does not
track commit inside plain reservations; only tests use them -- `GuestSpace` uses placeholders,
which it does track).

Not defended, on either backend: a stale `Reservation` descriptor whose base **and** length were
reissued by the kernel is indistinguishable from the new one (the descriptor has no generation),
so a release through it frees the new one. `tests/vm_linux.rs` pins the case that *is* defended
(a larger reservation laid over the freed one: refused, memory intact).

## Measurements

All in `crates/omni-mem/tests/probe_linux`, `crates/omni-platform/tests/vm_commit_charge_linux.rs`,
`crates/omni-mem/tests/commit_charge_linux`, `crates/omni-elf/tests/loader_commit_linux`,
`crates/omni-elf/tests/cache_sharing_linux`, `crates/omni-mem/tests/pager_linux`,
`crates/omni-cpu/tests/roblox.rs`. Printed with `-- --nocapture`.

### D10 on Linux

| quantity | Linux (this host) | Windows (D10) |
|---|---|---|
| reserve 1 GiB / 16 GiB / 1 TiB / 16 TiB / 64 TiB | 1.88 / 1.85 / 2.17 / 2.12 / 2.17 us per call, **0 B** commit, 0 B resident (median, n = 31 each) | 0 B |
| largest single reservation | **96.44 TiB** (bisected to 64 GiB; depends on where ASLR put the executable) | 125.57 TB |
| 64 guest spaces x 16 GiB (1 TiB) | **0 B** commit | 240.8 MB (64 processes) |
| commit, one page per call | 1889 ns/page (mprotect + ledger; median of n = 11 runs of 8192 calls) | 284 ns/page |
| commit, 64 MiB in one call | 0.2 ns/page (n = 11) | 3 ns/page |
| decommit, one page per call | 2242 ns/page (n = 11 x 8192) | -- |
| decommit, 64 MiB touched, one call | 195 ns/page (n = 11): the pages are freed here | -- |
| kernel soft fault (first touch of committed page) | **1437 ns/page** (n = 11 x 16384) | 398 ns |
| SIGSEGV demand-pager fault, 4 KiB granule | **7181 ns/fault** (n = 11 x 8192 faults; includes the soft fault) | 2053 ns (VEH) |
| demand paging at the default 64 KiB granule | 28174 ns/fault = **1761 ns per page touched** (n = 11 x 1024 faults / 16384 pages) | -- |
| `ensure_committed` per granule (64 MiB span) | 4 KiB: 3282 ns/page; 16 KiB: 783; **64 KiB: 180**; 256 KiB: 56; 1 MiB: 10 (1 run each) | 2414 / 596 / 150 / 35 / 9 |
| commit is charged at commit, not touch | 64 MiB committed: +64.000 MiB charge, +0.000 working set; touched: +0.000 charge, +64.004 MiB working set | same shape |
| decommit returns charge and RSS | -64.000 MiB charge, -64.000 MiB working set (placeholder path) | yes |
| grow to 3 GiB at the default ceilings, release | +3072.000 MiB, back to +0.000 | +3078.020 (page tables) |

Two differences that matter:

* **No page-table term.** `VM_ACCOUNT` is per page of mapping; Linux does not charge page tables
  to `Committed_AS`. Every Linux figure is exactly its size (3 GiB -> +3072.000 MiB, not +3078).
* **The kernel soft fault costs 3.6x Windows' here (1437 against 398 ns)**, and the pager's
  overhead on top of it is 5.7 us per fault at a 4 KiB granule. At the default 64 KiB granule the
  pager adds ~324 ns per touched page over the bare soft fault (1761 against 1437): D10's rule still
  holds -- commit in granules ahead of use, fault-driven paging is for correctness, not the hot path
  -- and 64 KiB remains where the curve flattens (180 ns/page against 3282 at 4 KiB).

### D12 on Linux

| path | ns per emit+execute | mismatches |
|---|---|---|
| dual-mapped memfd, RW view + RX view | **117.5** | 0 in 1,000,000 |
| one page, `mprotect` RW -> RX -> RW (through the seam, so + ledger) | **2417.7** | 0 in 1,000,000 |

Median of n = 5 runs of 200,000 cycles, each cycle a new `mov eax, imm32; ret` whose return value is
checked. 20.6x (Windows: 162 against 2259 ns, 14x). No page of either view is ever both writable and
executable: `/proc/self/maps` shows `rw-s` and `r-xs` (`tests/vm_linux.rs`).

### The extraction cache across processes (the multi-instance requirement)

`crates/omni-elf/tests/cache_sharing_linux`: the 104.1 MiB `libroblox.so` cache entry (on ext4 under
`target/`, not tmpfs), mapped r-x and every page read in the parent, then in a second process while
the parent's view is live. The second process's `/proc/self/smaps` for that view (n = 1 pair):

| RSS | PSS | Shared | Private | `VM_ACCOUNT` |
|---|---|---|---|---|
| 106,632 kB | **53,316 kB** | 106,632 kB | **0 kB** | **0 B** |

Exactly half: two processes share every page. A third instance would pay a third, and so on.

### Loading libroblox.so (D11, D14)

| | Linux | Windows |
|---|---|---|
| steady commit, eager `.bss` | **16.332 MiB** (11.039 `.bss` + relro/.data) | 16.7 MiB |
| lazy `.bss` | **5.293 MiB** | ~5.4 MiB |
| 3 instances in one process | 49.137 MiB (16.332 / 16.332 / 16.348 marginal) | -- |
| file-backed, charged 0 | 104.141 MiB per instance | same |
| relocation (64 KiB window) | 220.5 ms, 171 windows, transient +5.285 MiB | -- |

### Per guest thread (D5 amendment 2)

`omni-cpu/tests/roblox.rs`, n = 8 threads, 1 measurement: **22.355 MiB** at creation, 22.408 MiB
after translating real Roblox code (Windows: 24.5 MiB). On Linux it is `VM_ACCOUNT`: dynarmic's code
cache and the 16 MiB FastDispatch table are private writable `mmap`s.

## Guest faults: `fault/linux.rs`

* `sigaction(SIGSEGV)` and `sigaction(SIGBUS)`, `SA_SIGINFO | SA_ONSTACK`, both signals masked while
  the handler runs. Only kernel page faults are shown to handlers: `si_code` `SEGV_MAPERR`/
  `SEGV_ACCERR`/`BUS_ADRERR` **and** `REG_TRAPNO == 14`. A sent signal is passed down the chain
  untouched (tested with a stale trap-14 frame: the kernel reports the thread's *last* trap in
  every frame, sent signals included, so `si_code` is the only thing that tells them apart).
* Access kind from `REG_ERR`: bit 4 instruction fetch, bit 1 write, else read. RIP from `REG_RIP`.
* The slot table and quiescence protocol are `fault/windows.rs`'s, copied unchanged, with its three
  deterministic tests. `fault_teardown_race_linux` under a real signal storm: n = 24 rounds x 4
  threads x 48 pages, 1437 faults examined, 24/24 rounds with the teardown mid-load, **25 releases
  waited for an in-flight dispatch**, 0 frames after release, 0 null contexts.
* `errno` saved and restored around the whole handler.

### The ordering problem, and the fix

dynarmic's `exception_handler_posix.cpp` installs its `SIGSEGV` handler lazily, once per process, at
the first `Jit` (a function-local `std::optional<SigHandler>` emplaced by `RegisterHandler`), saving
the disposition it displaced; for a RIP inside its code cache it rewrites RIP to the fastmem fallback
and returns without chaining. `DynarmicBackend::new` installs the pager first, so dynarmic ends up in
front. Reproduced exactly by `fault_chain_linux` (a dynarmic-shaped handler installed after ours
takes region A's fault; ours never sees it).

Fix: `omni_platform::fault::reassert_precedence()` -- if the current disposition is not ours, record
it as the next link and install ours on top -- called by `omni-cpu` after every `od_jit_new`. The
chain is then ours -> dynarmic's -> (dynarmic's saved = ours) -> Rust's. The loop that makes is
broken with a per-thread chain depth: entered again while forwarding, the handler forwards straight
to the disposition that preceded it. Forwarding follows dynarmic's own chaining code (SA_SIGINFO
three-argument call; plain handler; `SIG_DFL` -> reset and return, so the instruction re-faults with
the default action and dies at the real fault; `SIG_IGN` for a kernel fault treated as `SIG_DFL`, as
the kernel itself does), with the forwarded action's `sa_mask` blocked for the call.

Evidence: `omni-cpu/tests/pager_precedence_linux.rs` -- in a binary where it owns the first jit, three
guest `LDR`s to untouched granules are resolved by the pager (3 resolved, 196,608 bytes = 3
granules), **0 dynarmic slow-path entries, 0 degraded slices, the D4 amendment 2 invariant armed**;
a second jit keeps it; an unmapped access is declined by the pager and still becomes a typed
`MemoryFault` through dynarmic's handler. `omni-cpu/tests/faults.rs`'s
`the_vectored_handler_takes_a_guest_fault_before_dynarmic_does` (previously an early-return "SKIPPED"
on Linux) now runs and passes too: 1 fault, 65,536 bytes, 0 slow-path entries.

### Signal safety

POSIX allows only async-signal-safe calls in a handler, and the pager path is not that: it takes the
space's `parking_lot` mutex and the vm ledger's `std` mutex, allocates `BTreeMap` nodes and `Vec`s,
and runs `catch_unwind`. What makes it sound here is narrower, and it is the same bargain the Windows
VEH already makes (a VEH also runs on the faulting thread):

* the signals dispatched are **synchronous** page faults, delivered at the faulting instruction;
* that instruction is guest code or host code reading guest memory -- never inside the allocator or
  under the space's or the ledger's lock (the pager's documented invariant);
* so the only hazard is a fault *inside* the allocator or under one of those locks, which is a host
  defect (heap corruption), not guest input. One more: a panic caught by the pager's `catch_unwind`
  runs the panic hook, which takes stderr's lock -- a deadlock if the faulting thread held it. Not
  fixed; documented here.

The handler's own code (atomics, `sigaction`, `pthread_sigmask`, `raise`, TLS reads of a
const-initialised destructor-less `Cell`) is async-signal-safe.

### Alternate stacks

Rust installs a stack-overflow handler at startup and gives every `std::thread` an alternate stack
of `max(SIGSTKSZ, AT_MINSIGSTKSZ)` = **8192 bytes** here (queried with `sigaltstack`); dynarmic
replaces the first-jit thread's with a 2 MiB one. Measured with a painted alternate stack
(`omni-mem/tests/pager_linux`): the whole demand-pager path, kernel signal frame included, used at
most **3128 bytes** over n = 32 faults on a fragmented map. So `SA_ONSTACK` is kept -- a stack
overflow is then still delivered and forwarded to Rust's handler, which names the thread -- and no
extra alternate stacks are needed for std threads (8 std threads serving demand faults on their own
alternate stacks: `std_threads_serve_demand_faults_on_their_own_alternate_stacks`). A thread with no
alternate stack runs the handler on its own stack, which is fine for everything but an overflow.

## Tests (Linux mirrors; directory targets where a `windows_only.rs` lists `tests/*.rs`)

| crate | file | result |
|---|---|---|
| omni-platform | `tests/vm_linux.rs` (mirror of vm_windows.rs + 4 Linux-only) | 30 pass |
| omni-platform | `tests/vm_commit_charge_linux.rs` | 1 pass (8 measurements) |
| omni-platform | `tests/fault_linux.rs`, `fault_chain_linux.rs`, `fault_teardown_race_linux.rs`, `src/fault/linux.rs` unit tests | all pass |
| omni-mem | `tests/space_linux/`, `arena_linux/`, `arena_execution_linux/`, `commit_charge_linux/`, `pager_linux/`, `probe_linux/` | 41, 12, 5, 1, 2, 1 pass |
| omni-elf | `tests/loader_commit_linux/`, `cache_sharing_linux/`, `relro_linux/` | pass |
| omni-cpu | `tests/pager_precedence_linux.rs` | pass |

### The four suites (`cargo test -p <crate> --release --no-fail-fast`, the process exit code)

| crate | exit | the failures, all in files this port does not own |
|---|---|---|
| omni-platform | **101** | `vm_seam.rs::structural_backends_report_unsupported` (asserts the Linux vm is Unsupported; see Merge notes); `fs::tests::dev_urandom_is_a_device_that_fills_the_buffer_with_entropy` (`process::random_bytes` still structural -- the POSIX worker's area) |
| omni-mem | **101** | `config.rs::an_unimplemented_backend_fails_honestly_rather_than_appearing_to_work` (the same structural assertion) |
| omni-elf | **101** | `loader_m1.rs::writing_to_sealed_relro_faults` (asserts the Windows exit code; `relro_linux` passes) |
| omni-cpu | **0** | -- |

Every other target in the four passes (198, 77, 161 and 124 `ok` lines respectively), including
every Linux mirror above.

### Mutation rows (`tools/lnx_rows/mem.py`)

**23/23 caught** (`flock ~/odb/build.lock python3 tools/mutate_linux.py --only lnx-vm`: 14/14;
`--only lnx-fault`: 9/9; pre-flight: every pattern matched once, every command passed on the
unmutated tree; `git diff --exit-code` clean after each run).

| row | what it breaks | caught by |
|---|---|---|
| lnx-vm-A1 | decommit = `MADV_DONTNEED` + `mprotect(PROT_NONE)` (pages freed, charge kept) | vm_commit_charge_linux |
| lnx-vm-A2 | decommit_to_placeholder only `mprotect`s | vm_commit_charge_linux |
| lnx-vm-B1 | reservations `MAP_NORESERVE` (the brief's suggestion): commit never charged | vm_commit_charge_linux |
| lnx-vm-A3 | release trusts any descriptor | vm_linux (2) |
| lnx-vm-A4 | no exact-size placeholder check | vm_linux |
| lnx-vm-A11 | a split is not recorded | vm_linux (2) |
| lnx-vm-A5 | a partial view unmap is carried out | vm_linux (2) |
| lnx-vm-A6 | r-x allowed on a view of a non-executable file | vm_linux |
| lnx-vm-B2 | r-x refused on every view | vm_linux |
| lnx-vm-A7 | a shared file's view `MAP_PRIVATE` | vm_linux |
| lnx-vm-B3 | every file view `MAP_SHARED` | vm_linux (3) |
| lnx-vm-A8 | a read-only descriptor accepted as shareable | vm_linux |
| lnx-vm-A9 | the D12 section's views `MAP_PRIVATE` | vm_linux |
| lnx-vm-A10 | commit charge sums every VMA | vm_commit_charge_linux |
| lnx-fault-A1 | `reassert_precedence` a no-op on Linux | omni-cpu pager_precedence_linux |
| lnx-fault-A2 | omni-cpu never re-asserts: dynarmic's handler runs first | omni-cpu pager_precedence_linux |
| lnx-fault-A3 | the chain-depth guard removed: the loop | fault_chain_linux (the process dies) |
| lnx-fault-B1 | a re-assertion re-installs when already first, forwarding to itself | fault_chain_linux (the process dies) |
| lnx-fault-A4 | a sent signal dispatched as a fault | fault_linux (2) |
| lnx-fault-A5 | the access kind read from the present bit | fault_linux |
| lnx-fault-A6 | release does not wait for an in-flight dispatch | fault::linux unit tests (2) |
| lnx-fault-A7 | a draining slot unpublished with 0 | fault::linux unit tests |
| lnx-fault-A8 | `SIG_DFL` at the end of the chain returns without resetting | fault_chain_linux (20 s deadline) |

One refusal has no row because this host cannot exercise it: the `PROT_EXEC` probe in
`open_file_for_mapping` refusing a file on a `noexec` mount. There is no user-writable `noexec` mount
here, and unprivileged user namespaces (which would allow mounting one) are blocked by AppArmor
(`kernel.apparmor_restrict_unprivileged_userns = 1`). The accepting direction is exercised by every
executable open.

### omni-android, with this backend

* `cargo test -p omni-android --release --test initializers -- --test-threads=1` -> **exit 101**:
  `libroblox.so` loads and relocates, and init_array[0..3] run, then init_array[3] stops at
  `arc4random_buf`: `omni_platform::process::random_bytes` is still structural on Linux (the POSIX
  worker's area). `--test jni_startup` -> **exit 101**, the same stop.
* **Experiment, not committed** (reverted with `git checkout`, tree verified clean): with
  `random_bytes` (`getrandom`), `current_cpu` (`sched_getcpu`) and `cpu_time`
  (`CLOCK_PROCESS_CPUTIME_ID`) filled in temporarily in `process/unix.rs`, **all 3,594
  initializers run in order** (`initializers`: 3 passed, exit 0, 17.8 s) and **`jni_startup` passes**
  (2 passed, exit 0): `JNI_OnLoad` returns 1.6 and the scripted sequence runs. So on this host the
  memory and fault layers carry the whole initializer run; what stops it today is those three
  process primitives.


## Merge notes (shared edits)

Every edit to a file this port does not own, all additive, none changing Windows behaviour:

| file | what | why |
|---|---|---|
| `crates/omni-platform/src/fault/mod.rs` | `mod linux` / `use linux as backend` for `target_os = "linux"`; `unsupported` now `not(any(windows, linux))` | the backend selection |
| same | `FaultError::Signal { signal, source }`, `FaultError::PrecedenceContested { signal, displacements }` | `FaultError::Os` renders as "`AddVectoredExceptionHandler` failed", which would be false for a `sigaction` failure |
| same | `pub fn reassert_precedence() -> FaultResult<()>` (cfg'd: Linux backend, `Ok(())` elsewhere) | the ordering fix; Windows needs nothing, `windows.rs` untouched |
| same | the "Scope" doc paragraph | it said Linux was Unsupported |
| `crates/omni-mem/src/pager.rs` | `DemandPager::reassert_precedence()` pass-through; `install`'s error doc no longer says Linux is Unsupported | `omni-cpu` depends on `omni-mem`, not on `omni-platform` |
| `crates/omni-cpu/src/dynarmic/mod.rs` | after `od_jit_new`, when the backend owns paging: `DemandPager::reassert_precedence()`, freeing the jit and refusing on error | dynarmic installs its handler at the first jit |
| `crates/omni-mem/Cargo.toml` | `[target.'cfg(target_os = "linux")'.dev-dependencies] libc` | `sigaltstack` in `tests/pager_linux` |
| `Cargo.lock` | one line: `libc` in omni-mem's dependency list | follows from the above |

Not edited, and needing the coordinator (each fails or misleads on Linux **because** the backend is
now real):

* `crates/omni-platform/tests/vm_seam.rs::structural_backends_report_unsupported` is
  `#[cfg(not(target_os = "windows"))]` and asserts `vm::reserve` is Unsupported: it now **fails on
  Linux**. The fix is `#[cfg(target_os = "macos")]`.
* `crates/omni-mem/tests/config.rs::an_unimplemented_backend_fails_honestly_rather_than_appearing_to_work`:
  the same, the same fix.
* `crates/omni-mem/tests/windows_only.rs` and `crates/omni-elf/tests/windows_only.rs` print, on
  Linux, that the unix backend is structural and the gated suites did not run -- no longer true
  (their Linux mirrors run; they are directory targets so these lists do not see them). A print,
  not an assertion.
* `crates/omni-elf/tests/loader_m1.rs::writing_to_sealed_relro_faults` is portable but asserts the
  Windows exit code `0xC0000005`; on Linux the child correctly dies by `SIGSEGV` (no exit code) and
  the test **fails**. `tests/relro_linux/` asserts the Linux verdict. The fix is to accept
  `ExitStatusExt::signal() == Some(SIGSEGV)` on unix.
* `crates/omni-platform/src/lib.rs` and `vm/error.rs` docs still describe vm as structural on Linux
  and `VmError::Unsupported` as "returned by the Linux and macOS backends".
* `OsError::name()` (`vm/error.rs`) is Windows' table, so on Linux an `errno` that collides by number
  prints a Windows name: 2 `ENOENT` -> `ERROR_FILE_NOT_FOUND` (right by coincidence), but 5 `EIO` ->
  `ERROR_ACCESS_DENIED`, 3 `ESRCH` -> `ERROR_PATH_NOT_FOUND`, 6 `ENXIO`, 8 `ENOEXEC`, 32 `EPIPE`.
  The vm backend returns none of those except `ENOENT`; a `cfg(unix)` errno table was written and
  **backed out**, because `vm_seam.rs` asserts the Windows names on every target.


## Open issues

* **The kernel soft fault is 1437 ns here against 398 ns on Windows** (n = 11 x 16384 pages; not
  the same machine as D10's). Transparent huge pages are not involved (`enabled = madvise`, and
  nothing here madvises). This Haswell runs with the Meltdown mitigation (`vulnerabilities/meltdown:
  Mitigation: PTI`), which puts a page-table switch on every kernel entry and exit; that is the
  likely cause of the high fault and syscall figures throughout -- a hypothesis these numbers are
  consistent with, not one they establish.
* **The pager is not async-signal-safe in the POSIX sense** (see "Signal safety"). Sound for
  synchronous faults under the pager's invariant, as on Windows; a panic caught inside the handler
  takes stderr's lock.
* **Commit into a plain reservation by `protect`** is not refused (the ledger does not track commit
  inside plain reservations). Only tests use plain reservations.
* **A stale descriptor with the same base and length** as a newer reservation cannot be told apart
  (on either backend).
* **`vm.overcommit_memory = 2`** is argued from `mm/mmap.c` and `mm/mprotect.c`, not measured
  (needs root). Under it commit would fail with `ENOMEM` at `CommitLimit`, which `omni-mem` maps to
  the commit-limit refusal as `ERROR_COMMITMENT_LIMIT` is on Windows.
* **`vm.max_map_count`**: every commit granule and every protect split is a VMA. This host's limit is
  1,048,576 (Ubuntu's), far above the 65,530 kernel default; a host with the default and a
  fragmented 4 GiB guest space could reach it (`ENOMEM` from `mmap`/`mprotect`). Not measured on the
  real engine.
* **The executable probe's refusal** (a `noexec` mount) is untestable here (see the mutation table).
* **The declined-fault path through dynarmic's handler on an 8 KiB std alternate stack** is exercised
  (`omni-cpu/tests/faults.rs`, typed exits on many threads, all pass) but its stack use was not
  measured; the pager path was (3128 bytes).
* **The `a_larger_commit_granule...` and D12 timings are single machines under a live desktop**;
  the D12 flip path includes the ledger lock, which the Windows figure (raw `VirtualProtect`) does
  not.

