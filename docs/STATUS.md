# Status

The honest capability record. A thing is **Verified** only if it was run and observed. Nothing is
claimed for Linux or macOS, because nothing has been tested there.

Last updated: 2026-09-21

## Platforms

| Target | Status |
|---|---|
| Windows x86-64 | **Active development.** Host capabilities verified; runtime not yet built |
| Linux x86-64 | Not tested. Structural portability only |
| Linux ARM64 | Not tested. Structural portability only |
| macOS ARM64 | Not tested. Structural portability only |
| macOS x86-64 | Not tested. Structural portability only |

## Verified by measurement on this host

| Area | Finding |
|---|---|
| Toolchain | Rust 1.89 msvc compiles and links; MSVC 14.44 present but off `PATH` |
| Host CPU | AVX2, BMI2, FMA, F16C, AES, SHA, CMPXCHG16B; **no AVX-512** |
| Vulkan | Loader 1.4.321; RTX 4060 device API 1.4.325; `VK_KHR_win32_surface` available |
| Native window | Resizable desktop window rendering a Vulkan triangle, with correct swapchain recreation on resize |
| Texture formats | ETC2 and ASTC **unsupported**; BC1/BC3/BC7 supported. Transcoding is mandatory |
| Validation layers | **Absent.** A real use-after-free crashed the driver with no diagnostic |
| Memory: reservation | `MEM_RESERVE` costs 0 bytes of commit, verified to 97.7 TB |
| Memory: commit | Debited immediately on commit, not on touch. 4 GB guest space = 37.25 MB commit |
| Memory: reclamation | `MEM_DECOMMIT` is the only primitive that returns commit charge |
| Memory: placeholders | `MapViewOfFile3` gives real `MAP_FIXED` at **4 KB** base and file-offset granularity (512/512 verified) |
| JIT memory | Dual-mapped section: 162 ns per emit-and-execute cycle versus 2259 ns for `VirtualProtect` |
| Multi-instance | 4 concurrent Vulkan instances independent, about 52 MiB VRAM and 110 to 160 MB host RAM each |
| CPU: identity mapping | Guest VA == host VA verified at a 47-bit address with zero slow-path callbacks; costs one folded SIB base |
| CPU: correctness | dynarmic builds in 49 s; all 202,200 of its test assertions pass; 37/37 hand-encoded A64 checks correct |
| CPU: throughput | About 2.0x native on memory-heavy code, 2.2x on NEON/FP, about 33x on register-bound integer code |
| CPU: fastmem gain | Losing identity mapping costs **30-49x** (n=31, through the runtime's real callback path, two loop shapes, both degraded mechanisms). An earlier 13.2x figure measured a bare stub and is a floor, not the runtime's cost |
| CPU: silent degradation | Under the default 36-bit width a memory-heavy loop takes **20,000 of 20,000** callback-path entries and **still returns the right answer**; with identity mapping it takes **0**. Asserted at startup |
| CPU: per-thread cost, by cache size | 24.5 MiB at an 8 MiB code cache, 34.65 at 32 MiB and **34.65 at 128 MiB** (n = 8 threads, serialized) — **shrinking the cache does not help**, because 16 MiB of it is a fixed array the constructor writes even when its feature is disabled |
| Roblox branch shape | 2.27% indirect, one indirect transfer every 44 words; mean **4.30** instructions per basic block. Both **static** mixes, used as proxies for per-executed-transfer cost models |
| Thunk boundary: cost | **26.7-31.0 ns** per call dispatched inside the run loop, against **81-101 ns** for exiting to Rust (n = 31 per cell per process, 45 processes: 15 rounds × 3 code placements, `tools/thunk_sweep.py`). A band across three inline cells rather than a point; it is the boundary every imported call crosses. See D17 and D18 |
| Thunk boundary: binding | All **565** of `libroblox.so`'s undefined symbols bind to a named slot in a reserved region, verified through the real loader; **560 of 560** thunk addresses found in the relocated image. Every unresolved import is `STT_OBJECT`, so **no function is bound to null**. Nothing is implemented yet: calling one names the symbol and the guest address |
| Thunk boundary: `STT_OBJECT` split | **23** data imports across the library, **18** of them in the 188 the initializers reach. Two figures that looked like a drifted duplicate and are not: measured and asserted in `omni-android/tests/libroblox.rs` |
| CPU: cold translation | 0.15 to 0.31 Mguest-insn/s on synthetic loops, implying 7 to 25 s to warm a Roblox-sized working set. On **870 real `libroblox.so` leaf functions**: **0.516 Mguest-insn/s** (n = 11 passes, median, a fresh context each; 8,679 guest instructions) — 1.7 to 3.4x *better*. Per-instruction cost **rises** with function length (0.698 for the shortest third against 0.494 for the longest), which is the direction the "short functions give the IR optimizer less to work over" explanation needs; that explanation is a **hypothesis**, not established |
| CPU: per-thread cost | 20 to 35 MiB committed per guest thread, code caches not shared between threads. Measured at **24.5 MiB** for this backend's 8 MiB cache (n = 8 threads, serialized) and **asserted against a 32 MiB ceiling**. `GuestCpu::cost()` reports **16.004 MiB** of it from two derived terms — the TLS page and the pin's fixed 16 MiB `FastDispatchEntry` table — and the 8.52 MiB it still misses is itself bounded and asserted |
| CPU: call overhead | **under 53 ns** per entry to and exit from the guest — a ceiling, not the boundary. 63.7 ns per timed iteration (n = 31 passes, median, 870 real leaf functions), of which 10.7 ns is the harness's own register setup and the remaining 53.0 ns still contains ~10 guest instructions of real work |
| CPU: runtime degradation | The startup assertion cannot see a memory path that degrades *after* it passes. Per run slice the callback-path counter's delta must be zero unless the slice ended in a memory fault; measured at **0.396-0.430 ns per counter read** (two runs, each the median of n = 31 runs of 10,000,000) and **0.9906x** / **0.9989x** end to end on a 5,000,000-instruction workload (n = 31 per configuration, two runs) |
| Roblox function map | `.eh_frame_hdr` names **245,117** functions with exact bounds, cross-checked against their FDEs. **0 of 568,806 relocations** land in an executable segment, so the bytes in the file at a function's address are the bytes that execute |
| Roblox leaf functions | 819 pure-register, 6 stack-only, 45 stack-guard-protected — 870 of 245,117 are self-contained enough to run before the imported-symbol layer exists |

## Verified about the test APK

`Roblox-2.738.1397.apk` — see `research/apk-analysis.md`. **Note: this APK is cheat-injected and is
not a stock Roblox build** (D6). A stock Play-signed APK is needed before the API surface is frozen.

| Area | Finding |
|---|---|
| ABI | `lib/arm64-v8a/` only, 11 `.so`, all DEFLATED and 4-byte aligned |
| Relocations | `DT_ANDROID_RELA` (APS2): 568,272 (568,194 RELATIVE + 56 GLOB_DAT + 22 **ABS64**, type 257 — a **64-bit** store), plus 534 JUMP_SLOT from a separate `DT_JMPREL`, total 568,806. No `DT_RELA`, no `DT_RELR`. All 568,806 applied and verified against mapped memory |
| Imports | 565 undefined symbols in `libroblox.so` — 539 `STT_FUNC`, 23 `STT_OBJECT`, 3 `STT_NOTYPE`, 4 weak; 669 in the union across all 11 libraries. `DT_VERNEED` attributes 407 of the 565 to `libc.so` (345), `libm.so` (56) and `libdl.so` (6); the other 158 are unversioned and the file records no provider for them |
| JNI surface | Only 59 of 233 `JNINativeInterface` slots used; `JavaVM` needs 2; fields are read but never written; all `CallXxxMethod` go via the `...MethodV` slot |
| Dex execution | **Not required.** No reflection, no `dalvik/system/*`, no Java-side HTTP or file I/O; both `RegisterNatives` sites are native-driven |
| Java surface size | 409 members / 104 classes referenced; ~120 members needed for a first frame |
| Thread prerequisite | `TPIDR_EL0` must point at a bionic TLS block with a stack guard at +0x28 before any guest code runs (1,276 of 1,282 reads target that slot) |
| AGDK | Statically linked into `libroblox.so`; 21-slot callback map recovered, 19 of 21 individually verified; `android_main` at 0x2bcc6a4 |
| TLS | **None.** No `PT_TLS`, no `STT_TLS` anywhere. `pthread_key_*` only |
| Hardening | No ifunc, no BTI/PAC/MTE, no `DT_TEXTREL` |
| Initializers | 3,594 `init_array` entries before `JNI_OnLoad`; RELRO covers 5,205,568 bytes |
| C++ runtime | Statically linked; in-guest unwinder over 11.5 MB `.eh_frame`, needs real `dl_iterate_phdr` |
| Startup | AGDK `GameActivity`, not `NativeActivity` |
| Graphics | Vulkan `dlopen`-only with 1,364 shipped SPIR-V modules; EGL/GLESv2 hard-linked |

## Runtime implementation

The boot milestone ladder in `ARCHITECTURE.md` section 9 is the progress measure. Infrastructure
below a milestone is tracked separately, since a milestone only counts when it passes against the
real APK.

**Infrastructure**

"Reviewed" means an independent reviewer verified it and every Critical and Important finding was
fixed and re-verified. "Pending review" means the implementer's tests pass but nothing independent
has confirmed it yet — on this project that distinction has mattered every single time.

| Component | Status |
|---|---|
| Cargo workspace, nine crates | **Done** |
| `omni-platform` virtual-memory seam (Windows) | **Done, reviewed.** Reserve, 4 KB placeholder split, commit, decommit, protect, file-backed map, commit-charge measurement |
| `omni-platform` dual-mapped sections + placeholder coalescing | **Done, reviewed** |
| `omni-platform` vectored fault seam | **Done, reviewed.** Releasing a handler slot is a quiescence point: the slot is marked draining before the drain and zeroed only afterwards, so a new registrant cannot claim it mid-drain. Took four iterations; the last two defects were found by measurement, not inspection |
| `omni-platform` clock / process / log seams | **Done, pending review.** M3 task 3 phase 3a, the crate's first growth past `vm` and `fault`. Monotonic and wall clocks from one process epoch, sleep, pid, cpu count, entropy, current processor, a log sink with Android's and syslog's priority scales. 16 tests |
| `omni-platform` Linux / macOS | **Not implemented, and does not pretend to be.** Every primitive that calls an OS API returns a typed "unsupported on this platform" error naming its intended POSIX call, so a non-Windows build fails immediately rather than misbehaving. The primitives that call **no** OS API — the clocks, the log sink, `pid`, `cpu_count` — are portable `std` and are implemented once, deliberately without a fabricated refusal (D22). **Nothing here has been run on any non-Windows target** |
| `omni-apk` — zip reading + 4 KB-aligned extraction cache | **Done, reviewed.** 35 tests. Milestone **M0** |
| `omni-elf` — ELF64 parsing + APS2 packed relocations | **Done, reviewed.** 85 tests |
| `omni-elf` — loader: map, relocate, resolve, seal | **Done, reviewed.** Milestone **M1** |
| `omni-mem` — guest address space + JIT arena | **Done, reviewed.** 87 tests across `omni-mem` and `omni-platform` |
| `omni-cpu` — `GuestCpu` trait + dynarmic backend | **Done, reviewed.** Milestone **M2**. 489 passing test cases and 8 ignored across the workspace; mutation tables 87/87, 45/45 and 23/23 |
| `omni-elf` — `.eh_frame_hdr` function map + leaf classifier | **Done, reviewed.** 245,117 exact function bounds recovered and graded; the tool M2 chose its code with |
| `omni-android` — the thunk boundary | **Done, reviewed.** M3 task 2. Region, AAPCS64 marshalling both ways, the variadic rules and a guest `va_list` walk, host → guest re-entry. **No symbol is implemented**: all 565 slots are `Unbound` and name themselves when called. Review found three defects, all fixed |
| `omni-bionic` — the pure libc/libm subset | **Done, reviewed.** Strings, wide/multibyte, ctype, locale, numeric conversion, `printf` formatting, libm. Verified by mutation (11 rows, 11/11) after review found its errno constants had no test at all |
| `omni-bionic` — pthread / sync / TLS | **Done for its scope, reviewed.** 42 of the 51 reachable thread symbols; 1 excluded; 8 need host → guest re-entry or the OS and belong to the adapter (D19). Review found a `sem_post` lost wakeup — **1.0104 s** measured — and three timing flakes |
| `omni-android` — the bionic adapter | **Phases 1, 2 and 3a done, pending review.** M3 task 3. **137 of the 188 statically reachable imports** covered: 119 thunk functions bound and all 18 `STT_OBJECT` data objects placed and filled (D20, D21, D22). 51 left — files and directories, sockets and polling, thread lifecycle and signals. `AT_HWCAP` is deliberately still undecided and `getauxval` refuses until a host says which arm |
| `omni-android` — all 3,594 initializers | **Not started.** M3 task 4 |
| `omni-gfx`, `omni-core`, `omni-cli` | Not started |

**Measured, not assumed**

| Property | Measurement |
|---|---|
| Guest address space reservation | 4 GiB costs **0.000 MiB** of commit charge |
| Grown to 1 GiB, written through | **+1026.004 MiB** (+2.004 is page tables at size/512) |
| Everything unmapped, instance closed | back to **+0.000 MiB** — the project's memory requirement, as an assertion |
| Shared read-only file view | 4 MiB costs **+0.008 MiB**, unchanged after reading every byte, so instances share `libroblox.so` text for free |
| Commit granule | 64 KiB. Measured 2414 ns/page at 4 KiB (worse than the 2053 ns VEH fault D10 rejected), 150 ns/page at 64 KiB, against an unavoidable 381 ns first-touch fault |
| JIT arena | Dual-mapped, W+X unrepresentable in the API; a child process storing through the execute pointer dies with `0xC0000005` |
| Partial unmap | Copy-on-write content preserved in survivors, verified in the loader's exact relocation shape; sharing preserved at +0.008 MiB across a partial unmap of a clean 8 MiB view |
| CoW charging | Charged at `protect` time, not write time: +8.020 MiB the instant an 8 MiB view becomes writable, refunded on restore |
| **`libroblox.so` loaded, per instance** | ~16.7 MiB commit per instance (≈11 `.bss` + ≈5 RELRO + ≈0.3 `.data` + page tables), against ~104 MiB mapped file-backed and shared. Peak equals steady, so windowed relocation produces no transient spike. **What is pinned by assertion: steady ≤ 20 MiB and \|peak − steady\| ≤ 1 MiB.** The component figures vary by fractions of a MiB between runs and are indicative, not exact |
| **Three concurrent instances** | ~50 MiB total, ~16.7 MiB each, ~312 MiB mapped file-backed. **Each instance's marginal cost is asserted separately**, so sharing cannot be first-instance-only nor decay with instance count; anything privatising more than ~3.3 MiB per instance fails the test |
| Load wall-time | 11.8 ms release for a 109 MB library |
| `libroblox.so` extraction | 413 ms once (release), 130 us on a cache hit |
| `libroblox.so` load, end to end | **11.8 ms** release / 77.5 ms debug: map 32 us, bind 1,109 symbols 83 us, relocate 568,806 in 171 windows 11.7 ms, protect 13 us |
| Relocation window | 64 KiB, equal to the commit granule. Transient copy-on-write charge 5.285 MiB, all of it the RELRO region becoming private anyway. 4 KiB costs 9 ms more for no saving; past 64 KiB the curve is flat |
| Loader hostile input | 21 tamper cases each refused with a typed error and zero residue, plus 920 single-byte corruptions of a synthetic library: 447 loaded, 473 refused, 0 panics, 0 leaks, 1.8 s |
| Loader mutation testing | 18 mutations of the loader logic, 15 reverting it and 3 over-correcting it; every one caught by at least one test |
| Workspace mutation testing | See `tools/mutate.py`. Every row is caught by at least one named test, in both directions |
| APS2 decode | 568,272 relocations from 46,184 groups in ~2 ms, consuming 2,100,778 of 2,100,778 bytes |

**Known accounting gap (confirmed):** a pagefile-backed section does not appear in `PrivateUsage`, so
the JIT arena's cost is invisible to per-process commit-charge measurement. It invalidates no recorded
measurement — every commit-charge figure here concerns private memory — but the budgeting diagnostic
cannot see its fastest-growing consumer, since the CPU core commits 20-35 MiB per guest thread. The
arena's mapped size is now reported as a first-class figure alongside private usage.

**Confirmed constraint for the loader:** copy-on-write is charged at `protect` time, not write time.
Protecting an 8 MiB `ReadExecute` view to `ReadWrite` costs +8.020 MiB of commit immediately, before
any byte is written, and is refunded on restore. So relocation must proceed in windows — dropping the
whole 109 MB library to writable would transiently charge 109 MB per instance.

**Known gap:** Windows `unmap` is whole-view-only, so a guest partial `munmap` cannot be serviced by
the platform layer directly. The seam refuses it with a typed error carrying the view extent rather
than over-unmapping, and emulation (unmap the view, re-map the survivors) is owed by `omni-mem`.

| Milestone | Status |
|---|---|
| M0 APK parsed, libraries extracted to aligned cache | **Reached.** All 11 ARM64 libraries extracted into the content-addressed 4 KB-aligned cache and then mapped from it, end to end |
| M1 ELF loaded, all 568,806 relocations applied, symbols resolved | **Reached.** 568,806 relocations applied and read back from mapped memory, RELRO sealed over 5,205,568 bytes, 565 imports enumerated and attributed, 3,594 initializers collected. ~16.7 MiB commit per instance (≈11 `.bss` + ≈5 RELRO + ≈0.3 `.data` + page tables), against ~104 MiB mapped file-backed and shared |
| M2 ARM64 function from `libroblox.so` executes | **Reached.** Three real functions run out of the loaded, relocated, RELRO-sealed image with `init_array` deliberately not run. `+0x2c11e34` maps a base64 character to its sextet: **256 predicted values**, one per byte value, predicted from RFC 4648's alphabet rather than from the run, all 256 correct. `+0x2227844` converts a saturating `(seconds, microseconds)` difference to milliseconds across 18 vectors including both saturation bounds exactly. `+0x2872aac` is a stack-protected leaf and is D13's proof in three directions: it returns with the guard matching, it calls `__stack_chk_fail` when the guard is changed between the two reads, and it faults at exactly `TPIDR_EL0 + 0x28` when the thread pointer is unmapped |
| M3 All 3,594 initializers complete | Not started |
| M4 `JNI_OnLoad` succeeds | Not started |
| M5 `initializeNativeCode` runs, surface requested | Not started |
| M6 Vulkan device created through the forwarding layer | Not started |
| M7 First frame presented | Not started |
| M8 Interactive | Not started |

## Open decisions

None blocking. Both D5 (CPU backend) and D7 (JNI without a JVM) are resolved.

| # | Decision | Blocked on |
|---|---|---|

## A note on the numbers in this document

Figures here are **indicative unless stated as pinned**. Commit-charge measurements vary between runs
by fractions of a MiB — page-table overhead, allocator state and measurement timing all move them —
so quoting them to three decimal places implies a precision that does not exist.

This was not a hypothetical concern: an earlier revision of this file recorded lazy `.bss` at
+5.602 MiB while `DECISIONS.md` recorded +5.395 MiB and a fresh release run produced +5.363 MiB.
Three values, two documents, all written to three decimals, **none of them asserted by any test**.
The whole-branch review caught it, and it is exactly the failure this project's discipline exists to
prevent: a documented number being read later as a measured fact.

The rule applied here: quote approximate values in prose, and state separately what is actually
**pinned by an assertion**, because only the pinned values will still be true after the next change.


## Why two rows disagreed about the same number

An earlier revision of this file carried the per-thread CPU cost twice, with two values for the same
quantity at the same cache size, and the first of them carried **no sample size** — in the document
whose own closing note describes exactly that failure. The M2 whole-branch review caught it.

That is the third time in this project a measured figure has been recorded in two places and drifted,
and the pattern is consistent: the duplicate is always added later, by someone summarising a result
rather than measuring it. So the rule tightened here is not "check the figures" but **a measured
quantity appears once, with its sample size, and everything else links to it**.
