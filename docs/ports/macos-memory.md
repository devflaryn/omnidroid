# macOS port: memory

The owner's requirement is memory strictly on demand: boot ~4 GB peak, steady ~800 MB, and about ten
instances started one after another on an 8 GB machine. This is what the macOS workstream `mac-mem`
measured, changed and left open, on an Apple M1 (16 GB, macOS 26.5). Every figure carries its n and
its instrument. "The gate" is `gameactivity`'s
`initialize_native_code_returns_a_native_code_and_the_game_thread_starts` with the window, run as
`docs/ports/macos.md` describes, which reaches the engine's real landing screen
(`APP_READY ... data(Landing)`).

## Result

| | before (port-macos `ef65a2d`) | after (`mac-mem`, patches 0010-0013 + `LoadBytes`) |
|---|---|---|
| footprint at +60 s (landing screen) | 2,537 / 2,420 / 2,698 MiB (n = 3) | **834 / 841 / 811 / 815 MiB** (n = 4) |
| footprint at +75 s | 2,874 / 2,446 / 2,729 MiB -- still climbing | 841 / 847 / 818 / 821 MiB -- flat |
| boot peak (`ri_lifetime_max_phys_footprint`) | 2,878 / 2,492 / 2,737 MiB (n = 3) | **1,045 / 1,050 / 1,105 / 1,122 MiB** (n = 4) |
| dynarmic bookkeeping per translated block | 2,099 B (gate census) / 1,785 B (test shape) | 322 B (gate census, capacity) / 364 B (test shape) |
| block records the jits hold at +60 s | 593,699 (census, n = 1) | 331,691, of which 12,967 invalidated (census, n = 1) |
| bookkeeping held after a cache clear | 1,483 B per block (test shape) | 0-1 B, and given back to the kernel |
| whole-libroblox buffer held for the session | 104 MiB | 0 |

Instruments: `tools/footprint_mac.py` (1 s samples of `phys_footprint` and the kernel's lifetime peak,
from outside the process; one after-run at 0.05 s), `footprint(1)` and `vmmap` at +60 s. The before
column is three plain runs of the `ef65a2d` binary (a detached worktree with its own `target/`); the
after column is the final binary. The boot peak is a transient: sampled every 50 ms it is 150-250 MiB
spikes lasting ~0.5 s around the landing screen (+28-30 s), and it is the same with 0013's allocator
disabled (1,047 and 1,067 MiB, n = 2), so it is not that patch's; what allocates it was not
attributed. It was 890-904 MiB in three runs of the build before 0013 (n = 3) and 957-1,061 MiB in
four others of builds before it, so the run-to-run spread is about as large as any difference.

**At the landing screen, by category** (`vmmap -wide`, dirty + compressed, +60 s; the guest space is
the gate's 16 GiB reservation, found as the reserved run that ends it):

| | before (n = 2) | after (n = 3) |
|---|---|---|
| `MALLOC_LARGE` in use | 993 / 1,022 MiB | none |
| `MALLOC_LARGE (empty)` -- freed, kept by the allocator | 30 / 77 MiB | 57 / 57 / 61 MiB |
| `MALLOC_SMALL` | 668 / 848 MiB | 77 / 80 / 77 MiB |
| other `VM_ALLOCATE` (0013's arrays, host mappings) | 0.1 MiB | 67 / 104 / 59 MiB |
| code caches (`MAP_JIT`) | 287 / 309 MiB | 184 / 169 / 175 MiB |
| guest space | 326 / 323 MiB | 326 / 325 / 324 MiB |
| graphics (`owned physical footprint (graphics)`) | 71 / 71 MB | 75 / 71 / 77 MB |

The guest's own memory did not move, as it should not: it is the engine's.

## Where the memory was (MEASURED, before)

At the landing screen `footprint(1)` said `MALLOC_LARGE` 912-1,105 MB, `MALLOC_SMALL` 649-687 MB,
untagged `VM_ALLOCATE` 571-631 MB, graphics ~90 MB. A census compiled into a scratch build of the pin
(`AddressSpace` counters per jit: blocks, relocations, link targets, fastmem sites and the bucket
counts of every map, printed at +30/45/60/75/85 s; element and bucket sizes from `sizeof` on the
vendored headers) showed, at +60 s (n = 1 run):

* **593,699 blocks in 39 jits** (one jit per guest thread): 88,163 in the largest, 616 in the smallest;
  eleven jits hold 20,000-125,000 blocks each, of the same engine code.
* **2,099 bytes of bookkeeping per block** before malloc rounding, 1.19 GiB in all:
  `block_infos` buckets 890 (the 200-byte `EmittedBlockInfo` inline, load factor <= 0.5),
  `block_references` buckets 484 + sets 46, per-block fastmem maps 281, `relocations` 134, per-block
  link maps 122, `block_entries` 78, the reverse `std::map` 64. Per block: 6.3 relocations,
  1.3 link targets, 2.3 fastmem sites, 250 bytes of host code.
* the icl `block_ranges`: 846,107 interval nodes + 874,315 set nodes, 123 MB (`heap(1)`), never
  cleared.
* **54 % of the records were for invalidated blocks** at +85 s (451,075 of 835,605), and
  1,045,686 of the 1,581,230 blocks emitted since start had been invalidated -- almost all by one
  request, the whole 16 GiB guest space (below).
* `libroblox.so`, 104 MiB, read into a `Vec` held in a static for the whole process.

## What changed

| change | root cause | effect |
|---|---|---|
| dynarmic patch **0010** | every block's `EmittedBlockInfo` kept whole inside map buckets | flat 24-byte records |
| dynarmic patch **0011** | icl interval map per jit, never cleared | 24-byte ranges on a page index, cleared with the cache |
| dynarmic patch **0012** | invalidated blocks kept until the cache filled | an invalidation that leaves nothing is a clear |
| dynarmic patch **0013** | freed large arrays kept by the host allocator | the bookkeeping's large arrays are pages of their own |
| gate: `LoadBytes` | whole library held for the session | read for the load, released after |
| `omni-cpu` test floor | pre-existing failure on macOS since 0009 | instrument check on macOS |

Each dynarmic patch is carried as `crates/dynarmic-sys/patches/README.md` describes, with its
measurement there; `verify_patches.py` passes with 13 patches. Behaviour is unchanged: what is emitted,
when, and what the guest executes; the one difference in which translations are dropped (0011, after a
clear) is stated exactly in the README and removes only a retranslation of unchanged code.

## Per jit, after

The same census, rewritten for the new structures (sizes and capacities read from each jit at +60 and
+85 s; a scratch build, n = 1 run):

* **331,691 block records in 39 jits at +60 s, 318,724 of them live** -- 12,967 invalidated, against
  54 % before: the jits had been cleared 145 times by +60 s (patch 0012 turns a whole-space
  invalidation into a clear; a full cache is the other cause). The largest jit holds 81,459;
  seventeen hold under 3,100.
* **322 bytes per record, by capacity** (what is reserved; with 0013 only written pages are charged):
  fastmem sites 75, `block_entries` 67, `link_heads` 60, link records 39, block records 36, guest
  ranges 36, page index 10 -- 102 MiB in all, against 1.19 GiB + 123 MB before.
* Blocks emitted since start: 1,596,025 by +60 s -- the retranslation churn is unchanged (open item 3).
* Current code in the caches: 79 MiB (the touched high-water marks are 169-184 MiB, open item 2).

## Multi-instance

Method: `python3 tools/footprint_mac.py --launch N --stagger S --session T --out DIR` starts gate
instances one after another, each with its own fresh `OMNI_DATA_DIR`, log and window, and samples
every 2 s each instance's `phys_footprint` (`proc_pid_rusage`) and the system's compressor (`vm_stat`:
pages occupied by it), free level (`kern.memorystatus_level`) and swap (`vm.swapusage`). The machine
is a 16 GB M1 with other applications open: the compressor already held 3.3-3.6 GB (10-11 GB of
other processes' pages) at every start, and **a further engine process -- the `omnidroid-wt-hvf`
workstream's own gate -- was running during part of run 1**. Runs 1 and 2 used the binary before
0013 (runs 1-2 by a scratch copy of the same sampling code); run 3 the final binary, through the tool.

| run | binary | stagger, session | reached the landing screen, exit 0 | each instance at its landing screen | all four alive: total | system during it |
|---|---|---|---|---|---|---|
| 1 | 0010-0012 | 40 s, 300 s | 2 of 4, 2 of 4 | 916-918, 878-882 MiB | 1,798-2,172 MiB | compressor 3.3 -> 4.6 GB, free 65 % -> 55 %, swap unchanged |
| 2 | 0010-0012 | 90 s, 330 s | 3 of 4, 0 of 4 | 795-799, 888-904, 881-893 MiB | 2,465-2,730 MiB | compressor 3.4 -> 5.4 GB, free 64 % -> 49 %, swap unchanged |
| **3** | **final** | **90 s, 330 s** | **4 of 4, 4 of 4** | **770-859, 770-828, 793-845, 750-792 MiB** | **2,524-3,361 MiB (3,171 with all four at the landing screen)** | **compressor 3.6 -> 7.6 GB, free 62 % -> 35 %, swap unchanged (1,179 MiB)** |

* **An instance costs what it costs alone**: 750-859 MiB at the landing screen with up to three
  others running (run 3), against 811-841 MiB alone (n = 4). Nothing is shared between instances:
  each has its own guest, its own jits and its own copy of every translation. Boot transients reach
  939-1,020 MiB per instance (2 s samples).
* **The failures of runs 1 and 2 were on the network, not on memory**: in run 1 one engine's settings
  fetch failed `HttpError: SslConnectFail` and another's `getFlags` answered false (those two never
  left ~180 MiB); in run 2 the host resolver refused `getaddrinfo` (`nodename nor servname
  provided`) for `roblox.com` names, which the gate counts as killed guest threads, so every instance
  exited 101 although three had reached the landing screen. **Each of those failures fell within about
  a second of another instance being started** (run 1: 23:59:01 against a start at 23:59:00; run 2:
  00:08:38 and 00:11:37 against starts at +91 s and +272 s); run 3 had none, and no single-instance
  run did (n = 21 this session). Recorded for the network workstream; not investigated.
* **The system absorbed four instances without swapping**, by compressing: free memory went from 62 %
  to 35 % and the compressor grew by 4 GB -- more than the 3.2 GB the instances held, so the kernel was
  compressing other processes' pages (and possibly the instances'; `phys_footprint` counts compressed
  pages at their uncompressed size, so the per-instance figures do not show it).

**What can be concluded for ten instances on 8 GB, and what cannot.** Ten instances at the measured
770-860 MiB each are 7.7-8.6 GB of `phys_footprint` -- about the whole machine, before the OS and the
window server. So ten fit on 8 GB only with the compressor doing a large share of the work (run 3
shows macOS compressing heavily once free memory falls, and never swapping, but on a 16 GB machine it
was not pushed to where 8 GB would be), or with less per instance -- the open items below are where
that would come from. CPU was the other visible limit: with three or four engines rendering on eight
cores one instance's settings fetch took 55 s instead of ~10 s (run 1), and landing took up to 35 s
(run 3, instance 3) against 23-29 s alone. So: **one instance now meets the ~800 MB steady target
(811-841 MiB at +60 s alone, 750-860 MiB with three others), and boots at a 1.0-1.1 GB peak against
the ~4 GB budget; four run side by side on 16 GB without swap; ten on 8 GB is not shown, and at
today's figure needs the compressor or roughly a fifth less per instance.**

## Sharing one translation across threads (scoped, not built)

Each guest thread's jit translates the same engine code into its own cache: at the landing screen the
eleven busy jits hold 20,000-125,000 blocks each, and after this workstream the code caches are still
the second-largest item (169-184 MiB of touched `MAP_JIT` pages, n = 3 runs). One cache shared by
every thread of an instance would divide that, and the records, by roughly the number of busy threads.
What it would take in the arm64 backend, from reading the pin:

1. **Generated code must stop embedding per-thread values.** Blocks inline the addresses of the
   thread's `TPIDR_EL0`/`TPIDRRO_EL0` boxes (`emit_arm64_a64.cpp:529-546`) and, for the inline
   exclusives of patch 0007, the thread's monitor slots (`processor_id`,
   `emit_arm64_memory.cpp:730-833`). Both would have to be loaded from `JitState` (which is per
   thread and in `Xstate`) instead.
2. **One prelude, per-thread callbacks.** The prelude's trampolines embed the thread's
   `UserCallbacks*` and `&conf` (`EmitCallTrampoline` and the exclusive trampolines,
   `a64_address_space.cpp:25-213`), and blocks reach them with PC-relative `BL`s. A shared prelude
   would load the callbacks pointer from `JitState` or the run-code frame.
3. **Concurrent lookup and emission.** `block_entries` is read by the dispatcher of every thread and
   written by whichever thread misses: a lock around `GetOrEmit` (emission is rare once warm) and a
   lookup safe against a concurrent insert. macOS's per-thread `MAP_JIT` write protection is on the
   emitting thread only, which suits this.
4. **Invalidation while other threads execute.** Unlinking patches branches in live code (an aligned
   4-byte store plus cache maintenance is safe on arm64), but an invalidated block's code may still be
   running on another thread, so its space cannot be reused -- and `ClearCache` cannot reset the
   cache -- until every thread has left generated code: an epoch or quiescence scheme at `RunCode`
   entry and exit. Patch 0012's reasoning ("no generated code on the stack") would have to hold for
   every thread at once.
5. `FastmemManager::do_not_fastmem` and the exception handler's registration would be per cache, not
   per jit.

That is a rewrite of the address space's ownership model, with a concurrency design that needs its
own tests (the stoppability matrix and the invalidation tests at N threads), not a patch; it was not
started.

## Idle threads

The census answers whether idle threads' translations are worth dropping: the smallest jits hold
616-1,017 blocks, which after 0010-0012 is about 0.5 MiB each including their code. And the idle
threads are already the ones cleared: a context that does not cross the boundary lets its queue of
other threads' invalidations overflow, which `omni-android` turns into a whole-space invalidation,
which patch 0012 turns into a clear. Nothing further was done.

## Still open, with the consequence

In order of size at the landing screen after this workstream (`vmmap`, +60 s, n = 3 runs):

1. **The guest's own memory, 324-326 MiB** (dirty + compressed) in the guest space -- the engine's heap,
   its libraries' writable segments, stacks. It is what the engine uses; this layer already gives back
   what the guest returns (`MADV_DONTNEED` decommits; `munmap` releases). `MADV_FREE` only marks
   (`advise_idle`) and is reclaimed at the next `MADV_DONTNEED`, which is Linux's own "when the
   kernel wants it" semantics; how much of the 300 MiB is such marked-but-held memory was not
   measured. *Consequence*: the floor of an instance, whatever the translator does.
2. **Code caches, 169-184 MiB** of touched `MAP_JIT` pages over 40 jits: each jit's high-water mark,
   up to its 32 MiB cache for the busiest (four were at 28-31 MiB in one run). A clear resets the offset but the
   pages stay dirty, and nothing gives them back. Returning the pages above the offset on a clear
   (a fresh `MAP_FIXED|MAP_JIT` mapping over them) is a small patch; it would help only jits that
   clear and do not refill, which was not measured, so it was not made. Sharing one cache across a
   process's threads (scoped above) is the change that divides this term. *Consequence*: ~20 % of an
   instance.
3. **The translator's retranslation churn.** `omni-android` broadcasts every guest `munmap`,
   `mprotect` and `MADV_DONTNEED` -- data ranges included -- to every other live context
   (`boundary.rs`, `CodeWatch::broadcast`); a context that has not crossed the boundary for 64 of them
   invalidates its whole guest space. MEASURED on the pin: 1,045,686 of 1,581,230 blocks emitted in
   85 s were later invalidated, nearly all by that whole-space request. Patch 0012 makes the memory
   side of it free (the invalidated jit is cleared), but the retranslation is still paid in CPU.
   Queuing only ranges that intersect something executable -- the guest's `PROT_EXEC` mappings --
   would remove most of it; that is a change to `omni-android`'s invalidation contract (Global
   Constraint 11), not this workstream's to make. *Consequence*: CPU, which is what limited the
   multi-instance runs.
4. **The host heap, 134-141 MiB**: `MALLOC_SMALL` 77-80 MiB and 57-61 MiB of `MALLOC_LARGE (empty)`
   -- large blocks freed by someone and kept, dirty, by the allocator (not dynarmic's: they stayed
   when 0013 took dynarmic's arrays off the heap). Not attributed; `malloc_history` under
   `MallocStackLogging` on this build is the next measurement, and `malloc_zone_pressure_relief`
   is what would return the kept blocks.
5. **Graphics, ~85 MiB** (`owned physical footprint (graphics)` 75-79 MiB + IOAccelerator/IOSurface):
   the swapchain and the engine's textures through MoltenVK; not examined.
6. **Ten instances on 8 GB is not demonstrated** (multi-instance, above), and the network failures
   seen twice when an instance started beside running ones are not investigated.

## Merge notes

Every edit outside `crates/dynarmic-sys/{vendor/…/backend/arm64, patches, tests, tools, shim}`,
`tools/mutate_mac/cpu.py`, `tools/footprint_mac.py` and this file:

* **`crates/omni-cpu/tests/roblox.rs`** (`09ba018`): `the_per_thread_cpu_cost_is_measured_and_under_its_ceiling`
  failed on macOS at `ef65a2d`, before this workstream (0.027 MiB per thread at creation against a
  1 MiB floor, because patch 0009 made the code cache charge lazily). On macOS the floor is now the
  instrument seeing 16 MiB touched; `#[cfg(not(target_os = "macos"))]` keeps the old assertion, so
  Windows runs exactly what it ran.
* **`crates/omni-android/tests/gameactivity.rs`** (`f8a4e1d`, `445343d`): `Guest::load` reads
  `libroblox.so` into a `LoadBytes` -- an `omni_platform::vm` reservation, reserved, committed,
  filled, and released when the load returns -- instead of `main_lib_bytes()`'s process-lifetime
  `Vec`. The other tests' `main_lib_bytes()` is unchanged. On Windows the same seam reserves,
  commits and releases the same size; nothing at runtime changes.
* **`crates/dynarmic-sys/shim`**: `od_page_backed_bytes()` added (measurement only; no struct or
  existing signature changed, ABI version unchanged). `src/lib.rs` has no binding for it; the test
  that reads it declares it.
* **Nothing** in `omni-mem`, `omni-platform`, `omni-cpu/src` or `omni-android/src` changed. The x64
  backend is untouched: every dynarmic change is under `backend/arm64/`, and `AddressSpace::ClearCache`
  becoming virtual is in the arm64 `AddressSpace`, which x64 does not have.

* **Patch numbering and `port-macos`'s 0014.** This branch carries 0010-0013; `port-macos` has since
  added 0014 (`emit_arm64_memory.cpp`, the store-exclusive as a fastmem patch location). Checked on
  this branch: `git apply --check` of `port-macos`'s 0014 onto `vendor/dynarmic` with 0010-0013
  applied succeeds, and with it applied `host_fault` (6, including 0014's two new tests), `exclusive`
  (9), `bookkeeping` (11), `hostile` (18) and `a64_exec` (14) all pass -- 0014's new patch sites go
  through 0010's `FastmemRecord` like every other. `vendor/PIN.txt` and the README's "Applied" list
  conflict textually (both add lines after 0008/0009); the resolution is the union, in number order.
  `verify_patches.py` passes on this branch: pin + 13 patches.
* **An environment trap, not a code change**: a worktree whose APK is a *symbolic* link fails the
  gate (`base.apk is a symbolic link`) because the gate `hard_link`s the APK into the guest's root and
  macOS links the symlink itself. A hard link to the APK (same volume) works.

The mutation rows are `mac-mem-A1`..`A8`, `B1`, `B2` in `tools/mutate_mac/cpu.py`.

## Verification

* `cargo test -p dynarmic-sys --release --no-fail-fast`: every target ok (exit 0) after each patch;
  `bookkeeping` has 11 tests, two of them instrument checks (the heap reading sees a 16 MiB
  allocation; the footprint sees 16 MiB touched).
* `cargo test -p omni-cpu --release --no-fail-fast`: exit 0 (with `09ba018`; it failed at `ef65a2d`).
* `omni-android` `initializers` (3 + 3 ignored), `jni_startup` (2), `bionic` (242): exit 0.
* The gate: exit 0 and `data(Landing)` in every single-instance run of the final binary (n = 4) and of
  every intermediate build (n = 8), and in the instrumented and diagnostic runs (n = 6).
* `python3 tools/mutate.py --only mac-mem-`: **10/10 caught** (A2 by the process aborting in dynarmic's
  "Segfault wasn't at a fastmem patch location", which the harness counts as a failed suite); then
  `git diff --exit-code crates tools` clean, `PIN.txt` touched and the tree rebuilt.
