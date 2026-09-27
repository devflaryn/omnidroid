# Decisions

One entry per decision, D1-D38 (there is no D0): the current ruling first, then why, then what is
live or would reverse it, then evidence. Amendments, corrections and exceptions are folded into
their parent under the sub-label code cites (e.g. "D5 amendment 4"). History is in git.

**Which APK a figure was measured on.** The repo-root `Roblox-2.738.1397.apk` is now the stock,
Roblox-signed build (D6). Its `libroblox.so` is byte-identical to the one every figure dated before
2026-09-24 07:25 was measured on, so those stand. Real-engine figures from later that day and from
2026-09-25 (in-world `w`-runs in Pet Simulator 99 "PS99", real-code tests) ran on the modified
2.739.691 build, a different `libroblox.so`; they are marked **[2.739.691]** and have not been
re-measured on stock. Synthetic benchmarks are APK-independent. "The D2 host" is the Windows
development workstation (24 threads).

---

## D1 — Implementation language: Rust

**Ruling.** Rust, with C++ through FFI only where a reused component earns it; today that is
dynarmic alone (`crates/dynarmic-sys`, built by CMake from `build.rs`).

**Why.** Memory safety in a process that hosts foreign binaries; Cargo targets for the five hosts;
`unsafe` stays local to the JIT and guest-memory layers. The CPU sits behind `GuestCpu`, so the C++
borrow does not spread. Evidence: `research/host-environment.md`.

## D2 — Baseline host ISA is x86-64-v3, AVX-512 optional only

**Ruling.** x64 code generation of our own targets x86-64-v3 (AVX2, BMI2, FMA, F16C, LZCNT, MOVBE,
CMPXCHG16B); AVX-512 may only be an opportunistic fast path.

**Why.** The D2 host (Raptor Lake) has AVX2/BMI2 and no AVX-512; BMI2 shifts map onto AArch64
shifted operands; CMPXCHG16B carries 128-bit atomics.

**Live caveat.** Nothing enforces it: the build sets no `target-cpu`, and dynarmic detects host
features at run time. arm64 hosts use dynarmic's arm64 backend. Evidence:
`research/host-environment.md`.

## D3 — Reuse only permissively-licensed components

**Ruling.** Link only MIT, BSD, Apache-2.0, ISC, 0BSD, BSL-1.0 and PSF-2.0 code. GPL/LGPL projects
(`libhybris`, `android_translation_layer`) are study material only, so the bionic-compatible loader
is our own. **Why.** Keeps every licensing option open; the owner has not chosen a licence.

**D3 (amendment).** BSL-1.0 and PSF-2.0 were added when dynarmic's vendored tree brought them in via
Boost, an undeclared dependency (376 files 0BSD, 214 BSL-1.0, 12 MIT; no reciprocal licence).
Licence enumeration belongs in the vendoring step. Evidence: `research/prior-art.md`,
`crates/dynarmic-sys/LICENSES.md`.

## D4 — Guest virtual address == host virtual address

**Ruling (D4 resolved).** Identity mapping: a guest pointer is a host pointer. dynarmic runs with
`fastmem_pointer = 0`, `fastmem_address_space_bits = 64`, `silently_mirror_fastmem = false`; a guest
load is one host instruction with no bounds check.

**Why.** The fastmem path ran a memory-heavy loop at 5,055-5,207 Mguest-insn/s; the runtime's
callback path is 30-49x slower (n = 31, two loop shapes; the spike's 13.2x used bare stubs and is a
floor). Guest `mmap` becomes a host reservation directly (D10).

**Guarded in code.** dynarmic's default `fastmem_address_space_bits` (36) silently degrades high
addresses to callbacks, so a startup assertion reads back the live `UserConfig`. Guest PC is
truncated to a sign-extended 56 bits. Omnidroid's vectored handler runs before dynarmic's frame-based
SEH, so Omnidroid owns demand paging.

**D4 amendment 2.** The configuration can pass and the path still degrade (a declined fault made
dynarmic recompile a block with fastmem off for good; a panic in the fault handler declined
silently). So per run slice, the callback-path counter's delta must be zero unless the slice ended in
a memory-fault exit (`CpuError::DegradedMemoryPath`). Cost: one `od_jit_slow_path_total` load
(~0.4 ns) twice per 1M-instruction slice. Disarmed when the backend does not own paging;
`DynarmicBackend::slice_invariant_armed()` says which.

**D4 amendment 1 — identity fastmem does not confine the guest.** Only an address unmapped in the
whole process faults (pager `NotOurs`, typed `ExitReason::MemoryFault`); anything mapped outside
`GuestSpace` (code cache, thunk region, Rust heap, DLLs, driver mappings) is read or written
directly. `omni_mem::admit` stops *this layer* dereferencing guest-chosen numbers (Global
Constraint 11); it is not isolation. Omnidroid is a compatibility layer, not a sandbox. So never hand
the guest a host pointer (it works silently until a shim refuses it far from the cause); memory the
guest touches lives in `GuestSpace` (Vulkan: `VK_EXT_external_memory_host`, `vulkan/memory.rs`).
Confinement would reopen D4 (a based, real-width address space or guard reservations, at 30-49x).
Evidence: `omni-cpu/tests/identity.rs`, `pager_precedence_linux.rs`; `omni-cpu/src/dynarmic/mod.rs`
"What fastmem does not check".

## D5 — CPU core: dynarmic as a pinned fork

**Ruling (D5 resolved).** dynarmic `yuzu-mirror/dynarmic@9d45823` (v6.7.0; `merryhime/dynarmic`
404s), vendored in `crates/dynarmic-sys/vendor/` with patches 0001-0022 and 0024-0028 in
`crates/dynarmic-sys/patches/` (0023 is on another branch; `tools/verify_patches.py` checks pin plus
patches byte for byte), behind `GuestCpu`. Replacing the x64 backend is a plan, not scheduled.

**Why.** The only permissive, direction-correct A64 JIT; a custom one was estimated at ~45k LOC,
2-4 engineer-years. dynarmic's suite in our configuration (A64, Release) passes 201,698 assertions
with the patches (84 cases MSVC, 83 M1). Spike throughput: memory-heavy 5,207 Mguest-insn/s (~2x
native), NEON/FP 1,626 (~2.2x), register-bound 603 (~33x: per-block spills, `lahf`/`sahf`). Build
needs Boost, `-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, and short paths under MSVC.

**D5 confirmed.** `TPIDR_EL0`/`TPIDRRO_EL0`, PAC/BTI hint no-ops, crypto, CRC32, SDOT and unaligned
access work; `SVC` reaches `CallSVC`. `hook_hint_instructions` is not plumbed into A64 (every
`YIELD` exits); the fix is a known candidate, **not applied** (patches README).

**The four risks, as they stand.**
1. *Cold translation* (spike 0.15-0.31 Mguest-insn/s on loops). **D5 amendment 2:** 870 real leaves
   translate cold at 0.516 (n = 11). Its "under 53 ns" call boundary was a ceiling, replaced by
   `omni-cpu/tests/thunk.rs`'s measured round trip. No persistent cache; D38 shares translation.
2. *Per-thread memory* (amendment 2: 24.5 MiB = 16 MiB fast-dispatch table + 8 MiB cache). Now
   4.47 MiB (D32), 4.54 (D35), ~0.06 plus one shared cache (D38); `omni-cpu/tests/roblox.rs`
   asserts 6 MiB (10 on Linux).
3. *Exclusive monitor* (one global spin lock, 21x anti-scaling 1 to 16 threads). **D5 amendment:**
   `fastmem_exclusive_access` is on (without it an `LDXR` takes 2 callbacks). `tools/atomic_mix.py`:
   128 exclusive-monitor sites, 53 LSE, 15,516 ordered `LDAR`/`STLR` (no monitor, stay on fastmem).
   All 53 LSE sites are outline atomics gated on one byte, `__aarch64_have_lse_atomics`
   (`0x683ba58`), and 106 of the 128 exclusive sites are their fallbacks, so `AT_HWCAP` switches
   between the two (D26). Resolved by D31.
4. *Unimplemented decoder entries* (231 of 874: LSE, FP16 arithmetic, BF16, i8mm, `FJCVTZS`,
   `MIDR_EL1`/`ID_AA64*`). There is no interpreter: `InterpreterFallback` is a typed
   `UnsupportedInstruction` stop (`dynarmic/callbacks.rs`). `CNTVCT_EL0` is patch 0001; arm64
   backend gaps are 0002-0006.

**D5 amendment 3.** A processor id is released only after its jit is freed, else two live jits share
a monitor entry and `STXR` wrongly succeeds. `DynarmicBackend::processor_ids_released_early` must
stay 0 (`omni-cpu/tests/lifecycle.rs`).

**D5 amendment 4.** `CNTPCT_EL0` is the host monotonic clock scaled to 600 MHz
(`omni_cpu::clock::CNTFRQ_HZ`, which also programs `cntfrq_el0`), from one process epoch. It had been
the per-slice instruction count, so guest intervals went negative and deadline loops hung; an
instruction accumulator has no honest units. `CNTVCT_EL0` reads the same counter
(`omni-cpu/tests/counter.rs`). Evidence: `research/dynarmic-spike.md`, patches README.

## D6 — Design against the stock engine; guest code and APK input are untrusted

**Ruling.** The required surface comes from `libroblox.so` and the legitimate Roblox, AGDK and
AndroidX parts of a stock, Roblox-signed APK. Guest code, and every field of an APK or library it
loads, is untrusted: a tampered library is an expected case that parsers refuse rather than trust.
This is defence in depth, not a response to a known payload; code comments that say "D6: the APK
under test is cheat-injected" mean this rule.

**Resolved: the old fixture was modified.** Until 2026-09-26 the repo's `Roblox-2.738.1397.apk` was
re-signed by a third party ("Gloop") with a trojanised 18 MB `libzstd-jni-1.5.7-6.so` (a Luau
executor), an injected `classes4.dex` (`com.roblox.gloop.Loader`) and `assets/gloop/dlt.zip`; from
2026-09-24 a re-signed `Roblox-2.739.691.apk` ("Arceus X" disguised as `libzstd-jni`) was used too.
Both are removed. The injected parts were always out of scope.

**Now.** The fixture is the stock, Roblox-signed build (identity in `STATUS.md`, contents in
`research/apk-analysis.md`). Its arm64 libraries other than zstd, `libroblox.so` included, are
byte-identical to the old fixture's, so every measurement of the 2.738.1397 `libroblox.so` stands.
Only `lib/arm64-v8a` is loaded.

**Live caveat.** 2.739.691's `libroblox.so` (109,777,096 bytes, 3,610 initializers) differs; its
figures are marked. Nothing has yet run in a world on the stock APK.

## D7 — No JVM, no ART, no dex interpreter

**Ruling (D7 resolved).** `JavaVM`/`JNIEnv` are native (D28); the Java classes the engine touches
are *defined* by the host, never executed.

**Why.** The stock APK has 26,615 dex classes (about 650 under `com.roblox`); the engine is
`libroblox.so`. Everything that would force dex execution is absent from it: no `dalvik/system/*`,
reflection, `Class.forName`/`defineClass`, `java/lang/invoke`, Java HTTP, `java/io/File`, webkit or
SQLite; `DefineClass` is never dereferenced; both `RegisterNatives` sites are native. The surface is
59 of 233 `JNINativeInterface` slots and 409 members in 104 classes (`jni-surface.md` §0). The cost
is orchestration: Java drives init, so a native script issues it (§8).

**Reverses it.** A Roblox version that moves logic into Java; the method repeats per APK.
Evidence: `research/jni-surface.md`, `jni-surface-lists.txt`.

## D8 — Graphics: forward Vulkan; forward GLES where Vulkan is emulated

**Ruling.** The guest gets a `libvulkan.so` forwarding to the host driver (`omni-android/src/vulkan`,
driver side `omni-gfx`). Its hard-linked EGL/GLES imports resolve to real EGL/GLES forwarded to the
host's (`omni-android/src/gles`), which the engine uses when the host's only Vulkan device is
emulated.

**Why.** Vulkan is `dlopen`-only in `libroblox.so` (593 `vk*` strings, no `vk*` imports).
`shaders_vulkan_mobile.pack` (14.7 MB, STORED) holds 1,364 SPIR-V modules, so no shader
translation. EGL/GLESv2 are `DT_NEEDED` with 91 imports (17 `egl*`, 74 `gl*`); the manifest requires
GLES 3.0. The engine refuses emulated Vulkan devices (`Vulkan: Device %s is emulated, skipping`); we
forward the real GPU and never work around that. MEASURED on a lavapipe-only Linux host: the engine
refused Vulkan and fell back to GLES, which is why GLES became real.
Evidence: `research/apk-analysis.md`, `research/graphics-spike.md`, `gles/mod.rs`.

## D9 — The ELF loader implements Android packed relocations (APS2)

**Fact.** `libroblox.so` uses `DT_ANDROID_RELA` (APS2), no `DT_RELA`/`DT_RELR`:

| Source | Count | Types |
|---|---|---|
| APS2 blob (2,100,778 B) | 568,272 | 568,194 `RELATIVE`, 56 `GLOB_DAT`, 22 `ABS64` |
| `.rela.plt` (`DT_JMPREL`), separate | 534 | `JUMP_SLOT` |

The other ten libraries use plain `DT_RELA`. Also: 3,594 `DT_INIT_ARRAY` entries before
`JNI_OnLoad`; `PT_GNU_RELRO` 5,205,568 bytes; no ELF TLS anywhere (`pthread_key_*` only); no ifunc,
BTI/PAC/MTE or `DT_TEXTREL`; 565 undefined symbols (641 across the 11 stock arm64 libraries; the old
669 included the trojanised zstd); a static C++ runtime whose unwinder needs a faithful
`dl_iterate_phdr`; all `.so` DEFLATED (D11); `libOpenSLES`/`libOpenMAXAL` import nothing but must
load.

**Built on it.** `DT_PLTGOT` is inside relro and `DF_BIND_NOW` is set, so every `JUMP_SLOT` is
applied before relro is sealed. Imports: 539 `FUNC`, 23 `OBJECT`, 3 `NOTYPE`, 4 weak; `DT_VERSYM`
names a provider for 407 (libc 345, libm 56, libdl 6) and none for 158, which must not be invented.
One `JUMP_SLOT` resolves inside the object (own symbol table first). `init_array` slots are zero in
the file: read them from relocated memory.

**D9 (correction).** `.eh_frame_hdr` is an exact function map: 245,117 functions (one empty range at
`0x364f404`, kept). No relocation lands in an executable segment. Function lengths sum to 69,943,828
of 103,645,584 executable bytes, and the scan refuses a map that breaks the bound (DoS guard).
Evidence: `omni-elf/tests/libroblox_golden.rs`; the APS2 decoder consumes all 2,100,778 bytes.

## D10 — Memory: free address space, lazily committed, decommit to reclaim

**Ruling.** Reserve generously, commit lazily in `omni_mem::DEFAULT_COMMIT_GRANULE` (64 KiB) blocks,
reclaim only by decommit. Never commit speculatively (D14 is the recorded exception).

**Why (Windows, `research/windows-memory-model.md`).** `MEM_RESERVE` costs no commit (125.57 TB in
one call). Commit is charged at commit, not touch, plus size/512 for page tables. Only
`MEM_DECOMMIT` returns commit; `MEM_RESET`, `DiscardVirtualMemory`, `OfferVirtualMemory` and
`EmptyWorkingSet` return none. A 4 GB guest space cost 37.25 MB. A VEH demand fault costs 2,053 ns
against 3 ns/page bulk commit, so faults are for correctness. No large pages. Linux and macOS:
`ports/linux-notes/mem.md`, `ports/macos-memory.md`.

**D10 (correction).** `DynarmicBackend::new` accepted a pager that could not be installed, which lost
fastmem silently. It now refuses anything but `FaultError::Unsupported`, and
`omni_platform::fault::MAX_HANDLERS` is 32 (8 was exhausted by parallel tests).
`omni-cpu/tests/pager_exhaustion.rs`.

**D10 (correction 2).** A process-wide handler can be mid-call when its slot is cleared, so
`release` is a quiescence point: a per-slot in-flight count that `release` waits to drain, all
`SeqCst` (`fault/windows.rs`). `omni-platform/tests/fault_teardown_race.rs`: 6 of 24 releases had to
wait.

## D11 — Guest libraries are mapped from a 4 KB-aligned extraction cache

**Ruling.** Each `.so` is decompressed once into a shared, content-addressed, 4 KB-aligned cache
file (`omni-apk`) and mapped from it, never from the APK.

**Why.** Windows placeholders (`VirtualAlloc2`/`MapViewOfFile3`, `kernelbase.dll` only) give
`mmap(MAP_FIXED)` at 4 KB base and offset alignment: reserve with `MEM_RESERVE_PLACEHOLDER`, split
with `MEM_PRESERVE_PLACEHOLDER`, replace an exact-size placeholder (else error 487). The libraries
are DEFLATED, and copying costs permanent private commit per launch. File-backed read-only and
executable pages cost no commit and are shared across instances, so `libroblox.so`'s 103,649,280
bytes of text and rodata are shared; only relro, `.data` and `.bss` are private.

**D11 correction.** Executability is chosen twice: the file is opened
`GENERIC_READ | GENERIC_EXECUTE` with a `PAGE_EXECUTE_READ` section, and again at every `map_file`;
a view mapped read-only can never become executable (error 87). So text is mapped `ReadExecute` and
relocated as `ReadExecute` -> `ReadWrite` -> write -> `ReadExecute`; writable segments are mapped
`Read` and raised only in windows. `ReadWrite` is `PAGE_WRITECOPY` for a view, `PAGE_READWRITE` for
private memory.

**Also measured.** Copy-on-write is charged at `protect`, so relocation goes in windows. A partial
unmap writes back only survivor pages that differ from a pristine view. Loading `libroblox.so`
costs +16.7 MiB commit (11.0 `.bss`, 5.0 relro, 0.3 `.data`) and 11.8 ms.
Evidence: `omni-mem/tests/space.rs`, `omni-elf/tests/loader_commit.rs`.

## D12 — Omnidroid's JIT memory is a dual-mapped section, never W+X

**Ruling.** Omnidroid's own JIT arena (`omni_mem::arena`) is one pagefile-backed section mapped RW
and RX; no page **Omnidroid owns** is writable and executable at once.

**Why.** 162 ns per emit+execute, 0 mismatches in 200,000, against 2,259 ns for a `VirtualProtect`
flip. Linux maps a `memfd_create` section twice; macOS remaps one anchor (`omni-platform/src/vm/`).

**D12 (guest carve-out, 2026-09-27) — a *guest* may map its own memory W+X.** `mprotect`/`mmap`
with `PROT_READ | PROT_WRITE | PROT_EXEC` from the guest is granted, as
`omni_platform::vm::Protection::ReadWriteExecute` (`bionic/guestmem.rs`, `omni-linux/src/mm.rs`).
The shapes that need it -- a self-decrypting library, an app's embedded JIT -- are apps this
runtime exists to run, and a device grants it. W^X above is an invariant of Omnidroid's *own* JIT
pages, not a rule imposed on the guest. Correctness does not rest on the protection: AArch64
requires the guest to issue `IC IVAU` after writing code (its instruction cache is not coherent
with stores), and the CPU backend intercepts that op to discard the stale translation
(`omni-cpu/src/dynarmic/callbacks.rs`, `cb_icache_op`). Pinned by
`bionic.rs::writable_executable_memory_runs_code_the_guest_writes_and_invalidates`. On Windows a
private W+X page is `PAGE_EXECUTE_READWRITE` and a private file view `PAGE_EXECUTE_WRITECOPY`; on
Apple Silicon a guest W+X mapping still needs `MAP_JIT` and is not yet wired, so the test is
skipped there.

**D12 (exception) — dynarmic's x64 code cache is W+X.** It is committed `PAGE_EXECUTE_READWRITE`;
`DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT=ON` makes upstream's suite segfault and `build.rs` refuses it.
Under D4 the cache is guest-writable in principle, guarded by ASLR alone; D38's shared cache too.
`dynarmic-sys/tests/a64_exec.rs` pins it. On macOS the cache is `MAP_JIT` with per-thread write
protection: W^X per thread (`tests/wx.rs`), though another thread's open write window can write it
(`ports/macos-cpu.md`). **Reverses it:** an upstream no-execute fix or a backend without W+X pages.

## D13 — `TPIDR_EL0` holds a bionic TLS block before any guest code runs

**Ruling.** Every guest thread gets a bionic-layout TLS block with the stack guard at `+0x28`
(slot 5), and `TPIDR_EL0` points at it, before its first instruction. One guard per process, from
the backend's TLS arena; `__stack_chk_guard` holds the same value; a zero canary is refused.

**Why.** 1,282 `MRS Xt, TPIDR_EL0` in `libroblox.so`, 1,276 reading `[Xt, #0x28]`, from the first
initializer on. Without it the first protected function crashes in a way that looks like a loader
bug.

**D13 (confirmed).** On real code (`libroblox.so + 0x2872aac`): a matching guard returns; a guard
changed between the two reads calls `__stack_chk_fail`; an unmapped `TPIDR_EL0` faults at exactly
tp + `0x28`. The second read goes through a kept register, so re-pointing `TPIDR_EL0` mid-function
changes nothing. Evidence: `omni-cpu/tests/thread_pointer.rs`, `roblox.rs`.

## D14 — `.bss` is committed eagerly: a recorded exception to D10

**Ruling.** `LoaderConfig::bss_commit` defaults to `CommitPolicy::Eager`: `libroblox.so`'s
11,575,296-byte `.bss` is ~11 MiB of a load's ~16.7 MiB. Lazy measures ~+5.4 MiB and is one field
away.

**Why.** Lazy `.bss` needs a first-touch driver, and there was none; eager is charge, not working
set. **Live.** The stated expiry (a commit driver exists) is met: guest `mmap` runs on the demand
pager (D21). The ~6 MiB per instance has not been taken back. Evidence:
`omni-elf/tests/loader_commit.rs` pins both figures.

## D15 — The commit ceiling is two bounds, anonymous commit only

**Ruling.** `omni_mem::DEFAULT_MAX_COMMIT_REQUEST` = 128 MiB per request (the security bound) and
`DEFAULT_MAX_COMMITTED` = 3.5 GiB total; the runtime sets the total to the device's RAM (D36).

**Why.** A tampered `p_memsz` (an 8-byte edit) committed +3,406 MiB with the load reporting
success. The largest real request is `.bss` (11.6 MB), the next 24,576 bytes; 128 MiB is 11.6x above
and 27x below the attack. The refusal is `CommitRequestTooLarge` with both sizes; a legitimate 3 GB
growth still succeeds.

**Not covered.** Copy-on-write charge from `protect` and page tables sit outside both (bounded by
file size, up to ~127 MiB per segment under the loader's 256 MiB cap).

## D16 — Stopping a runaway guest

**Ruling.** A runaway guest thread is stopped by short cycle-budget windows (the watchdog); a
cross-thread halt is honoured at the next window, never relied on alone. The flag set is
configuration: `dynarmic_sys::optimization::INTERRUPTIBLE` holds every flag whose terminal handlers
check budget and halt; since D33/D35 that is `ALL_SAFE` (`0x0000_FFFF`) on x86_64 and `0x0000_FFFB`
on aarch64.

**Why (27-cell matrix, one process per cell).** Upstream's `PopRSBHint` and `FastDispatchHint`
handlers checked neither budget nor halt; `LinkBlock` checks budget *or* halt; only
`ReturnFromRunCode` checks both. Clearing `BlockLinking` (`0xFFF8`) stops every shape at ~7x, the
expected cost since Roblox averages 4.30 instructions per block. Clearing RSB and FastDispatch
(`0xFFF9`) cost ~3.9 ns per indirect transfer until patches 0018-0020.

**Footgun.** The cycle comparison is signed: a budget above `i64::MAX` reads as spent (~100x
slower). Sleeping threads consume no budget; long guest waits are sliced instead (D22).
Evidence: `dynarmic-sys/tests/hostile.rs` `the_stoppability_matrix`; patches README "Known
candidates" 2.

## D17 — Imports are dispatched inside the run loop

**Ruling.** An import is serviced inside dynarmic's run loop (the `SVC` callback); the exit path
(`ExitReason::Thunk`) is for handlers that run guest code and for unresolved imports.

**Why.** Exit-per-call measured 80-105 ns and bimodal (unexplained), in-loop ~33 ns and stable: plan
against 3x. The halt check lands on `ReturnFromRunCode` (D16). An in-loop handler would run under the
guest's MXCSR (dynarmic's `SVC` emitter omits the switch), so the dispatcher sets the host's and
restores the guest's (1.1 ns, asserted from guest code under `FPCR.FZ`).

**Scope.** 188 imports are statically reachable from the 3,594 initializers (170 functions, 18 data
objects): a lower bound (17,698 unresolvable indirect calls), never a completion criterion. Runs
found more (`BEYOND_THE_PREDICTION`, `tests/bionic.rs`).
Evidence: `tools/thunk_sweep.py`, `research/thunk-sweep.txt`, `research/init-reachable-imports.txt`.

## D18 — The thunk boundary's shape

**Ruling.** An in-loop handler is `ImportFn` over `ImportCall`, which holds no CPU, so it cannot run
guest code (re-entering `od_jit_run` there would alias `&mut CpuCtx`). `ReentrantFn` over
`ReentrantCall` runs on the exit path and may; `ThunkCall::defer_to_caller()` escalates. Slots are
16 bytes (`region::VENEER_INSTRUCTIONS` = 4, room for an ARM64 host's veneer) in a non-executable
area, so a branch into a slot is refused by name. Re-entry depth is capped at
`boundary::MAX_GUEST_DEPTH` = 8 (`AbiError::TooDeep`), exit-path crossings by
`AbiError::CrossingLimit`, and a counted budget is spent down across crossings.

**Why.** Backend-neutral types (`ThunkRegs`, `set_return_sentinel` on `GuestCpu`,
`Capabilities::inline_thunks`) keep the ARM64-native path open (D34 used it). Measured then: in-loop
26.7-31.0 ns, exit 80-102 ns.

**AAPCS64 traps, each pinned by a test.** Variadic FP goes in `V0-V7` (not Apple's or Windows'
rule); a variadic `float` arrives as `double`; the SIMD save slot is 16 bytes; `va_list` is 32 bytes,
passed indirectly, and read only through `GuestMem`; `mallinfo` returns through `X8`.
Evidence: `omni-android/src/boundary.rs`, `region.rs`, `tests/libroblox.rs`.

## D19 — `omni-bionic` is a separate, zero-dependency crate

**Ruling.** bionic libc/libm lives in `omni-bionic` with an empty `[dependencies]` and no
`cfg(target_os)`; `omni-android` depends on it and holds the adapter.

**Why.** Zero dependencies make "no OS access" checkable by `cargo tree -p omni-bionic -e normal`.
The build-coupling argument for folding it in was wrong (`omni-android`'s normal dependencies exclude
`dynarmic-sys`). Folding back is mechanical if ever needed.

## D20 — The bionic adapter's shape

**Ruling.**
1. Per-instance state (`Bionic`) is reached through a thread-local `Activation` held across
   `Boundary::run`. No default instance (a per-call default would give two threads private copies of
   one mutex); a handler without one returns `AbiError::BionicNotActive`.
2. Guest-visible per-thread storage (`errno`, `strerror` buffers, `dl_phdr_info` slots) is an arena
   mapped once in `Bionic::new`, before any CPU exists (review finding F9: handlers never map).
3. A symbol that cannot be serviced correctly is bound to a refusal naming the missing piece; never
   left `Unbound` silently or stubbed with a believable answer.
4. Guest printf widths are bounded: `MAX_FIELD_WIDTH` 64 KiB per conversion (checked after `*`
   arguments) and `MAX_OUTPUT` 1 MiB per call.

**Superseded.** The futex once ignored `expected`; it now compares it atomically with the park,
because skipping it cost 1,000 ms lock stalls in the gate (`bionic/runtime.rs`). The bound set is
asserted against `init-reachable-imports.txt`, never counted (a counted "88" was really 79).
Evidence: `omni-android/tests/bionic.rs`.

## D21 — `dl*`, guest memory and the data symbols

**Ruling.**
- *Data symbols.* The 18 reachable `STT_OBJECT` imports are derived from `.dynsym` and the reachable
  list (a hand list was wrong by two each way with the right count); each has one `GLOB_DAT`, addend
  0. `__stack_chk_guard` is D13's canary; `environ` is an empty vector, not `NULL`;
  `AMEDIAFORMAT_KEY_*` are the real strings; calling a data symbol is `DataSymbolCalled`.
  `FILE_BYTES` = 152 is ASSUMED from bionic's headers (no NDK here); D23 says why that is safe.
- *`dl_iterate_phdr`* walks the images given to `Bionic::register_image` and refuses if none is: an
  empty answer is a success that breaks every C++ `throw`.
- *`dlopen`/`dlsym`* answer for the libraries this layer *is* (`dlopen("libc.so")` +
  `dlsym("getauxval")` at `init_array[3096]` get a thunk address) and refuse to load a file
  (`bionic/dl.rs`). They refused everything at first.
- *Guest memory* (`mmap`, `munmap`, `mprotect`, `madvise`, `mlock`) runs on the exit path because it
  changes mappings live translations depend on (F9); `munmap`/`mprotect` invalidate translations in
  every context (D24, D38). `mmap` is `CommitPolicy::Lazy`. `MAP_FIXED` refused,
  `MAP_FIXED_NOREPLACE` implemented; `MADV_FREE` implemented; `MADV_DONTNEED` by decommit (D28
  amendment 1); `MADV_REMOVE` and `mlock` refused; anonymous `MAP_SHARED` accepted (no `fork`).
- *The split used since:* cannot be done correctly -> `AbiError::Refused` naming symbol and argument;
  well-formed and legitimately failed -> what Linux returns, with `errno`.

Evidence: `tests/bionic.rs` `every_data_import_is_referenced_with_a_zero_addend`,
`dispatch_paths_are_what_f9_requires`.

## D22 — `omni-platform`'s five-target rule; process, clock and log

**Ruling.**
- *Five-target rule.* A primitive that one portable `std` call serves on all five hosts is written
  once, with no fabricated `Unsupported` arm; one that needs an OS API gets per-OS backends.
- *`AT_HWCAP`.* `bionic::HwcapPolicy` is `Undecided | Advertise | Decline`, no `Default`;
  `Undecided` refuses `getauxval(AT_HWCAP/AT_HWCAP2)` by name. The embedding chooses (D26).
- *Termination.* `abort`/`__stack_chk_fail` become `AbiError::GuestAborted`, `_exit`
  `GuestExited`: reported, never performed, because instances share a process.
- *Logging* is always serviced (no return value to get wrong), through the real printf engine,
  into a bounded ring (`LOG_CAPTURE_MAX` = 256 records, plus a byte bound).
- *Environment.* `getenv` and system properties are empty unless the host sets them
  (`Bionic::set_env`, `set_system_property`); the host's own environment is unreachable.
- *`gmtime_r`* is loop-free in `omni-bionic`; a year past `int` is `EOVERFLOW`.
- *Guest waits.* A sleeping thread consumes no budget (D16). Refusing waits past
  `MAX_SLEEP_SECONDS` (60) killed the join worker's legitimate ~120 s wait, so a long wait is now a
  series of host parks (at most 60 s, usually `STOP_SLICE` = 1 s) with the stop switch read between,
  and the guest's answer comes only at the real deadline or event.
- *`sysconf`* first refused (bionic's `_SC_*` numbers unverified); it now answers page size,
  processors (D37), `_SC_OPEN_MAX` and physical pages (D36).

**Live.** Windows sleep granularity (~15.6 ms) is guest-visible. Evidence: `tests/bionic.rs`
hostile-argument tests, `bionic/mod.rs`.

## D23 — Files: a guest cannot name a host file

**Ruling.** Every guest path resolves inside one host root (`Bionic::set_filesystem_root`, settable
once); with none, every path symbol refuses. `omni_platform::fs::path` applies six rules: length
(`PATH_MAX` 4096, `NAME_MAX` 255); UTF-8; lexical resolution before any host call (`..` stops at the
top); component hygiene on every target (`\`, `:`, wildcards, control characters, Windows device
names, trailing dot or space); no symlinks except a final `lstat` component; a containment
assertion. There is no working directory: relative paths resolve at the root.

**Why.** Guest code is untrusted (D6) and instances must not reach each other's files. Not race-free
against a symlink created inside the root while the guest runs (needs per-component `openat`).

**Other rulings.** A `FILE *` is a key into a host table; its bytes are zeroed and never read, so a
wrong `FILE_BYTES` gives a named refusal, not a wrong answer. Streams are unbuffered; transfers go
through a fixed 4 KiB buffer; `size * nmemb` is checked. `pread` restores the file position (Windows
`seek_read` moves it). `stat` (128 bytes), `statvfs` (112) and `dirent` (280) are ASSUMED layouts;
`st_ino` is a path hash, never 0. Refused by name: `access(X_OK)`, `O_SYNC`/`O_DIRECT`/`O_PATH`/
`O_TMPFILE`-class flags, `__open_2` with `O_CREAT`, a `__write_chk` overrun, a wild `FILE *`/`DIR *`,
and an unclassified host error (never `EIO`).

**Changed since.** `MAX_GUEST_FILES` 16 -> 512 after asset caches hit `errno=24`; the arena spans
`ARENA_GRANULES` = 5 granules (at most 256 KiB committed early per instance).
Evidence: `tests/bionic.rs` (a bait file outside the root, eleven traversal shapes).

## D24 — Guest threads, and signals

**Ruling.**
- `pthread_create` builds its context with `GuestCpuBackend::create_guest_thread`, whose TLS block
  comes from the backend's arena (D13). Create/join/detach run on the exit path via
  `ReentrantCall::boundary()`. A created thread runs in short budget windows and re-reads the stop
  switch (D16); `Bionic::join_guest_threads(timeout)` is required before teardown (D29).
- POSIX errors are return values (`EAGAIN`, `ESRCH`, `EINVAL`, `EDEADLK` with wait-chain cycle
  detection). Refused: no thread host, a `NULL` start routine, joining a thread that stopped without
  returning. Detached failures go to `Bionic::guest_thread_failures`.
- `pthread_getschedparam` answers `SCHED_OTHER`/0; forced while nothing reachable can set a policy.
- Invalidation reaches every context: `Boundary::run` registers each, `invalidate_code` applies
  locally and queues for the rest (a full queue collapses to the whole space), drained at their next
  run segment. Under D38 it is applied once to the shared cache.
- Signals: `raise`, `pthread_sigmask` and `longjmp` are refused (no delivery exists; a `0` from
  `raise(SIGABRT)` runs past a failed assert). `sigfillset` is implemented. `sigaction` answers
  `SIGPIPE` only, with `SIG_DFL`/`SIG_IGN` (libcurl's `sigpipe_ignore`).
- When a start routine returns, C++ `thread_local` and then `pthread_key` destructors run through
  `Boundary::call_guest` (`bionic/threads.rs`).

**Changed since.** `MAX_GUEST_THREADS` 64 -> 256 after a world join ran past 64. The 24.8 MiB per
thread measured here is gone (D5 risk 2). Evidence: `tests/bionic.rs`, `tests/thread_memory.rs`.

## D25 — The last reachable imports, and two that must resolve to nothing

**Ruling.**
- *`__gcov_dump`, `__gcov_flush` resolve to nothing.* The one site (`0x6194be8`) null-tests each GOT
  slot and otherwise calls it and then `BL abort`: a stub would abort four bytes later.
  `BoundaryBuilder::declare_absent` is per symbol and requires a weak reference; the other weak
  imports (`__cxa_thread_atexit_impl`, `gettid`, `getentropy`) are supplied, as on a device.
- *`inet_ntop` follows BIND*, not Rust's `Ipv6Addr` `Display` (they differ on IPv4-compatible
  addresses, 43 of 200,000); the differential test is committed.
- *`select`* leaves the guest's sets untouched on failure.
- *Clocks:* `CLOCK_PROCESS_CPUTIME_ID` is answered (`process::cpu_time`);
  `CLOCK_THREAD_CPUTIME_ID` and `CLOCK_BOOTTIME` are refused.

**Superseded.** The network refusals and the no-OS `poll`/`select` gave way to D29 (pipes) and D30
(sockets). `mallinfo` now answers zeros (the engine stopped on the refusal); `longjmp`, `vasprintf`
and `fscanf` stay refused. Evidence: `tests/libroblox.rs`
`the_two_gcov_imports_are_weak_null_tested_and_left_unresolved`, `bionic/absent.rs`.

## D26 — `AT_HWCAP` is declined

**Ruling.** The embedding runs with `HwcapPolicy::Decline` (`AT_HWCAP` = `AT_HWCAP2` = 0, an ARMv8.0
device), set explicitly (`omni-android/tests/gameactivity.rs`); `Undecided` stays the start state.

**Why.** Advertising `HWCAP_ATOMICS` sends the engine to its 53 LSE sites, which this dynarmic does
not implement: the thread stops with `UnsupportedInstruction` (D5 risk 4). This record once argued
both arms "execute correctly" from the spike's ~87 ns trap figure; that was wrong. Declining uses the
106 exclusive fallbacks, whose global-lock cost D31 removed.

**Reverses it.** LSE implemented in the backend and measured against value-compare. Advertising any
other capability is a new decision.

## D27 — Texture transcoding: ETC1 only, to RGBA8, in a crate that cannot reach the OS

**Ruling.** `omni-texture` decodes `GL_ETC1_RGB8_OES` to RGBA8 and refuses every ETC2, EAC, ASTC and
S3TC enum by its specification name; a block that escapes into an ETC2 mode fails naming it. It is
`#![no_std]`, has zero dependencies and allocates nothing; `omni-gfx` is its only dependent.

**Why.** Census first (`tools/texture_census.py --check`): 38 compressed textures, all ETC1 (the
twelve `.tex` files are the KTX1 skybox), no ASTC/ETC2/EAC/PVRTC/KTX2 bytes, and none of 813,802
blocks in an ETC2 mode. The host GPU samples neither ETC2 nor ASTC. ETC1 -> RGBA8 is exact and
testable from the spec; re-encoding to BC1 has no unique answer. Cost: 39.0 ns/block, 31.7 ms for
the whole set (n = 11, one core), 52,079,224 bytes decoded.

**If wrong.** An ETC2/ASTC asset is a named refusal and a decoder is additive. Open: whether the
engine loads baked ETC1 when ETC1 is not advertised. Evidence: `research/texture-formats.md`.

## D28 — JNI without a JVM

**Ruling.** `omni-android/src/jni/` implements `jni-surface.md` §8 steps 6-12: `JavaVM` and `JNIEnv`
in guest memory, a class and member registry, handles as checked indices, a host-owned startup
script. It does not violate D7: nothing loads code. `ClassLoader.loadClass/findClass` is a name
resolver (`Answer::ResolveClass`); `DefineClass` refuses. Every interface slot has a thunk and the
unimplemented ones refuse naming themselves (`AbiError::JniRefused`). An undecided member is
`Answer::Unanswered` and refuses when called; `Jni::define` decides.

**Registry.** `classes::DECLARED` (hand-written) wins over `surface::DEX_SURFACE` (generated by
`crates/omni-android/tools/gen_dex_surface.py`: 98 classes, 1,526 members; the stock dex regenerates
it identically). A missed lookup returns `NULL` with a pending exception and is recorded in
`Jni::misses`. Measured then: `JNI_OnLoad` returns `0x00010006`, 0 misses.

**D28 amendment 1.** (1) `MADV_DONTNEED` is implemented by decommit: zero-fill on the next touch
meets the contract without writing, so D21's refusal rested on a false premise. (2) `__strncpy_chk2`
fails only when no NUL lies in the first `min(n, src_size)` bytes. (3) §8 step 9's order is wrong:
the directory calls run before `nativeInitFastLog` (`script::SEQUENCE`). Also: `getcwd` answers `/`
(the root, D23); `getProcessTimestamp` assumes epoch milliseconds; `sizeof(JavaVMAttachArgs)` = 24
is ASSUMED. Evidence: `tests/jni_startup.rs`, `jni/classes.rs`.

## D29 — `initializeNativeCode` returns; instruments before silent failures

**Ruling.** §8 step 13 is reached by implementing what GameActivity needs (§5.2: a pipe with real
readiness, an `ALooper`, an `AAssetManager`, an `AConfiguration`; `omni-android/src/ndk/`) and by
instrumenting §8.1's two silent failure modes first: `Ndk::prepare_looper`, so a gate asserts a
looper exists (`ALooper_forThread` answers `NULL` when none does), and `Bionic::parked()`, listing
every thread in `pthread_cond_wait` with its cond, mutex and duration.

**Why it paid.** The cond-wait list showed the game thread dying in `AConfiguration_new` because
created threads carried only the bionic instance (`ThreadHost::with_instance` fixed it). Teardown
with live guest threads crashed, so `Bionic::join_guest_threads` is required; a `Drop` cannot run
while threads hold the `Arc<Bionic>`.

**Rules kept.** Readiness is `Filesystem::readiness`, a `match` with no default arm, so a new
descriptor kind must decide what `poll` says. A pipe needs no OS. The readiness generation is read
before each attempt (`VERIFICATION.md` entry 11).

**Corrections.** `libroblox.so` imports five `ANativeWindow_*` (the APK's nine include two other
libraries'). `AAsset_read` is bound although no stock arm64 library imports it; the evidence once
cited was the trojanised zstd (D6). Raw `syscall` 98 (futex), once the largest blocker, is answered
now, with `gettid`, `getrandom`, `statfs`, `fstatfs` and `rt_sigprocmask`.
Evidence: `tests/gameactivity.rs`.

## D30 — Global Constraint 8 is withdrawn: the guest gets a real network

**Ruling (owner, 2026-09-22).** "Playable Roblox is the higher-priority requirement. Networking is
allowed and required. ... Implement the smallest correct networking surface Roblox actually
exercises, preserving instance isolation and portability." Global Constraint 8 ("no network access
at runtime") no longer holds.

**What replaces it.** A policy the embedding sets, shaped like the filesystem root.
`omni_platform::net` is the only place socket syscalls are made; `NetPolicy` is closed by default and
a socket cannot be built without one. Its gates: host suffixes that may be *resolved*, destination
ports, loopback. The host gate stops at the resolver: an address obtained another way reaches any
admitted port. Sockets share the descriptor table with files, so `poll`/`select`/`close`/`fcntl`
see one space and isolation stays per `Bionic` instance. The engine's TLS is its own, so this layer
owes sockets and DNS only.

**Rules kept.** Bind a network symbol when a run reaches it, not because it is imported. A
diagnostic failure such as `EAI_NONAME` may only sit behind a loud, explicit switch, never as a
default or a tested contract (`VERIFICATION.md` entry 14).
Evidence: `omni-platform/src/net/policy.rs`, `bionic/net.rs`.

## D31 — Guest exclusives are atomic by value-compare, not the global monitor

**Ruling (2026-09-24).** `ExclusiveMonitor::ValueCompare` (dynarmic's `Unsafe_IgnoreGlobalMonitor`)
is the default; `OMNI_JIT_EXCLUSIVE_MONITOR=global|value` overrides it, announced on stderr.

**The arms.** Both make an exclusive store one host compare-and-swap against the value the thread's
exclusive load read. Global adds a process-wide spin lock on every exclusive access and an inline
scan of every monitor slot on each store. Value-compare keeps a per-processor reservation, slots 8
apart (`VALUE_COMPARE_SLOT_STRIDE`). The one difference: ABA across another thread's exclusive store
fails under Global and succeeds under value-compare, which is the C++ semantics of every LLVM
retry loop in `libroblox.so`.

**Why.** ns per guest atomic increment (median of 7): global, 256 slots, 131.4 / 511.1 / 836.2 (1
thread / 8 private / 8 shared) against value-compare 12.8 / 2.4 / 46.1. In a world (n = 1, global)
busy workers spent 3-8.5% of samples at the monitor. Value-compare has not been measured in a world.
**Reverses it:** a lost update or hang under value-compare that `global` does not show.

**D31 amendment 1 — patch 0021.** On arm64 the flag did nothing (patch 0007's inline exclusives
lacked x64's early returns), so macOS ran the global monitor; in a world the lock capped the process
near 400 M instructions/s. 0021 adds the early returns: M1 value-compare 1,306.9 / 1,943.5 / 1,963.7
ns before, 9.5 / 3.2 / 42.4 after. `omni-cpu/tests/exclusive.rs` runs on aarch64 too; hand
mutations `mac-cpu-V1..V3` are caught.
Evidence: `omni-cpu/tests/exclusive.rs` (`the_runtime_default_is_value_compare`, the ABA test),
`dynarmic-sys/tests/exclusive.rs`, `research/perf-world.md`.

## D32 — A guest thread's fixed JIT cost is paid on demand: patch 0017

**Ruling.** The fast-dispatch table is allocated only when `FastDispatch` is on, and a code cache's
prelude commits 2 MiB instead of 16; later blocks commit through dynarmic's 1 MiB-ahead
`EnsureMemoryCommitted`. A per-thread cache reserves 32 MiB (`CODE_CACHE_BYTES` in the gate,
`OMNI_JIT_CACHE_MB`) and commits what it translated plus ~1 MiB.

**Why.** Owner requirement "strictly on demand" (D10). On a logged-out landing (44-45 threads) the
table cost 704 MiB and the prelude 882 MiB committed for 432 MiB used; after, commit 3,157 -> 2,105
MiB, working set 2,528 -> 1,884 MiB (n = 1 each). **If wrong,** a prelude over 2 MiB faults in the
first `Jit` constructor. Evidence: `dynarmic-sys/tests/pin_constants.rs`; patches README 0017.

## D33 — The return-stack buffer checks budget and halt: patch 0018

**Ruling.** On a confirmed `PopRSBHint` hit the x64 handler compares `cycles_remaining` (when
counting) and `halt_reason` with 0 and leaves through `ReturnFromRunCode` on either. A hit is a
verified prediction, so a callback that rewrites the PC resumes at that PC.

**Why.** With the RSB off (D16) every `RET` took the dispatcher's lookup: `BL`+`RET` at 262,144 blocks
cost 132.02 ns under `0xFFF9`, 22.59 ns with 0018. The stoppability matrix grew to 39 cells; hand
mutations of each check wedge their cells; row `rsb-A1` is caught.

**D33 amendment 1.** 0018 is x64 only; arm64's `PopRSBHint` had no checks (a `RET` loop was
unstoppable under `0xFFFF` on the M1). **D33 amendment 2.** Both halves done in D35.

## D34 — A native CPU backend on Apple silicon: measured, not adopted

**Ruling.** dynarmic stays the only backend any gate or run uses. `omni-cpu`'s `native` module
(feature `native-hvf`, off by default, arm64 only) runs the guest at EL0 under Hypervisor.framework,
stage 2 at IPA == VA, and passes the M2 gate and the 3,594 initializers.

**Why (M1, release, 2.738.1397's `libroblox.so`).** Native compute is 5.7x faster (7.55 against
1.32 G insn/s), initializers take 795 against 2,417 ms (n = 8), 79 KiB per thread; but an import
crossing is a VM exit (1,614 ns against 23.7 ns), and working threads cross 0.40-0.52 M times a
second at the landing screen. Break-even is ~2 us of guest code between host calls.

**Adopt when all hold, on the gate:** hot imports (`pthread_getspecific`, `__errno`,
`clock_gettime`, `mem*`/`str*`, uncontended mutexes) served in guest code, under 50k exits/s; no
counted budget passed to a backend that cannot count; the macOS gate passes natively with every
host-pointer path closed (D4 amendment 1 becomes required); more than 64 guest threads; the binary
carries `com.apple.security.hypervisor`, one instance per process.
Evidence: `ports/macos-hvf.md`, `omni-cpu/tests/native.rs`.

## D35 — Fast dispatch (x64) and the RSB (arm64) check budget and halt: patches 0019, 0020

**Ruling.** `INTERRUPTIBLE` is `0x0000_FFFF` on x86_64 and `0x0000_FFFB` on aarch64. Patch 0019 makes
the x64 fast-dispatch handler check budget and halt after `rsb_cache_miss`, before the probe, and
shrinks its table to 4,096 entries (64 KiB per thread, now allocated). Patch 0020 gives arm64's
`PopRSBHint` 0018's checks, out of line in one prelude handler. arm64 does not implement
`FastDispatch`, so it stays clear there.

**Why.** [2.739.691] In PS99 the dispatcher's `GetBasicBlock` was ~12% of samples outside translated
code, ~25% on the render thread. Synthetic benchmarks (ns): x64 `BR` through 16/256/4,096 targets
17.7/20.7/26.0 -> 9.9/10.1/14.8; arm64 `BL`+`RET` at 262,144 blocks 24.6 -> 20.2 (inline checks would
be 47.4). A 2^12 table matches 2^14 and 2^16; only 2^20 helps past 16,384 targets.

**If wrong.** A stale entry would run old code; invalidation clears it (`Unpatch`,
`ClearFastDispatchTable`; tested in `a64_exec.rs`). Evidence: the stoppability matrix (57 cells x64,
45 arm64), hand mutations of 0019/0020, patches README.

## D36 — The device's RAM: 60% of host memory in whole GiB, at most 8 GiB

**Ruling.** The guest's RAM (`MemTotal`, `sysinfo.totalram`, `_SC_PHYS_PAGES`,
`ActivityManager.MemoryInfo.totalMem`) and its commit ceiling are one figure:
`OMNI_GUEST_MEMORY_MB` if set (128..=16384, else refused by name), otherwise 60% of
`omni_platform::vm::physical_memory()` rounded down to whole GiB, clamped to 1..=8 GiB, printed as
`MEMORY: the guest is a <n> MiB device (<why>)`. The 16 GiB reservation is unchanged.
`Bionic::set_memory_budget` has no default; the embedding (`tests/gameactivity.rs`) states it.

**Why.** [2.739.691] On the 7.2 GiB Linux host a fixed 8 GiB device reached 4.6 GiB private with 3.3
GiB resident in PS99 and ran at ~1 fps while swapping; the engine sizes caches and tier from
`MemTotal`. 60% leaves room for the desktop, our heap and JIT caches, and the file cache. Hosts:
Windows 31.8 GiB -> 8 (cap), M1 16 GiB -> 8 (cap), Linux 7.2 GiB -> 4.

**If wrong.** Too generous pages later, too mean picks a lower tier; the variable fixes either.
Evidence: `physical_memory` unit tests per OS; rows `devmem-*`.

## D37 — The device has at most 8 processors

**Ruling.** `sysconf(_SC_NPROCESSORS_CONF/ONLN)` answers `min(8, host)` (`procenv::DEVICE_CPUS`), or
`OMNI_GUEST_CPUS=<n>` clamped to the host's count, printed once as `CPUS: ...`.

**Why.** Processor count is a device property, like screen and RAM. The engine sizes its
TaskScheduler from it (16 workers on the D2 host, 8 when told 8) and each worker is a guest thread.
[2.739.691] PS99, Windows, n = 1 each: told 24, 48.2 fps and 4.65 GiB private; told 8, 49.2 fps and
3.92 GiB. **If wrong,** a workload needing more than 8 workers leaves cores idle; none measured did.

## D38 — One translation cache per guest address space: patch 0022 and successors

**Ruling.** On x64 every guest thread of an address space runs one shared translation cache
(`DynarmicOptions::shared_code_cache`, **default on for x86_64**; `OMNI_JIT_SHARED_CACHE=0|1`
overrides). arm64 keeps a cache per thread (the shim refuses a shared one; omni-cpu falls back and
says so). Defaults: 1 GiB reserved (`SHARED_CODE_CACHE_BYTES`, `OMNI_JIT_SHARED_CACHE_MB`, 64-2048),
16 MiB regions (`SHARED_CODE_REGION_BYTES`, `OMNI_JIT_SHARED_CACHE_REGION_MB`), 256 MiB live
(`SHARED_CODE_LIVE_BYTES`, `OMNI_JIT_SHARED_CACHE_LIVE_MB`); past that the oldest region is retired.

**Shape.** Shared: prelude, constant pool, every block, block map, guest-range index, link slots,
fastmem fault table. Per thread: `JitState`, RSB, the 64 KiB fast-dispatch table, monitor slot,
callbacks, budget, halt word, pending invalidations, epoch. Links go through 8-byte slots so shared
code is never rewritten; translation runs outside the lock, emission inside. Breakpoints are refused
and recompile-on-fastmem-failure is off under the shared cache. D12's exception stands.

**Why.** [2.739.691] Per-thread caches translated engine code once per thread: in PS99, input stalls
of seconds while translation jumped to 150-310k instructions/s, and ~0.4-0.5 GiB of duplicate code
per instance. Synthetic real-code bench (eight threads, the same 870 leaves): instructions fetched
for translation 50,904 -> 6,363, commit +54 -> +2 MiB, per-thread cost 4.548 -> 0.055 MiB. Trade:
emission is serialized (Windows cold 22 -> 31 ms, Linux 63 -> 43 ms), and a dispatcher lookup that
misses the thread's table takes a shared lock (+15-50 ns at large indirect working sets).

**D38 amendment 1.** [2.739.691] One A/B run thrashed: threads parked in `SVC` callbacks held pages of
retired regions as holes, regions never came back whole, and reclaim retries (IPI barriers) put
threads 73-85% in the kernel. Now shared code calls every `SVC` callback through a prelude trampoline
that publishes the resume address, and the reclaimer moves a parked thread off a retiring region
with one compare-exchange. `OMNI_PERF` prints a `PERF jit cache:` line. Detector:
`threads_parked_all_over_a_region_do_not_fragment_it`.

**D38 amendment 2 — on by default on x64.** [2.739.691] PS99 with w20's drag script, shared (w27,
w29) against per-thread (w28): 0 s under 20 fps during input against 14 s, translation peak 16 against
326 k insn/s, 3.2 against 4.1 GiB private, settled fps unchanged (48-52 against 49).

**D38 amendment 3 — a full region is not a flush (patch 0028).** Filling a region used to forget
every block. Now full regions stay live; past the live limit only the oldest region is retired and
only its blocks forgotten (a block re-translated into a newer region survives). Synthetic eviction:
~18 ms on Windows, ~41 ms on the 4-core Linux host (16 MiB regions, 128 MiB live). Hand mutations
S14-S19 are caught (`mutate_0022.py` 19/19). Patches 0024-0027 compacted the per-block tables.

**D38 amendment 4 — 256 MiB live, not 128.** [2.739.691] At 128 MiB the settled PS99 world evicted
all session (w35: evictions on 31 `PERF` lines, ~60 s at 0-18 fps), against none at the old capacity
(w32). A world emits ~245 MiB in its first minutes, then ~0.4 MiB a minute. A memory-first
multi-instance setup can set `OMNI_JIT_SHARED_CACHE_LIVE_MB=128` and accept the churn.

**Reverses it.** A wrong result under the shared cache that per-thread caches do not show, or a
settled-fps loss traced to locked lookups. The initializer gate's flakiness under it was the test's
(`VERIFICATION.md` entry 22). Evidence: `dynarmic-sys/tests/shared_cache.rs`,
`crates/dynarmic-sys/tools/mutate_0022.py`, `research/shared-jit-cache.md`, patches README.

## D39 — The real AOSP userspace on a Linux kernel personality (reverses D7)

**Ruling (2026-09-27, the owner).** Any Roblox APK, future versions included, must run without
omnidroid being updated for it: every `classes*.dex` loaded into the app's class loader and run,
every `.so` loaded when code asks for it, as on a device. Transcribing the Java side (`jni/surface.rs`,
`jni/classes.rs`) and emulating bionic function by function are per-version by construction, so
omnidroid moves down a level: it emulates the **Linux kernel** (`omni-linux`), and the real AOSP 15
`linker64`, bionic and (sub-project B) ART run unmodified as guest code. Four sub-projects:
A kernel personality, B ART, C binder and services with Roblox's own `Application` and
`PathClassLoader`, D Roblox in a world on the new path, then the transcription retired
(`docs/superpowers/specs/2026-09-27-linux-abi-layer-design.md`). The current Roblox path is
untouched until D.

**A1, verified on Windows.** The real `toybox echo hello` from the pinned image
(`arm64-v8a-35_r02.zip`, sysroot manifest sha256 `5b586655...`, reproducible: entries sorted by path) runs through the real `linker64`
and `libc.so` (scudo, `libcrypto`'s self-test) and exits 0; `uname -a` and `ls` too
(`omni-linux/tests/a1_toybox.rs`, `omni-linux-run`). Only liblog's `socket` to logd is refused.

**What A1 found that the plan did not list.**
- **Top Byte Ignore.** Android 15's scudo tags every heap pointer (`orr x9, x0, #0x200000000000000`)
  and bionic adds `0xb4`; arm64 Linux gives user space TBI. `DynarmicOptions::top_byte_ignore`
  (off by default: the Roblox path keeps D4's 64-bit identity mapping) covers 56 address bits,
  mirrored, which is dynarmic's `shl`/`shr` mask, and clears the tag on the callback path.
- **Patch 0029.** dynarmic's x64 `LDAR` was `lock xadd [addr], 0`, which faults on a read-only
  page and degrades the site for good; bionic's `malloc` reads its write-protected globals that way.
  Now a plain `mov` (x86 loads are acquire; ordered stores are `xchg`).
- **Windows filenames.** Seven AOSP ringtones differ only by case, so the sysroot is stored by
  content (`objects/<sha256>`), with symlinks kept in the manifest.

**Reverses it.** Nothing planned; a sub-project that cannot reach its milestone is reported, not
worked around with transcription.

**D39 amendment 1 — sub-project A complete on Windows (2026-09-27).** A2 `/proc` and `/sys`
generated from the process (`procfs`); A3 system properties in bionic's own formats (`props`; the
properties come from the image's system, system_ext and product partitions plus an omnidroid
overlay -- the emulator's vendor partition is left out -- and init's derived `ro.product.*` and
fingerprint); A4 real threads (`clone`, a futex queue with requeue and wake-op, `exit_group`
halting every task); A5 signal delivery (the kernel's frame; bionic on arm64 sets no
`SA_RESTORER`, so a one-page `[vdso]` carries `__kernel_rt_sigreturn`). A4 and A5 are proven by
C programs built with NDK r28c (`tests/fixtures`). A1 also verified on Linux x86-64 (no change)
and macOS arm64 (the host page is the guest page, 16 KiB). The A1 review's Critical finding -- a
writable mount escaped with Windows path syntax -- is fixed (`vfs::host_path`).

## D40 — Headless mode: the frame's draws dropped where the engine cannot see it, reversibly

**Ruling (2026-09-27, the owner's requirements).** An instance nobody watches (a farm, a notebook, a
GPU container) keeps running and presenting every frame while the GPU draws none of them, and can
be turned back at runtime; a screenshot renders the moment's frame for real. `omni_android::headless`:

- **What is dropped: the draws into the frame's own render targets, nothing else.** Vulkan at
  record time (recording goes straight to the driver, every command buffer is primary):
  `vkBeginCommandBuffer` latches the recording's mode; `vkCmdBeginRenderPass` onto a per-frame
  framebuffer opens a dropping pass, which is itself recorded (clears, stores, layout transitions
  happen); `vkCmdDraw`/`vkCmdDrawIndexed` inside it are not. GLES at the call (`dispatch`):
  `glDraw*`, `glMultiDraw*`, `glClear`, `glClearBuffer*`, `glBlitFramebuffer` into a per-frame draw
  framebuffer. Barriers, copies, uploads, compute, shader compiles, queries, fences, submits,
  presents and swaps are forwarded, so every call answers as before and every fence is real. The
  engine's GPU timer (two timestamps) reads the last real frame's GPU time while headless.
- **Per-frame** (`headless::history`): a framebuffer with a swapchain image view (GL: framebuffer
  0), or one rendered in at least 4 of the last 8 frames (double-buffered targets included). A
  target drawn now and then (a composited avatar, a cached GUI) stays real, so nothing the engine
  renders once and keeps is lost. MEASURED: PS99 at quality 10 has 40 targets, 20 per-frame; every
  draw fell in a per-frame one (w2: 2.2 M dropped, 0 made into a kept target); GLES 110 targets,
  4 per-frame, ~0.5% of draws kept real.
- **Screenshot**: from the request on every recording is real; while headless it waits 3 frames
  (targets that feed the next frame are whole again), then reads that frame back before it is
  presented -- Vulkan: a copy on the present queue inside `vkQueuePresentKHR`, waiting on the
  present's own semaphores (the present then waits on none); GLES: `glReadPixels` of framebuffer 0
  before the host's swap, bindings restored. PNG off-thread (`flate2` + `crc32fast`, no new crate).
  **The host swapchain gets `TRANSFER_SRC`** added to the guest's usage when the surface supports
  it -- invisible to the guest, the images are the same (owner's ruling; `read_presented_image`'s
  old stance was test-only).
- **Control**: the gate reads stdin and `OMNI_CONTROL=<file>` (append-only) each turn: `headless
  on|off`, `screenshot <path>`, `status`; answers `CONTROL: ...`, `SCREENSHOT: saved <path> <w>x<h>`.
  `omnidroid --headless` (`OMNI_HEADLESS=1`), `--control <file>`, `--no-window`.
- **No display** (`--no-window`, or `--headless` where no window opens): `RawWindow::Headless`, no
  Vulkan bound (the engine logs `Mode 6 failed: Unable to load Vulkan API` and takes GLES), EGL on
  `EGL_PLATFORM_DEVICE_EXT` (the first hardware device; a software one under
  `LIBGL_ALWAYS_SOFTWARE=1`) else `EGL_PLATFORM_SURFACELESS_MESA`, a pbuffer of the window's size;
  `eglChooseConfig` asks `EGL_PBUFFER_BIT` for `EGL_WINDOW_BIT` (recorded).

**Why.** Rule 1 forbids fabricated answers, and none is made: every return value is the driver's;
what is withheld is work whose only product is pixels nobody reads, in targets redrawn the next
frame -- which is why `off` needs no repair. Dropping at present (skipping frames) would change
the engine's frame pacing and its own state; withholding the surface (`set_surface_withheld`) makes
the engine destroy its framebuffer, which it notices.

**Evidence** (stock APK sha256 `bbe00ae3...2742`, PS99 8737899170). Windows RTX 4060, Vulkan, w2
(2304x1296, `SavedQualityLevel` 10): the engine's GPU timer (`GPU TIMER` lines, the driver's
answer) 3.7-4.6 ms per frame drawn, 0.87-1.13 ms headless, back on `off`; the game's 3D engine
share (Windows GPU counters) 22.5% off, 7.8% headless, 23.3% off again; fps unchanged (the engine's
cap). `nvidia-smi` overall utilisation stayed ~35% throughout: this desktop's other clients (Parsec,
DWM) hold that much with no game running. Linux Quadro 4000 (nouveau, GPU-bound): X11 (l1) 12 fps
drawn, 18-21 headless; no display (l2, EGL device + pbuffer) 15 drawn, 22 headless; llvmpipe, no
display (l3): the engine accepts it (`GL Renderer: llvmpipe`, SuperHQ shaders excluded), 4-6 fps
drawn, 18-21 headless. Screenshots while headless show the live world; after `off` the window
draws again (w1, w2 desktop captures). Every session closed clean.

**Costs, stated.** While headless a visible window shows the last real frame (Windows: the
swapchain images keep it) -- cosmetic. The engine's own screenshot features read stale pixels. A
target rendered in 4 of 8 frames and then kept (none measured) would be stale until next drawn.
Compute dispatches, clears and copies still run (the ~0.9 ms left in w2). GLES has no GPU-timer
rewrite (the engine's GLES timer queries are forwarded as they are).

**Reverses it.** A target the rule drops that the engine keeps (a stale texture after `off`), or
an engine path that reads back a per-frame target while headless and acts on it.

