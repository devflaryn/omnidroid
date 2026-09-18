# Omnidroid Architecture

Omnidroid runs the ARM64 Android Roblox engine on desktop through a targeted compatibility layer.
It is not an Android emulator and contains no virtual machine, container, or hypervisor.

This document is written **after** the research in `docs/research/`, and every design choice below
traces to a measurement or a verified observation recorded there. Where something is still
unverified it says so. Rationale for the bigger calls lives in `docs/DECISIONS.md`.

---

## 1. The central idea

Conventional emulators give the guest its own address space and translate every guest address into
a host address. Omnidroid does not. **A guest pointer is a host pointer.** Guest ARM64 code is
loaded into the host process's own address space at its own addresses, and guest loads and stores
touch host memory directly.

Three requirements from the project goal fall out of this one decision:

- **Performance.** There is no address translation on the memory path at all: no page table walk,
  no base-register add, no bounds check. This is the largest single performance lever available.
- **Demand-driven memory.** Guest `mmap` becomes a host reservation plus a lazy commit, and guest
  `munmap` becomes `MEM_DECOMMIT`, which measurably returns commit charge. There is no preallocated
  guest RAM blob to balloon, because there is no guest RAM. There is only host memory.
- **Zero-copy API boundaries.** When the guest hands a pointer to Vulkan, the host driver reads it
  directly. No struct marshalling, no bounce buffers.

The cost is that Omnidroid gives up the memory isolation an emulator gets for free: buggy guest
code can corrupt the host process. That is accepted deliberately, and it is why **instance
isolation is done at the OS process level** (section 7) rather than inside the address space.

```
          one OS process per instance
  +-------------------------------------------------+
  |  host code (Rust)        guest code (ARM64)      |
  |  - runtime core          - libroblox.so          |
  |  - bionic/JNI impl  <->  - 3,594 init_array      |   same address space,
  |  - Vulkan forward        - engine threads        |   identity mapped
  |  - window/input                                  |
  +-------------------------------------------------+
        |                                    |
   host OS (Win/Linux/macOS)          host GPU via Vulkan
```

---

## 2. Module structure

A Cargo workspace. The dependency direction is strictly downward: `omni-core` defines traits, the
backends implement them, and nothing in the core knows which backend it has.

| Crate | Responsibility | Platform-specific? |
|---|---|---|
| `omni-platform` | OS primitives: virtual memory, threads, files, dynamic loading, clocks, windowing. One module per OS behind one trait set. | **yes**, the only place `cfg(target_os)` is allowed |
| `omni-mem` | Guest address-space manager built on `omni-platform`: reservation, lazy commit, decommit, placeholder mapping, the JIT code arena | no |
| `omni-apk` | APK reading, zip parsing, and the content-addressed 4 KB-aligned extraction cache | no |
| `omni-elf` | Bionic-compatible ELF loader: program headers, APS2 packed relocations, symbol resolution, `init_array`, RELRO, `dl_iterate_phdr` state | no |
| `omni-cpu` | `GuestCpu` trait plus backends: native execution on ARM64 hosts, binary translation on x86-64 hosts | backend-specific |
| `omni-android` | The compatibility layer: bionic libc/libm, `libdl`, `liblog`, JNI without a JVM, GameActivity, `ALooper`, `AAssetManager`, `ANativeWindow` | no |
| `omni-gfx` | Renderer abstraction and the guest-facing `libvulkan.so`/EGL/GLES surfaces; Vulkan backend now, D3D12/Metal later | backend-specific |
| `omni-core` | Instance lifecycle, orchestration, configuration, diagnostics; owns the traits | no |
| `omni-cli` | Command-line host, the first deliverable. Execution before UI. | no |

**Portability rule.** Anything that is not `omni-platform` or a named backend must compile for all
five targets without `cfg`. Linux and macOS support is *structural* until it is actually tested on
those systems; nothing will be described as working there before then.

---

## 3. APK handling and the extraction cache

The APK is a zip. Its 11 `.so` files are DEFLATED and only 4-byte aligned, so, as measured, none
can be mapped in place: placeholder mapping needs a 4 KB-aligned file offset, and a misaligned
offset fails with `ERROR_MAPPED_ALIGNMENT`.

So each `.so` is decompressed **once** into a shared, content-addressed cache, written 4 KB-aligned:

```
<cache-root>/libs/<sha256-of-entry>/libroblox.so     (4 KB-aligned, immutable)
```

Keyed by content hash rather than by APK path, so different APKs sharing a library share the cache
entry and a modified library can never collide with a stock one. The cache is the **only** state
shared between instances, and instances open it read-only.

This matters for the multi-instance requirement. Cache files are mapped file-backed
`PAGE_EXECUTE_READ`, so the roughly 109 MB of `libroblox.so` text and rodata is backed by the file
and shared across instances at near-zero marginal commit charge. Only private pages, meaning the
5.2 MB RELRO region after relocation plus `.data`, `.bss` and heap, cost per-instance commit.

Mechanical requirement found by measurement: the file must be opened `GENERIC_READ |
GENERIC_EXECUTE` and the section created `PAGE_EXECUTE_READ`, or `.text` can never be made
executable later.

Assets are a separate concern: `AAssetManager` reads them from the APK on demand. Large STORED
assets such as the 14.7 MB SPIR-V shader pack can be mapped directly when 4 KB-aligned, and are
otherwise streamed.

---

## 4. ELF loading

A bionic-compatible loader written from scratch, since no permissively-licensed one exists (D3).
Requirements are taken from the actual binary rather than from the ELF spec in general:

1. **APS2 packed relocations are mandatory.** `libroblox.so` carries `DT_ANDROID_RELA` and has
   **no `DT_RELA` and no `DT_RELR`**. Its APS2 blob is 2,100,778 bytes holding **568,272**
   relocations (568,194 `R_AARCH64_RELATIVE`, 56 `GLOB_DAT`, 22 **`ABS64`**), SLEB128-delta-encoded in a
   group-based format. A further **534** `JUMP_SLOT` relocations arrive **separately** via
   `DT_JMPREL`, for a grand total of 568,806. A loader without APS2 applies *zero* relocations. This
   is the highest-risk piece of the loader and gets the most testing.
2. **Relocation proceeds in windows**, because copy-on-write is charged at `protect` time and not at
   write time (D11). A window is 64 KiB, equal to the commit granule, so a window landing in `.bss`
   needs exactly one commit. Executability is chosen at map time and can never be raised; writability
   is *not*, because a copy-on-write view is charged its full size the instant it is mapped, so a
   writable segment is mapped read-only and raised in windows and once at the end.
3. **Segment mapping** via placeholder split plus `MapViewOfFile3(MEM_REPLACE_PLACEHOLDER)` at 4 KB
   granularity, honouring `p_align`, which for `libroblox.so` is **0x4000 (16 KiB)** on every
   `PT_LOAD` — not the 4 KiB an earlier draft assumed. Windows splits placeholders at 4 KiB, so 16 KiB
   is satisfiable, but the segment arithmetic must use the real `p_align`.
4. **Symbol resolution** against Omnidroid's own provided libraries (section 5). `libroblox.so` has
   **only `DT_GNU_HASH`** — there is no `DT_HASH` fallback — though other libraries in the APK carry
   both, and where both exist they were verified to agree exactly. Which library an import is expected
   to come from is recorded only in `DT_VERNEED` + `DT_VERSYM`, which names `libc.so`, `libm.so` and
   `libdl.so` for 407 of the 565; the other 158 are unversioned and the file says nothing, so the
   loader reports them as unattributed rather than guessing from their names.
5. **RELRO**: make the 5,205,568-byte `PT_GNU_RELRO` region read-only after relocation — and note
   that `DT_PLTGOT` is **inside** it and `DT_FLAGS` carries `DF_BIND_NOW`, so all 534 `JUMP_SLOT`
   relocations are sealed with it and lazy PLT binding is impossible. They must be applied before the
   seal, which fixes the order of the whole load.
6. **`init_array`**: run all 3,594 entries in order. All must succeed. The array must be read from
   **relocated memory**: every slot is zero in the file, because the pointers are produced by
   `R_AARCH64_RELATIVE` relocations, so reading the file image yields 3,594 null pointers.
7. **`dl_iterate_phdr` must be faithful.** The C++ runtime is statically linked, so the unwinder
   lives inside the guest and walks 11.5 MB of `.eh_frame` using this call. A stub breaks every C++
   exception, and Roblox will throw.
8. **Deliberately not implemented**, because the APK contains none of it: ELF TLS (no `PT_TLS` and
   no `STT_TLS` anywhere), ifuncs, BTI/PAC/MTE, `DT_TEXTREL`. Thread-local storage is
   `pthread_key_*` only. This is a real saving, recorded so nobody adds it speculatively.
9. Two `DT_NEEDED` libraries (`libOpenSLES.so`, `libOpenMAXAL.so`) import zero symbols but must
   still resolve as loadable objects, and 10 `AMEDIAFORMAT_KEY_*` imports are **data** symbols, not
   functions. Both fail in ways that name no symbol, so the loader reports unresolved objects and
   data-versus-function mismatches explicitly.

---

## 5. The Android compatibility layer

Omnidroid implements only what the engine imports: **669 distinct undefined symbols**, enumerated
in `research/apk-undefined-symbols.txt` (365 generic libc, 55 libm, 50 bionic-specific, 88 GLES,
32 libandroid, 33 libmediandk, 20 EGL, 8 libz, 6 libdl, 5 liblog, 4 C++ ABI, 3 jnigraphics). The
import list *is* the specification, so the stub surface is generated from the ELF rather than
hand-maintained, and a missing symbol becomes a build-time fact instead of a runtime surprise.

**Host-native, not guest-compiled.** Every one of these is a Rust function running as native host
code. `malloc` is the host allocator, so the guest heap *is* the host heap, which is what makes the
memory model work and what "use host resources directly" means in practice. It also means libc runs
at full native speed on x86-64 hosts instead of being translated.

**The thunk boundary.** Guest code is ARM64; host code is x86-64 on x86-64 hosts. The loader binds
each undefined symbol to a synthetic guest address inside a reserved *thunk region*. When the CPU
backend reaches a branch into that region, it marshals AAPCS64 into the host ABI, calls the Rust
implementation, and returns. Callbacks in the other direction, host to guest, such as a Vulkan
allocator callback or a pthread entry point, use the mirror mechanism. On ARM64 hosts the ABI
already matches and the thunk reduces to close to a direct call.

**JNI without a JVM, and no dex interpreter** (D7, now verified). Omnidroid implements `JavaVM` and
`JNIEnv` as host-native function tables. The measured surface is small and lopsided: only **59 of
233** `JNINativeInterface` slots are ever dereferenced, `JavaVM` needs just **2** (`GetEnv` and
`AttachCurrentThread`), the engine **only reads Java fields and never writes them**, and every
`CallXxxMethod` funnels through the `...MethodV` slot — so the `va_list` forms must be right and the
convenience forms need not exist at all. Of 409 referenced Java members across 104 classes, roughly
**120 are needed for a first frame**.

The real work is not interpretation but **orchestration**: `libroblox.so` does not bootstrap itself.
Flags, client settings, base URLs, directories, device parameters and `InitParams` all arrive from
Java, and `NativeEngine` waits for them, so Omnidroid supplies a native shell that issues that
ordered sequence. A failed class or method lookup must return `NULL` with a pending exception rather
than aborting, because 5 referenced members do not exist in this APK's dex at all.

**Startup is AGDK `GameActivity`, not `NativeActivity`.** The entry point is
`Java_com_google_androidgamesdk_GameActivity_initializeNativeCode`, with
`meta-data android.app.lib_name=roblox`. GameActivity's native half is statically linked into
`libroblox.so`, so Omnidroid supplies the Java-side half and drives the lifecycle and input
callbacks itself. This contract is less documented than `NativeActivity` and is being pinned down
from the binary.

---

## 6. ARM64 execution

`omni-cpu` exposes a `GuestCpu` trait (create a context, run from an address, handle a thunk exit,
invalidate translated code for a range) with two backends.

**On ARM64 hosts** (Linux ARM64, macOS ARM64): guest code executes **natively**. There is no
translation at all; the loader maps it executable and calls it. The thunk boundary reduces to an
ABI-compatible call. This is the reason the abstraction exists.

**On x86-64 hosts**: ARM64 to x86-64 binary translation. Host baseline is x86-64-v3 (D2). BMI2's
flag-preserving `SHLX`/`SHRX`/`SARX` map directly onto AArch64's pervasive shifted-register
operands, and `CMPXCHG16B` is what makes 128-bit guest atomics implementable without a lock.
AVX-512 is never required.

Translated code lives in a **dual-mapped arena**, one RW view for emission and one RX view for
execution of the same pages, measured at 162 ns per emit-and-execute cycle versus 2259 ns for
`VirtualProtect` flipping, and never holding a W+X page (D12).

**The backend is dynarmic, pinned as a fork** (D5), behind the `GuestCpu` trait. The spike
confirmed the central bet: `fastmem_pointer = 0` with `fastmem_address_space_bits = 64` emits
`mov reg, [r13 + vaddr]` with `r13 = 0` — identity mapping at **zero** runtime cost, verified at a
47-bit host VA with no slow-path callbacks. Losing it costs **30-49x** (n=31, measured through the
runtime's real callback path across two loop shapes and both degraded mechanisms; an earlier 13.2x
figure measured a bare stub and is a floor). Omnidroid therefore asserts this configuration at startup
rather than trusting the default, which is 36 bits and **silently degrades high addresses to the slow
path while still producing correct results** — a loss no functional test can detect.

That assertion defends the *configuration*, once. It structurally cannot see a memory path that
degrades at **runtime**, after it has passed, which Task 3 found two ways to do. So the runtime also
checks the behaviour: **per run slice, the callback-path counter's delta must be zero unless that
slice ended in a memory-fault exit** (D4 amendment 2). It costs one load per slice — 0.430 ns,
median of n = 31 runs of 10,000,000 reads — against a slice of a million guest instructions.

Measured throughput is uneven and shapes what comes next: about **2.0x native** on memory-heavy
code and **2.2x** on NEON/FP, but about **33x** on register-bound integer code, caused by per-block
register allocation spilling every guest register to `JitState` each iteration plus `lahf`/`sahf`
NZCV round-trips. That is a fixable backend-quality problem, which is why the plan is to replace the
x64 backend eventually while keeping the A64 frontend.

Three consequences the rest of the design must absorb:

- **Cold translation is slow** (0.15-0.31 Mguest-insn/s on synthetic loops, implying 7-25 s to warm
  a Roblox-sized working set), so translation is parallelized across cores and backed by a
  persistent on-disk code cache keyed by library content hash. Measured on **870 real
  `libroblox.so` leaf functions** it is **0.486 Mguest-insn/s** — better than the synthetic figure,
  because short functions give the IR optimizer less to work over than a tight loop does, so the
  synthetic number remains the right one for loop-shaped code (D5 amendment 2).
- **Code caches are per-thread and not shared**, committing 20-35 MiB per guest thread regardless of
  code volume. This is in direct tension with D10 and is tracked as a primary risk.
- **`ExclusiveMonitor` uses one global spinlock** and anti-scales 21x from 1 to 16 threads. Since
  Omnidroid implements bionic it controls `getauxval(AT_HWCAP)` and could decline to advertise LSE
  atomics — but that steers the engine onto `LDXR`/`STXR` and into this very lock, so the two are
  resolved together.

`TPIDR_EL0` is fully supported, which matters because Android TLS depends on the thread pointer.
Unimplemented instructions (LSE atomics, FP16 arithmetic, `CNTVCT_EL0`, `ID_AA64*`) surface cleanly
through an interpreter fallback at about 87 ns per trap, so they are correct but slow, and are
patches we carry on the fork.

---

## 7. Instance isolation

**One OS process per instance.** This is the strongest isolation available, it is the only thing
that actually contains the memory-safety consequence of identity mapping (section 1), and it was
measured cheap: about 52 MiB VRAM and 110 to 160 MB host RAM per instance at 4 concurrent
instances.

Each instance gets a private directory tree, and nothing is shared except the read-only library
cache:

```
<instance-root>/<instance-id>/
    files/    cache/    config/    tmp/    logs/    state/
```

The guest filesystem is **virtual**: bionic file calls are serviced by a VFS that maps Android
paths such as `/data/data/com.roblox.client/...` and `/sdcard/...` onto that tree. The guest cannot
name a host path outside it, so isolation does not depend on the guest behaving. `/proc/self/maps`
and similar are synthesized from the loader's own state.

Per-instance memory follows D10: reserve a generous guest address space (free, 0 bytes of commit
charge, verified to 97.7 TB), commit lazily in 64 KB to 1 MB blocks (per-page fault-driven paging
costs 2053 ns per fault versus 3 ns per page for bulk commit, so it is not used as a hot path), and
reclaim with `MEM_DECOMMIT`, the **only** primitive measured to return commit charge. A 4 GB guest
space costs 37.25 MB of commit, and the goal's "several GB at startup, about 500 MB later" profile
is directly achievable and was demonstrated end to end. `EmptyWorkingSet` is a secondary lever for
backgrounded instances but is never mistaken for reclamation.

---

## 8. Graphics

**Vulkan is the primary path and it is nearly a pass-through.** The engine `dlopen`s
`libvulkan.so` (volk-style: 593 `vk*` name strings, zero `vk*` imports), so Omnidroid supplies that
library and forwards to the host driver. Because memory is identity-mapped, guest-filled Vulkan
structs are read directly by the host driver with **no marshalling and no copies**; the structs are
fixed-width and LP64 on both sides. Guest function pointers embedded in Vulkan structs, such as
allocator and debug callbacks, go through host-to-guest trampolines.

Two findings shape the rest:

- **The engine ships 1,364 SPIR-V modules** (`shaders_vulkan_mobile.pack`, 14.7 MB, STORED), so
  shader translation is off the critical path entirely.
- **The engine rejects emulated Vulkan devices**: `Vulkan: Device %s is emulated, skipping`, plus
  vendor and driver blacklists. Forwarding to the real host GPU reports a genuine `deviceType` and
  real IDs, which satisfies this. A software device would be refused.

**EGL and GLESv2 are hard-linked** (`DT_NEEDED`, 91 EGL and GL imports) and must resolve at load
time or the library will not load at all. That is a linker requirement, not a rendering one, so
they are provided as resolvable symbols and a real GLES3 implementation is deferred until proven
necessary. The manifest declares `glEsVersion=0x30000` required and no Vulkan feature at all, so
GLES3 is the guaranteed fallback: if the Vulkan path is ever refused on a host we cannot control,
GLES3-on-Vulkan becomes necessary, and the renderer abstraction stays general enough to admit it.

**Texture compression is a real cost.** The host GPU supports **neither ETC2 nor ASTC** (measured,
all variants); it supports BC1/BC3/BC7. Android assets ship ETC2/ASTC, so runtime transcoding is
mandatory infrastructure, budgeted from the start with a disk-backed transcode cache and run on the
**dedicated compute queue** so it does not serialize behind rendering. Bulk uploads use staging
buffers over the **dedicated transfer queue**, because only about 214 MiB is both host-visible and
device-local.

**The window is a genuine native desktop window**, resizable by the user, via `winit`. Verified
working end to end with correct swapchain recreation. The guest's `ANativeWindow` reports the real
window geometry, and resizes propagate as GameActivity surface-changed callbacks. There is no fixed
Android resolution anywhere.

Backends sit behind a renderer trait so D3D12 and Metal can be added later without touching the
compatibility layer.

---

## 9. Testing strategy

The project runs the real APK continuously. Progress is measured by a ladder of **falsifiable boot
milestones**, each a checkpoint that either passes against the real binary or does not:

| # | Milestone | Verified by |
|---|---|---|
| M0 | APK parsed; `.so` extracted to the 4 KB-aligned cache | entry list and hashes match the forensic report |
| M1 | ELF loaded; **all 568,806** relocations applied (568,272 APS2 + 534 `DT_JMPREL`); imports enumerated | exact per-type relocation counts; all 565 `libroblox.so` imports accounted for |
| M2 | A trivial ARM64 function from `libroblox.so` executes and returns | known input and output |
| M3 | All **3,594** `init_array` entries complete | counter reaches 3,594 with no fault |
| M4 | `JNI_OnLoad` returns successfully | return value is a valid JNI version |
| M5 | `initializeNativeCode` runs; engine requests a surface | callback observed |
| M6 | Vulkan instance and device created through the forwarding layer | device is the real GPU, not rejected as emulated |
| M7 | First frame presented to the native window | visual confirmation |
| M8 | Interactive: input, resize, sustained frames | sustained run |

Alongside that:

- **Golden-data unit tests** taken from the real binary. The APS2 decoder is tested against
  `libroblox.so`'s own 568,272 relocations, which is a far stronger test than synthetic input; the
  reference decoder used during analysis consumed 2,100,778 of 2,100,778 bytes exactly.
- **A guest thread is not runnable until `TPIDR_EL0` points at a bionic-layout TLS block** with a
  stack guard at offset 0x28 (D13). 1,276 of the engine's 1,282 thread-pointer reads want exactly
  that slot, and they happen before `JNI_OnLoad` and before the first static initializer, so this is
  asserted in thread bring-up rather than discovered at M3.
- **Differential CPU tests** for the translation backend: instruction sequences run through the
  backend and compared against a reference model, focused on flags, shifted operands, NEON, and
  atomics.
- **Memory-model assertions** that measure commit charge rather than assuming it, since the
  difference between `MEM_DECOMMIT` and `MEM_RESET` is invisible to functional tests.
- **Multi-instance tests** asserting no cross-instance filesystem or state visibility.

Rules that follow from the goal: no placeholder success messages, no fabricated implementations,
and no large bodies of untested code. A subsystem is "working" only when a milestone above passes
against the real APK. `docs/STATUS.md` is the honest record and distinguishes verified from
planned.

---

## 10. Known risks

| Risk | Why it matters | Mitigation |
|---|---|---|
| CPU backend undecided | Determines feasibility and effort of the whole x86-64 path | Spike running; trait boundary keeps the choice cheap |
| Test APK is cheat-injected | Library set, permissions and dex graph are contaminated (D6) | Design from stock `libroblox.so`; stock APK requested |
| JNI surface size unknown | If large, or if dex execution is forced, D7 collapses | Being measured before code is written |
| No Vulkan validation layers | A real use-after-free already crashed the driver silently | Install recommended; until then, extra care on resource lifetime |
| APS2 relocations | A wrong decoder loads zero code and fails confusingly | Tested against the real 568,272-relocation binary |
| 3,594 initializers | Any one failing blocks startup, far from the symptom | Per-initializer tracing from the start |
| Linux and macOS untested | Claiming support without testing is explicitly forbidden | Structural portability only; no claims until tested |
| Frame pacing measured through Parsec | Remote display distorts present timings | Re-verify on a local display before tuning |
