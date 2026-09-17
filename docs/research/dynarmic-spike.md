# dynarmic de-risking spike — ARM64-guest JIT on x86-64 Windows

**Date:** 2026-09-18
**Host:** Windows 11 Pro 10.0.26200 x86-64, i7-13700F (AVX2/BMI2/FMA/F16C/AES/SHA, no AVX-512), 24 logical cores, 32 GB RAM
**Toolchain:** MSVC 19.44.35228 (VS BuildTools 14.44.35207), Windows SDK 10.0.26100, CMake 4.4.3, Ninja 1.13.2, Rust 1.89 msvc
**Scratchpad (all code and logs):** `C:\Users\berat\AppData\Local\Temp\claude\C--Users-berat-Desktop-Omni-Apps-omnidroid\2692e040-d7b4-4250-b114-62e3f26c66c9\scratchpad\dynarmic-spike\`

Everything numbered below was produced by building and running real code on this machine. Files referenced:

| file | purpose |
|---|---|
| `enc.py` | hand-rolled AArch64 encoder (no external assembler); self-checking |
| `spike.cpp` | C++ harness: Q2 correctness, Q3 identity mapping, Q4 perf, Q5 threading, Q6 coverage, Q7 invalidation, `dis` host-code dump |
| `shim/od_dynarmic.cpp` | `extern "C"` shim over dynarmic (Q8) |
| `rustdemo/` | Rust crate that drives the JIT through the shim (Q8), no external crates |
| `out_q2.txt … out_q7.txt`, `q4.txt`, `build2.log` | captured outputs |
| `vc.bat`, `buildspike.bat`, `buildshim.bat`, `buildrust.bat` | build wrappers that source `vcvars64.bat` |

---

## Q1. Does it build? — YES

### Repository: `merryhime/dynarmic` is GONE (404)

```
$ git clone --recursive --depth 1 https://github.com/merryhime/dynarmic.git
remote: Repository not found.
fatal: repository 'https://github.com/merryhime/dynarmic.git/' not found
```

Probed mirrors (`git ls-remote`): `merryhime/dynarmic` 404, `MerryMage/dynarmic` 404, `dynarmic/dynarmic` 404, `Lime3DS/dynarmic` 404, `torzu-emu/dynarmic` 404.
**Alive:** `lioncash/dynarmic`, `azahar-emu/dynarmic`, `PabloMK7/dynarmic`, `yuzu-mirror/dynarmic`, `suyu-emu/dynarmic`.

**Mirror used: `https://github.com/yuzu-mirror/dynarmic.git`**, HEAD `9d4582339990d4eae53f1dc7160686920fc2075c`, 2024-03-05, `project(dynarmic VERSION 6.7.0)`. Its top commit is literally *"Replace some more dead repo references"*, i.e. the mirror has already been de-rotted. License `LICENSE.txt` is the ISC/0BSD text (“Permission to use, copy, modify, and/or distribute … with or without fee is hereby granted”) — permissive, confirmed.

**No submodules.** This fork vendors externals as **git subtrees** under `externals/` (`biscuit catch fmt mcl oaknut robin-map xbyak zycore zydis`), so `--recursive` is unnecessary and there are no submodule URLs to rot. Source size: 89,943 lines under `src/`.

### Build failure 1 — undeclared dependency: **Boost**

`find_package(Boost 1.57 REQUIRED)` at `CMakeLists.txt:143`. Prior research's dependency list (fmt, mcl, xbyak, zydis, robin-map) is **incomplete**. dynarmic needs Boost headers:

```
boost/icl/interval_map.hpp   boost/icl/interval_set.hpp
boost/variant.hpp            boost/variant/get.hpp
```

Header-only, BSL-1.0 (permissive, fine). `boost::icl` backs `BlockRangeInformation` (the invalidation range map); `boost::variant` backs IR terminals. Fixed by downloading `boost_1_88_0.zip` (241 MB) from `archives.boost.io` and extracting only `boost/` (17,332 header entries, 10.2 s), then `-DBoost_INCLUDE_DIR=<...>/boost_1_88_0`.

> For Omnidroid: either vendor these Boost subtrees, or replace them (icl → a simple interval set; variant → `std::variant`) — roughly a 300-line patch that removes a 240 MB dependency.

### Build failure 2 — **CMake 4.x policy floor (confirmed, as anticipated)**

```
CMake Error at externals/robin-map/CMakeLists.txt:1 (cmake_minimum_required):
  Compatibility with CMake < 3.5 has been removed from CMake.
  Or, add -DCMAKE_POLICY_VERSION_MINIMUM=3.5 to try configuring anyway.
```

`externals/robin-map/CMakeLists.txt:1` is `cmake_minimum_required(VERSION 3.1)`. **`-DCMAKE_POLICY_VERSION_MINIMUM=3.5` is mandatory on CMake 4.4.3** — verified by configuring without it (exit 1). Other externals are ≥3.5 so this is the only offender. `CMP0167` (FindBoost removal) only produces a *warning*; `-DCMAKE_POLICY_DEFAULT_CMP0167=OLD` silences it but is not required.

### Build failure 3 — MAX_PATH / `CMAKE_OBJECT_PATH_MAX`

Building inside the scratchpad path produced, for ~6 translation units:

```
...\frontend\A64\translate\impl\floating_point_conditional_compare.cpp :
  fatal error C1083: Cannot open compiler generated file: '': Invalid argument
```

CMake warned up front (`object file directory has 192 characters … maximum 250`). Fixed with `subst X: <scratchpad>` and building from `X:\`. **Not a dynarmic defect** — it is MSVC + long paths — but Omnidroid's CI must keep build paths short or enable long paths.

### Working commands

```bat
subst X: C:\...\scratchpad\dynarmic-spike
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat"

cmake -S X:\yz -B X:\b2 -G Ninja ^
  -DCMAKE_BUILD_TYPE=Release ^
  -DDYNARMIC_TESTS=ON ^
  -DDYNARMIC_WARNINGS_AS_ERRORS=OFF ^
  -DCMAKE_POLICY_VERSION_MINIMUM=3.5 ^
  -DBoost_INCLUDE_DIR=X:/boostroot/boost_1_88_0
cmake --build X:\b2 --parallel 24
```

`-DDYNARMIC_WARNINGS_AS_ERRORS=OFF` is prudent (it defaults ON for a master project; MSVC /W4 on a 2024 codebase with a 2025 compiler is a coin flip). `-DDYNARMIC_FRONTENDS=A64` builds A64 only if you do not want the A32 frontend.

### Measured build results

| item | measurement |
|---|---|
| Configure (clean) | **8 s** |
| Full build, A32+A64+tests, `-j24`, Release | **49 s** (362 ninja targets, 0 errors) |
| A64-only + tests | **33 s** (290 targets) |
| `dynarmic.lib` (A32+A64, Release) | **102.6 MiB** (107,574,132 B) |
| `dynarmic.lib` (A64 only) | **69.9 MiB** |
| Tests build? | **Yes** — `dynarmic_tests.exe` (6.2 MiB), plus `dynarmic_test_generator.exe`, `dynarmic_test_reader.exe` |
| Test suite | **All tests passed (202,200 assertions in 123 test cases) in 6 s** |
| A64-only test run | All tests passed (201,698 assertions in 84 test cases), 5 s |

The test binary prints host CPU features it detected, correctly identifying AVX2/BMI2/FMA/F16C/SHA/VAES and **no AVX-512** on this part.

**Verdict Q1: builds cleanly and quickly; all upstream tests pass. Three fixable friction points (Boost, CMake policy floor, path length), all documented above.**

---

## Q2. Does it execute A64 correctly? — YES, 37/37 checks

Harness: `spike.cpp` sections `q2()`. All instruction words were produced by `enc.py` (a hand-written encoder whose self-checks cross-validate against encodings independently known from dynarmic's own test suite, e.g. `ADD X0,X1,X2 == 0x8b020020`, `RET == 0xd65f03c0`, `ADDV B1,V0.8B == 0x0e31b801`). Running `out_q2.txt`:

```
=== Q2  A64 execution correctness ===
  T1 integer ALU + shifted operands: done (svc=1)
  T2 load/store via callbacks: reads=7 writes=3
==== checks passed=37 failed=0 ====
```

| test | encodings (hand-assembled) | result |
|---|---|---|
| **T1 integer ALU + shifted operands** | `d2824681` MOVZ X1,#0x1234 · `d2800222` MOVZ X2,#0x11 · `8b021023` ADD X3,X1,X2,LSL #4 · `4b820464` SUB W4,W3,W2,ASR #1 · `aa042065` ORR X5,X3,X4,LSL #8 · `ca0100a6` EOR X6,X5,X1 · `9b020c27` MADD X7,X1,X2,X3 · `9ac22028` LSLV X8,X1,X2 · `f1040029` SUBS X9,X1,#0x100 | PASS incl. NZCV (`GetPstate()>>28 == 0x2`) |
| **T2 load/store** | `f9000001` STR X1,[X0] · `f9400002` LDR X2,[X0] · `39400403` LDRB · `79400404` LDRH · `b9400405` LDR W · `a9010801` STP X1,X2,[X0,#16] · `a9411c06` LDP · `f86a6808` LDR X8,[X0,X10] | PASS (all sizes, pair, register-offset) |
| **T3 branch/loop** | `d2800000`,`d2800c81`,`8b010000`,`f1000421` SUBS,`54ffffc1` B.NE −8 | PASS, X0 = 5050 (Σ1..100) |
| **T4 BL + RET** | `94000003` BL +12 · `d65f03c0` RET | PASS, X0=11 and X30 = return address |
| **T5 NEON/ASIMD** | `4e080c20` DUP V0.2D,X1 · `4e040c41` DUP V1.4S,W2 · `4ee08402` ADD 2D · `4ea18423` ADD 4S · `4ea19c24` MUL 4S · `6e231c45` EOR 16B · `4eb1b826` ADDV S6,V1.4S · `4e083c49` UMOV X9,V2.D[0] | PASS, all 9 lane values exact |
| **T6 floating point** | `9e670020` FMOV D0,X1 · `1e612802` FADD D · `1e610803` FMUL D · `1e611804` FDIV D · `1e61c005` FSQRT D · `9e620066` SCVTF · `9e78004a` FCVTZS · `1e624047` FCVT S←D · `9e66004b` FMOV X←D | PASS (9.0+4.0=13, ×=36, ÷=2.25, √9=3, int↔fp exact) |
| **T7 atomics LDXR/STXR** | `c85f7c01` LDXR X1,[X0] · `91000421` ADD · `c8027c01` STXR W2,X1,[X0] · `35ffffa2` CBNZ W2,−12 | PASS, 41→42, STXR status 0 |

Additionally the whole upstream A64 suite passes (201,698 assertions). Nothing failed.

---

## Q3. THE CRITICAL QUESTION — guest VA == host VA?

### **VERDICT: IDENTITY MAPPING IS POSSIBLE, AND IT IS FREE AT RUNTIME.** Two caveats, both configuration-level.

### 3.0 The real field names (`src/dynarmic/interface/A64/config.h`, lines 138–294)

Three memory paths exist, in order of preference:

```cpp
// slow path: virtual dispatch, always present
virtual std::uint8_t  MemoryRead8 (VAddr) = 0;  ... MemoryRead128
virtual void          MemoryWrite8(VAddr, std::uint8_t) = 0;  ... MemoryWrite128
virtual bool          MemoryWriteExclusive8/16/32/64/128(...)
virtual std::optional<std::uint32_t> MemoryReadCode(VAddr vaddr);

// middle path: software page table
void**  page_table = nullptr;
size_t  page_table_address_space_bits = 36;        // "Valid values 12..64 inclusive"
int     page_table_pointer_mask_bits = 0;
bool    silently_mirror_page_table = true;
bool    absolute_offset_page_table = false;
std::uint8_t detect_misaligned_access_via_page_table = 0;
bool    only_detect_misalignment_via_page_table_on_page_boundary = false;

// fast path: host-address arena
std::optional<uintptr_t> fastmem_pointer = std::nullopt;
bool    recompile_on_fastmem_failure = true;
size_t  fastmem_address_space_bits = 36;           // "Valid values 12..64 inclusive"
bool    silently_mirror_fastmem = true;
bool    fastmem_exclusive_access = false;
bool    recompile_on_exclusive_fastmem_failure = true;
```

### 3.1 Can `fastmem_pointer` be 0? — **YES, and 0 is a first-class value**

`fastmem_pointer` is `std::optional<uintptr_t>`, and every enable-check in the codebase is `if (conf.fastmem_pointer)` — i.e. `has_value()`, **not** a non-zero test (`a64_interface.cpp:45`, `a64_emit_x64.cpp:82`, `emit_x64_memory.cpp.inc:18`). So `std::optional<uintptr_t>{0}` enables fastmem with a zero base. There is no assert or special case against it.

The codegen (`backend/x64/emit_x64_memory.h:163-193`):

```cpp
template<> RegExp EmitFastmemVAddr<A64EmitContext>(...) {
    const size_t unused_top_bits = 64 - ctx.conf.fastmem_address_space_bits;
    if (unused_top_bits == 0) {
        return r13 + vaddr;          // <-- no mask, no bounds check
    } else if (ctx.conf.silently_mirror_fastmem) { ... mask ... }
      else { ... test/jnz to abort ... }
}
```

and the base is loaded once, at JIT construction, into a permanently reserved register (`a64_interface.cpp:45`):

```cpp
if (conf.fastmem_pointer) { code.mov(code.r13, *conf.fastmem_pointer); }
```

**Cost of the base register: one reserved host GPR (r13) and zero runtime instructions.** Proof — actual emitted host code for a guest `LDR X1,[X9]` under identity mapping (`spike.exe dis mem`):

```
mov rax, [r15+0x48]        ; load guest X9 from JitState
mov r14, [r13+rax*1]       ; <-- THE guest load. one instruction, base+index SIB.
```

`r13` is folded into the SIB byte, so a nonzero base would cost exactly the same (0 extra instructions). With `fastmem_pointer = 0`, `r13 = 0` and the effective address literally *is* the guest address. Register pressure: `hostloc.h` lists 14 allocatable GPRs (`any_gpr`, r15 is JitState, rsp reserved); enabling fastmem removes `R13` and a page table would remove `R14`, so **12 allocatable GPRs** with fastmem alone.

### 3.2 Is there an address-space cap? — **The default caps at 36 bits and WILL break you; set it to 64.**

Measured (`out_q3.txt`):

```
3a guest data ptr = host ptr 0000023E816E0000 ; code ptr = 0000023E81420000 ; PC after = 0x23e8142001c
3a slow-path callback hits: read=0 write=0 (0 == fastmem in use)
3b code @ 00007E0000000000 (bit 46), data @ 00007F0000000000 : OK, callbacks=0
3c with fastmem_address_space_bits=36 and VA=00007F1000000000: slowpath read=1 write=1, X2=0xaaaabbbbccccdddd
```

* **3a** — `fastmem_pointer = 0`, `fastmem_address_space_bits = 64`: the guest `STR X1,[X0]`/`LDR`/`LDP` operate directly on `VirtualAlloc` memory at ordinary host addresses, values round-trip exactly, and **zero** slow-path callbacks fire. The callbacks were instrumented to be counted, so 0 proves the fastmem path was taken.
* **3b** — same config with code at **0x7E00_0000_0000** and data at **0x7F00_0000_0000** (bit 46, the top of the Windows x64 user range): works, still 0 callbacks. **No 32-bit or 36-bit cap once you set the bits to 64.**
* **3c** — leaving `fastmem_address_space_bits` at its **default 36** while using a 47-bit VA silently routes every access to the slow path (the emitted code is `shr tmp, 36; jnz abort`). The value returned was the callback's sentinel `0xaaaabbbbccccdddd`, not the real memory. **This is a footgun: get the config wrong and you get a working-but-30×-slower emulator, not an error.**

There **is** one genuine hard cap, in the *code* path rather than the data path (`frontend/A64/a64_location_descriptor.h:27`):

```cpp
static constexpr size_t pc_bit_count = 56;
static constexpr u64 pc_mask = mcl::bit::ones<u64>(pc_bit_count);
u64 PC() const { return mcl::bit::sign_extend<pc_bit_count>(pc); }
```

**Guest PC is truncated to a sign-extended 56-bit value.** Windows x64 user space is 47 bits (≤ 0x7FFF_FFFE_FFFF) and Android VAs are 39/48-bit, so this is harmless here — but it forbids ever placing guest code above 2^55, and it means the FPCR bits packed into bits 56–63 of the location hash are load-bearing. Worth a comment in Omnidroid's allocator.

The **page_table** path, by contrast, is genuinely unusable for a 64-bit guest: it is a flat `void*[1 << (bits-12)]` array. 36 bits = 2^24 entries = **128 MiB of table per address space**; 48 bits would be 512 GiB. I measured the 36-bit table working (Q4 B2 PAGE_TABLE, 0 slow-path hits) but it is a dead end for Omnidroid. Use fastmem.

### 3.3 Unmapped guest memory on Windows — **frame-based SEH, and Omnidroid can pre-empt it with a VEH**

`backend/x64/exception_handler_windows.cpp` does **not** use a vectored exception handler. It builds a synthetic `UNWIND_INFO` + `RUNTIME_FUNCTION` covering the entire JIT code block and registers it with `RtlAddFunctionTable(rfuncs, 1, code.getCode())`, with `UNW_FLAG_EHANDLER` pointing at a hand-emitted language-specific handler. That handler first checks whether the faulting RIP is inside the code cache:

```cpp
code.mov(rax, Safe::Negate(bit_cast<u64>(code.getCode())));
code.add(rax, qword[ABI_PARAM3 + offsetof(CONTEXT, Rip)]);
code.cmp(rax, (u32)code.GetTotalCodeSize());
code.ja(exception_handler_without_cb);          // not ours -> ExceptionContinueSearch
```

and otherwise calls `FastmemCallback(rip)`, which looks the faulting instruction up in `fastmem_patch_info`, marks it `do_not_fastmem`, requests recompilation (`recompile_on_fastmem_failure`) and resumes by injecting a fake call. Consequences:

* dynarmic's handler is **scoped to its own code region** and returns `ExceptionContinueSearch` for anything else. It does not hijack the process.
* Because it is a *frame-based* handler, **a vectored exception handler installed by Omnidroid runs first.** Verified (3d, 3e):

```
3d access to reserved-but-uncommitted page survived: dynarmic SEH handler routed it to MemoryRead64 (hits=1)
3e demand paging by an Omnidroid-owned VEH worked: veh_hits=1 dynarmic_slowpath=0
```

* **3d** with no VEH: guest `LDR X2,[X0]` at a `MEM_RESERVE`-only page faulted, dynarmic's handler caught it, called `MemoryRead64`, and execution continued. The block was then recompiled without fastmem for that instruction.
* **3e** with an Omnidroid `AddVectoredExceptionHandler(1, …)` that commits the page and returns `EXCEPTION_CONTINUE_EXECUTION`: the VEH serviced the fault (`veh_hits=1`), the value was read back correctly, and **dynarmic's slow path was never entered** (`dynarmic_slowpath=0`). This is exactly the demand-driven memory model Omnidroid wants: lazily commit guest pages on first touch, with dynarmic's handler as a second-line fallback for genuinely invalid accesses.

One caveat: `recompile_on_fastmem_failure` is *sticky per instruction*. If a page is legitimately absent once and dynarmic's handler sees it, that guest instruction is demoted to the slow path permanently (until cache invalidation). With an Omnidroid VEH in front this never happens for demand-paged memory, so keep the VEH.

### 3.4 Q3 verdict

**Identity mapping is possible with caveats — all of them configuration, none of them architectural.**

Required config:
```cpp
conf.fastmem_pointer            = std::optional<uintptr_t>{0};  // identity
conf.fastmem_address_space_bits = 64;                           // NOT the default 36
conf.silently_mirror_fastmem    = false;                        // irrelevant when bits==64
conf.page_table                 = nullptr;                      // do not use it
```
Caveats: (1) `fastmem_address_space_bits` must be 64 or high addresses silently fall to callbacks; (2) guest code must live below 2^55 (the 56-bit PC mask) — fine everywhere; (3) `r13` is permanently reserved; (4) guest and host share one address space, so Omnidroid's own allocations (including dynarmic's 128 MiB-per-Jit code cache) must not squat on addresses the guest ELF wants — `MAP_FIXED`-style loads need a reservation pass at startup; (5) install a VEH for demand paging so dynarmic's handler never demotes instructions.

**Runtime cost of identity mapping: zero instructions.** One `mov` per guest load/store, base folded into the SIB byte.

---

## Q4. Performance

All measured with `spike.exe q4`; every loop's completion is verified (`X20=0`, plus an accumulator check) so no number comes from a loop that exited early.

### Steady-state translated throughput (4,000,000 iterations each)

| benchmark | guest insns | time | **Mguest-insn/s** | native x86-64 reference | **slowdown** |
|---|---|---|---|---|---|
| **B1** integer ALU loop (8 ALU ops + SUBS + B.NE) | 40,000,000 | 0.066 s | **603** | 0.002 s / 24,774 Mops/s | **≈ 33×** |
| **B2** memory loop, **FASTMEM identity** (2 LDR + 2 STR + ADD + AND) | 32,000,000 | 0.006 s | **5,207** | 0.003 s / 10,271 Mops/s | **≈ 2.0×** |
| **B2** memory loop, **page_table (36-bit)** | 32,000,000 | 0.009 s | **3,480** | — | ≈ 3.0× |
| **B2** memory loop, **callbacks only** (16 M virtual calls) | 32,000,000 | 0.081 s | **396** | — | **≈ 26×** |
| **B3** NEON + scalar FP loop (ADD.4S, MUL.4S, FADD.2D, FMUL.2D, FADD D, FMUL D) | 32,000,000 | 0.020 s | **1,626** | 0.009 s / 3,579 Mops/s | **≈ 2.2×** |

**fastmem vs callbacks on identical guest code: 5,207 vs 396 Mguest-insn/s — a 13.2× difference.** This is the single most important measurement in the spike and it validates the Omnidroid design intent completely. The page-table middle path is only 1.5× worse than fastmem but costs 128 MiB of table and is capped at 36 bits; skip it.

### Why B1 (register-bound integer code) is the worst case: 33×

Dumped with `spike.exe dis int`. The steady-state loop body dynarmic emits for 10 guest instructions is ~40 host instructions, and **every guest instruction round-trips its operands through the JitState in memory**:

```
mov rax, [r15]            ; guest X0  <- memory
mov r14, [r15+0x50]       ; guest X10 <- memory
lea r12, [rax+r14*1]      ; the actual ADD
mov [r15], r12            ; guest X0  -> memory
mov rax, [r15+0x08]       ; guest X1  <- memory
mov r12, [r15+0x58]       ; guest X11 <- memory
xor rax, r12
mov [r15+0x08], rax
...
mov rax, [r15+0xA0] / mov [rsp+0x30], rax / mov r14, [rsp+0x30]   ; counter via stack
sub r14, r12
cmc / lahf / seto al / mov [r15+0x108], eax                        ; NZCV -> memory
mov eax, [r15+0x108] / sahf                                        ; NZCV <- memory
jnz <loop>
cmp dword ptr [r15+0x318], 0x00                                    ; halt check every iteration
mov rax, <guest pc> / mov [r15+0x100], rax                         ; guest PC -> memory
```

Two structural costs are visible: (a) the register allocator is **per-basic-block** and does not keep guest registers in host registers across the loop back-edge, so a loop-carried value costs a store + a store-forwarded load every iteration; (b) **NZCV is materialised through memory with `lahf`/`sahf`** for every flag-setting op feeding a conditional branch — a serialising 5-cycle store-to-load round trip per iteration. Memory-bound code hides this (the loads/stores dominate anyway), which is why B2 is 2× and B1 is 33×.

Realistic expectation for mixed Android/Roblox native code: **somewhere between 3× and 15× native**, weighted toward the memory/NEON end for a game engine. That is in the same class as Rosetta 2's JIT tier and far better than any interpreter.

### Multi-thread aggregate throughput scales (Q5d)

| threads | aggregate Mguest-insn/s |
|---|---|
| 1 | 729 |
| 2 | 1,541 |
| 4 | 2,574 |
| 8 | 4,425 |
| 16 | 5,215 |
| 24 | 4,872 |

Independent `Jit` instances scale ~6× to 8 threads and saturate around 16 (P-core count). No shared lock on the execution path.

### JIT **compilation** throughput — this is the weak spot

Realistic shape: 20,000 distinct basic blocks of *blk* integer instructions, each ending in a branch.

| block size | guest insns | cold translate+run | **Mguest-insn/s** | **blocks/s** | warm re-run | host x64 insns / guest | **host bytes / guest insn** | bytes / block |
|---|---|---|---|---|---|---|---|---|
| 4 | 100,000 | 0.325 s | **0.308** | 61,559 | 0.0005 s | 6.30 | **30.4 B** | 152 B |
| 8 | 180,000 | 0.677 s | **0.266** | 29,562 | 0.0004 s | 4.61 | **21.7 B** | 195 B |
| 16 | 340,000 | 1.440 s | **0.236** | 13,886 | 0.0009 s | 3.54 | **16.3 B** | 277 B |
| 32 | 660,000 | 4.517 s | **0.146** | 4,427 | 0.0013 s | 2.64 | **12.0 B** | 394 B |

Pathological single giant basic block: 0.31–0.36 Mguest-insn/s regardless of size (2.8–3.2 µs per guest instruction), so the cost is per-instruction in the IR pipeline, not quadratic.

**Cold-code translation runs at roughly 0.15–0.31 million guest instructions per second — about 3–7 µs of CPU per guest instruction translated.** That is very slow for a JIT. Concretely for Roblox: a `libroblox.so` with, say, 8 MB of *actually executed* `.text` ≈ 2 M guest instructions would cost **~7–13 seconds of single-threaded translation** and produce ~32 MB of host code. If 20 MB of code is touched over a session, that is ~25 s of translation and ~80 MB of host code, and 80 MB is already past the point where the default 128 MiB cache starts flushing (see Q7).

Mitigations available without touching dynarmic: translate on background threads is **not** possible (one `Jit` owns its cache), but you can (a) pre-warm hot libraries on a worker `Jit` and accept the duplicate cache, (b) raise `code_cache_size`, (c) reduce optimisation via `UserConfig::optimizations` — the passes (`ConstantPropagation`, `A64GetSetElimination`, `DeadCodeElimination`, `VerificationPass`) are where the time goes. `VerificationPass` in particular runs unconditionally in Release.

---

## Q5. Threading and per-thread state

### Model: **one `Jit` per guest thread, with a completely private code cache**

Verified with `spike.exe q5`:

```
8 x A64::Jit (code_cache_size=32MiB, nothing compiled): +276.68 MiB  => 34.59 MiB per Jit
after each Jit compiled the same 4-insn block: +1.16 MiB total => code caches are per-Jit, not shared
after destroying all 8: 1.88 MiB vs baseline
```

`A64::Jit::Impl` owns its `BlockOfCode` and `A64EmitX64`; there is no cross-`Jit` sharing of translations. Eight Jits each compiling the *same* four-instruction block each paid for their own copy. **Translations are duplicated per guest thread.** For Roblox (dozens of threads running the same engine code) this is a real multiplier on both memory and cold-start CPU.

### Per-`Jit` memory footprint (measured private bytes at construction)

| `code_cache_size` | private bytes at construction |
|---|---|
| 4 MiB | **+20.5 MiB** |
| 8 MiB | **+24.5 MiB** |
| 32 MiB | **+34.5 MiB** |
| 128 MiB (the default) | **+34.5 MiB** |

On Windows the code arena is `VirtualAlloc(MEM_RESERVE)` only (`block_of_code.cpp:64`) and is committed lazily in ~1 MiB steps by `EnsureMemoryCommitted`, with a 16 MiB `PRELUDE_COMMIT_SIZE` up front — hence the flat ~34.5 MiB once the cache is ≥32 MiB. The floor is **~20 MiB per guest thread** (16 MiB prelude commit + 2 MiB `CONSTANT_POOL_SIZE` + JitState + fast-dispatch table), independent of how little code you translate.

**Budget: 32 guest threads × 20–35 MiB = 0.6–1.1 GiB of host RAM just for JIT scaffolding**, before any guest memory. This is the biggest memory line item in the design.

### `ExclusiveMonitor` across threads — **CORRECT BUT ANTI-SCALING**

`ExclusiveMonitor(processor_count)` is constructed with a **fixed** processor count and each `Jit` takes a unique `processor_id < processor_count` (`interface/exclusive_monitor.h`). Guest threads created at runtime therefore need a pre-allocated id pool or an over-sized monitor. `RESERVATION_GRANULE_MASK = 0xFFFF'FFFF'FFFF'FFFF`, i.e. the reservation granule is one byte — stricter than real hardware, which is the safe direction.

Correctness: 8 threads × 200,000 `LDXR`/`ADD`/`STXR` increments on **one shared word** produced exactly 1,600,000. No lost updates, no errors.

Performance (Q5c, one shared word, worst case):

| threads | increments | time | **M exclusive-ops/s** | correct |
|---|---|---|---|---|
| 1 | 100,000 | 0.030 s | **3.4** | yes |
| 2 | 200,000 | 0.104 s | **1.9** | yes |
| 4 | 400,000 | 0.366 s | **1.1** | yes |
| 8 | 800,000 | 2.272 s | **0.35** | yes |
| 16 | 1,600,000 | 9.963 s | **0.16** | yes |

**Throughput falls 21× as threads go 1→16.** The cause is `ExclusiveMonitor::Lock()` — a single global `SpinLock` taken for *every* `LDXR` and every `STXR`, in `ReadAndMark` and `CheckAndClear`. Native `lock xadd` under 16-way contention would sustine tens of millions of ops/s. A contended guest spinlock or refcount in Roblox would convert into a pathological host hot spot.

Partial mitigation exists: `UserConfig::fastmem_exclusive_access = true` makes dynarmic use x64 `cmpxchg` directly on the fastmem arena instead of the callback + monitor (the config comment admits "may not provide fully accurate emulation"). That should remove the global lock for the common case; it is **UNVERIFIED** in this spike (not exercised) and should be the first follow-up experiment, since it plausibly fixes the worst threading result here.

### LSE atomics (`CAS`, `LDADD`, `SWP`) — **NOT SUPPORTED** (see Q6). This is the other major threading finding.

---

## Q6. AArch64 feature coverage

Decoder table census (`frontend/A64/decoder/a64.inc`): **643 enabled `INST(...)` entries, 231 commented out.** IR has 612 generic + 57 A64-specific opcodes.

### How unsupported instructions surface — **cleanly, via callbacks, never a crash**

Measured for every probe (`out_q6.txt`). Two distinct mechanisms:

* **`UserCallbacks::InterpreterFallback(pc, num_insns)`** — for encodings the decoder recognises but the translator declines (`TranslatorVisitor::InterpretThisInstruction()`). Block translation stops, control returns to the host, Omnidroid emulates the instruction and resumes. **This is the clean interception path Omnidroid needs.**
* **`UserCallbacks::ExceptionRaised(pc, Exception)`** — `UnallocatedEncoding`, `ReservedValue`, `Breakpoint`, `Yield`, `WaitForEvent`, …

No asserts fired, no crashes, in any probe. `DYNARMIC_IGNORE_ASSERTS` / `DYNARMIC_FATAL_ERRORS` exist as build options if you want to harden further.

### Probe results

| feature | probe | outcome |
|---|---|---|
| **TPIDR_EL0 / TPIDRRO_EL0** (Android TLS) | `d51bd041` MSR TPIDR_EL0,X1 · `d53bd042` MRS · `d53bd063` MRS TPIDRRO | **SUPPORTED, read and write.** Backed by caller-supplied `u64*` (`config.tpidr_el0`, `config.tpidrro_el0`), pointer baked into emitted code. Round-trip of `0x7FFF123456789000` verified. |
| MRS `CNTFRQ_EL0` / `CTR_EL0` / `DCZID_EL0` / `CNTPCT_EL0` / `FPCR` / `FPSR` / `NZCV` | — | **SUPPORTED** (values come from `UserConfig` / `GetCNTPCT()`); verified exact |
| MRS **`CNTVCT_EL0`** | `d53be046` | **NOT supported → InterpreterFallback.** Note only `CNTPCT_EL0` is implemented. Android's `clock_gettime` vDSO reads `CNTVCT_EL0`, so **every guest clock read traps.** |
| MRS `MIDR_EL1` | `d5380009` | InterpreterFallback |
| MRS `ID_AA64ISAR0_EL1` | `d538060a` | InterpreterFallback |
| MRS `ID_AA64PFR0_EL1` | `d538040b` | InterpreterFallback |
| **LSE atomics: `CAS`** | `88a27c03` | **NOT supported → InterpreterFallback.** `//INST(CAS,...)` commented out in `a64.inc:188` |
| **LSE `LDADD`** | `b8220003` | **NOT supported → InterpreterFallback** (`a64.inc:262`) |
| **LSE `SWP`** | `b8228003` | **NOT supported → InterpreterFallback** (`a64.inc:270`) |
| **`LDAPR`** (RCpc) | `f8bfc002` | **NOT supported → InterpreterFallback** |
| `LDXR`/`STXR`/`LDAXR`/`STLXR`/`CLREX`/`STLR`/`STLLR` | — | **SUPPORTED** (Q2 T7, Q5) |
| **PAC in HINT space** — `PACIASP`, `AUTIASP`, `XPACLRI` | `d503233f`, `d50323bf`, `d50320ff` | **Executed as `HINT` = no-op.** Correct behaviour for a non-PAC CPU. `-mbranch-protection=standard` binaries run unmodified. |
| **`BTI c`** | `d503249f` | Executed as `HINT` = no-op. Fine. |
| PAC **register** forms — `PACIA X0,X1`, `RETAA` | `dac10020`, `d65f0bff` | **NOT supported → InterpreterFallback.** Rare in Android builds but would need emulation. |
| `SVC` | `d4000841` | **SUPPORTED** → `CallSVC(swi)`. Clean syscall hook. |
| `BRK` | `d4200020` | → `ExceptionRaised(Exception::Breakpoint)` |
| `UDF` (word 0) | `00000000` | → InterpreterFallback |
| `DMB SY`, `ISB` | `d5033fbf`, `d5033fdf` | **SUPPORTED**; `hook_isb` available |
| `YIELD`, `WFE` | `d503203f`, `d503205f` | → `ExceptionRaised(Yield / WaitForEvent)` — **always, see bug below** |
| Crypto: `AESE`, `SHA1C`, `SHA256H`, `PMULL`, `CRC32X` | `4e284820`, `5e020020`, `5e024020`, `0ee2e020`, `9ac24c20` | **ALL SUPPORTED** (native AES-NI/SHA/PCLMUL/CRC32 on this host; SHA polyfill exists if absent) |
| NEON breadth: `LD1`, `ST4`, `TBL`, `RBIT`, `LDNP`, `FCVTL`, `SDOT` | — | **ALL SUPPORTED** (incl. `SDOT`, ARMv8.2 dot product) |
| **FP16 arithmetic** — `FADD H0,H1,H2` | `1ee22820` | **NOT supported → `ExceptionRaised(UnallocatedEncoding)`** |
| **FP16 vector** — `FADD V0.8H,...` | `4e421420` | **NOT supported → InterpreterFallback** |
| FP16 *conversion* — `FCVTL V0.4S,V1.4H` | `0e217820` | SUPPORTED |
| **i8mm** — `SMMLA` | `4e82a420` | NOT supported → InterpreterFallback |
| **BF16** — `BFDOT` | `2e42fc20` | NOT supported → InterpreterFallback |
| **`FJCVTZS`** (ARMv8.3 JS convert) | `1e7e0020` | NOT supported → InterpreterFallback |
| `DC ZVA`, `IC IVAU` | `d50b7420`, `d50b7520` | **SUPPORTED**, with `hook_data_cache_operations` / `InstructionCacheOperationRaised` hooks |
| **Unaligned access** | `LDR X2,[X0]` with X0 = base+1, fastmem | **Just works.** `X2=0x0001020304050607`, 0 slow-path hits, 0 exceptions — x86 tolerates unaligned loads, and `detect_misaligned_access_via_page_table` defaults to 0. Matches AArch64 behaviour for normal memory. |

Full disabled-instruction list (231 entries) is in `enc.py`-adjacent output; the families that matter are: **all LSE atomics**, **all FP16 arithmetic**, **BF16**, **i8mm**, **MTE** (`STG/LDG/IRG/SUBP`), **PAC register forms**, `SQRDMLAH/SQRDMLSH` (ARMv8.1 RDMA), `FRINT32/64` (v8.5), `SYS`/`SYSL`/`MSR (immediate)`, `ERET`/`HVC`/`SMC`/`HLT`, `LD64B`/`ST64B`.

### Cost of the escape hatch — **87 ns per trap**

`spike.exe q6b` measures the full exit-JIT → host-callback → re-enter-`Run()` round trip:

```
MRS Xn,CNTVCT_EL0            86.8 ns per trap+re-enter  ( 11.52 M traps/s)
LDADD W2,W3,[X0] (LSE)       86.4 ns per trap+re-enter  ( 11.57 M traps/s)
CAS W2,W3,[X0] (LSE)         86.8 ns per trap+re-enter  ( 11.52 M traps/s)
LDAPR X2,[X0]                88.0 ns per trap+re-enter  ( 11.37 M traps/s)
baseline SVC round trip      69.1 ns  (floor: Run() enter+exit)
```

**~87 ns ≈ 450 host cycles per unsupported instruction.** A native `lock xadd` is ~20 cycles. So an LSE atomic emulated through `InterpreterFallback` is **~20–25× slower than the native equivalent**, and it also truncates the containing basic block (destroying block-linking). If Roblox's arm64 build emits LSE atomics inline — which it will if compiled with `-march=armv8.2-a`, or with `+lse`, or with outline-atomics resolving to the LSE path because we advertise `HWCAP_ATOMICS` — throughput collapses. **Mitigations: (a) never advertise `HWCAP_ATOMICS` so outline-atomics takes the LDXR/STXR path; (b) implement the ~40 LSE `INST(...)` entries in the frontend, which is a bounded, mechanical patch (they lower to existing IR exclusive ops or straight to x64 `lock`-prefixed ops).**

### Bug found: `UserConfig::hook_hint_instructions` is **ignored** by the A64 frontend

`YIELD` raised `ExceptionRaised(Yield)` with `hook_hint_instructions = false`, which contradicts `frontend/A64/translate/impl/system.cpp:42`. Root cause: `backend/x64/a64_interface.cpp:271` calls

```cpp
A64::Translate(..., {conf.define_unpredictable_behaviour, conf.wall_clock_cntpct});
```

and never passes the third `TranslationOptions` field, which defaults to `true` (`frontend/A64/translate/a64_translate.h:37`). The **A32** backend does pass it (`a32_interface.cpp:216`). So on A64, **`YIELD`/`WFE`/`WFI`/`SEV`/`SEVL` always exit the JIT**, and each exit costs the ~70–87 ns measured above. Android spin/backoff loops use `YIELD` liberally. **One-line upstream fix**; Omnidroid should carry it.

---

## Q7. Self-modifying code and invalidation

Measured (`out_q7.txt`):

```
after overwriting guest code with no invalidation: X0=42 (42 => stale translation is used)
InvalidateCacheRange(0000025CDD420000,8) -> new translation picked up
20000 distinct blocks compiled in 0.158 s (517999 host x64 insns).
InvalidateCacheRange over 64 bytes with 20000 live blocks: 0.082 ms
ClearCache (full flush): 0.000 ms
```

* **There is no automatic SMC detection.** Rewriting guest code in place kept executing the stale translation (`X0=42` after the code said 171). `InvalidateCacheRange(addr, len)` then picked up the new code; `ClearCache()` also works.
* **What Omnidroid must call:**
  * `mmap`/`mmap(MAP_FIXED)` over executable memory, `munmap`, `mprotect` to/from `PROT_EXEC`, `dlclose`/library unload, any writable/executable page the guest writes: **`InvalidateCacheRange(start, length)`** for the affected range.
  * Guest `IC IVAU` / `IC IALLU` / `ISB` after a JIT-in-guest writes code: hook via `hook_data_cache_operations` / `InstructionCacheOperationRaised` / `hook_isb` and forward to `InvalidateCacheRange`. Note that a guest-side JIT (Roblox's Luau does generate native code on some platforms) makes this path mandatory, not optional.
  * The call is thread-safe-ish: it sets `HaltReason::CacheInvalidation` and `invalid_cache_ranges`, and the work happens at the next `Run()` boundary under `invalidation_mutex`. **But the invalidation is per-`Jit`** — Omnidroid must fan a single guest `mprotect` out to *every* thread's `Jit`.
* **Cost:** a range invalidation with 20,000 live blocks is **82 µs** — it walks a `boost::icl::interval_set` (`BlockRangeInformation::InvalidateRanges`), rebuilds affected links, clears the fast-dispatch table and *drops all `fastmem_patch_info`*. It does **not** reclaim code-cache space; the invalidated bytes are simply orphaned.
* **Eviction policy: there is none.** `BlockOfCode::ClearCache()` is `SetCodePtr(code_begin)` — a full reset, nothing finer. The only automatic mechanism is in `a64_interface.cpp:263`:

```cpp
constexpr size_t MINIMUM_REMAINING_CODESIZE = 1 * 1024 * 1024;
if (block_of_code.SpaceRemaining() < MINIMUM_REMAINING_CODESIZE) {
    invalidate_entire_cache = true;             // "Immediately evacuate cache"
    PerformRequestedCacheInvalidation(HaltReason::CacheInvalidation);
}
```

**When the cache fills, everything is thrown away and re-translated from scratch.** Combined with the measured **12–30 bytes of host code per guest instruction** and the **0.15–0.31 Mguest-insn/s translation rate**, a long Roblox session that touches more than `code_cache_size / ~16 B` ≈ 8 M guest instructions (per thread) will periodically stall for seconds re-JITing. Max `code_cache_size` is ~2 GiB on x64 (limited by 32-bit jump range and the `cmp reg, imm32` in the SEH handler), so you can buy headroom with RAM — at 20–35 MiB × threads of committed floor plus the growth.

---

## Q8. Rust FFI viability — **DEMONSTRATED WORKING**

Built `shim/od_dynarmic.cpp` (a 240-line `extern "C"` wrapper) into `od_dynarmic.lib` with MSVC, then a Rust binary (`rustdemo/`) that links it with **zero external crates** — `build.rs` only emits `cargo:rustc-link-search` / `cargo:rustc-link-lib` lines plus `msvcprt`. (The `cc`/`cmake` crates would work too; avoiding them proved the link model without needing the network.)

### Shim surface required: **18 `extern "C"` functions + 17 callback function pointers**

```
od_monitor_new/free
od_jit_new(const od_config*) / od_jit_free / od_jit_run / od_jit_step
od_jit_halt / od_jit_clear_halt
od_jit_get_pc/set_pc, get_sp/set_sp, get_reg/set_reg, get_vec/set_vec
od_jit_get_pstate/set_pstate, get_fpcr/set_fpcr
od_jit_invalidate_range / od_jit_clear_cache / od_jit_clear_exclusive
```
Callbacks (`struct od_callbacks`, all plain `extern "C" fn(*mut c_void, ...)`): `read_code`, `read8/16/32/64/128`, `write8/16/32/64/128`, `cas32`, `cas64`, `interpreter_fallback`, `call_svc`, `exception_raised`, `get_cntpct`.

That is the whole surface. The C++ `ShimCallbacks` class implements `A64::UserCallbacks` by forwarding each virtual to the corresponding function pointer — one indirection, and only on the slow path.

### Measured from Rust

```
Omnidroid Q8: dynarmic A64 JIT driven from Rust via extern "C" shim
  loop sum 1..100 -> X0 = 5050 (want 5050)  svcs=1 pc=0x256e0a30018
  guest STR wrote into a Rust Vec<u64>: buf[0]=0x0badc0def00dface, LDR read back X2=0x0badc0def00dface
  slow-path callbacks taken: reads=0 writes=0 (0 => fastmem, no FFI on the hot path)
  od_jit_invalidate_range from Rust -> X0=171 (want 171)
  fastmem (identity)    :   0.0022 s      12000000 guest insn    5566.1 Minsn/s  (FFI callbacks taken: 0)
  callbacks into Rust   :   0.0145 s      12000000 guest insn     825.9 Minsn/s  (FFI callbacks taken: 8000000)
OK
```

Highlights:
* Guest AArch64 code **wrote directly into a Rust `Vec<u64>`** under identity mapping — no translation layer, no copy.
* `od_jit_invalidate_range` driven from Rust correctly picked up rewritten guest code.
* **fastmem: 5,566 Mguest-insn/s with *zero* FFI calls. Callbacks: 826 Mguest-insn/s with 8,000,000 FFI calls — a 6.7× penalty.** So the answer to "would per-callback dispatch sit in a hot path?" is: **only if fastmem fails**, and fastmem does not fail (Q3). The FFI boundary is on the cold path (syscalls, unsupported instructions, exceptions) where ~87 ns per event already dominates the ~2 ns of FFI.
* C++ ↔ Rust link on MSVC needed only `cargo:rustc-link-lib=dylib=msvcprt` alongside the static libs; both sides on `/MD`. No exceptions cross the boundary (dynarmic throws only `Xbyak::Error` at construction — the shim should `catch` there; currently it does not, which is a 5-line fix).

---

## Q9. Verdict

### **(b) Adopt dynarmic now, with a forked/patched copy, and plan to replace the x64 backend later if throughput becomes the binding constraint.**

Not (a) unqualified, because of the LSE gap, the register-allocation ceiling and the per-thread memory cost. Not (c), because the measurements show dynarmic already clears the bar that matters most (identity mapping, 2× on memory/NEON code) and a custom JIT is a multi-engineer-year project.

### What the measurements justify

| question | answer |
|---|---|
| Builds on this machine? | Yes, 49 s clean, all 202,200 test assertions pass |
| Correct A64 semantics? | Yes, 37/37 hand-written checks + the full upstream suite |
| **Identity mapping (the whole design bet)?** | **Yes, zero runtime cost** — `fastmem_pointer = 0`, `fastmem_address_space_bits = 64`, one reserved GPR, one `mov [r13+reg]` per access, works at 47-bit VAs |
| Demand-driven memory? | Yes — an Omnidroid VEH pre-empts dynarmic's SEH handler and can commit pages lazily |
| Steady-state speed | 2.0× native for memory code, 2.2× for NEON/FP, 33× for register-bound integer code; 13× better with fastmem than with callbacks |
| Rust integration | 18 + 17 symbol surface, demonstrated, no hot-path FFI |
| Multi-thread scaling | Good for independent work (6× on 8 threads); **bad for contended atomics (21× *slowdown* 1→16 threads)** |
| Cold start | 0.15–0.31 Mguest-insn/s — the weakest number in the spike |

### What a custom JIT would cost (honest estimate)

Scope, from dynarmic's own line counts and instruction tables:

* **A64 frontend** (decode + lower to IR): 13,395 LOC for **643 implemented encodings** across ~58 translation-unit families. A minimal-but-real Android subset — integer ALU/shift/bitfield/multiply/div, load/store in all addressing modes, branches, exclusives, the full ASIMD three-same/two-reg-misc/across-lanes/permute/table/indexed-element set, scalar and vector FP, conversions, system registers — is realistically **500–600 encodings**, i.e. essentially the same scope. NEON alone is ~300 of them.
* **x86-64 backend**: 23,023 LOC, of which `emit_x64_vector*.cpp` is the bulk (NEON→SSE/AVX2 lowering, including all the cases where no single x86 instruction exists: saturating arithmetic, pairwise ops, `TBL`, unsigned compares, 64-bit multiplies, FP rounding-mode fidelity).
* **IR + optimiser + register allocator**: 8,120 LOC.
* Plus a differential fuzzer against real hardware, which is the only way to get FP/NEON edge cases right — dynarmic has one (`dynarmic_test_generator`, Unicorn fuzzing hooks) and it took years of emulator-community bug reports to converge.

Realistically **~45,000 lines of hard, correctness-critical code and 2–4 engineer-years** to reach parity, with the NEON/FP edge cases being the long tail. A custom JIT's *upside* — a proper linear-scan or global register allocator that keeps guest registers in host registers across loops, and native flag handling instead of `lahf`/`sahf` — would plausibly turn B1's 33× into 3–5×. That is a large win, but it is a second-year win.

The pragmatic middle path, and the recommendation: **adopt dynarmic, and treat its x64 backend as replaceable.** The `A64::UserCallbacks`/`UserConfig` interface is the right seam; if Omnidroid later writes its own backend, the frontend, decoder and test corpus are the expensive parts and they carry over.

### Immediate work items if we adopt

1. **Fork it.** Upstream is gone; pin `yuzu-mirror/dynarmic@9d45823` (or `azahar-emu`, actively maintained) into `third_party/dynarmic` and own the patches.
2. **Config, non-negotiable:** `fastmem_pointer = 0`, `fastmem_address_space_bits = 64`, `page_table = nullptr`, `enable_cycle_counting = false`, `wall_clock_cntpct = true`, `tpidr_el0`/`tpidrro_el0` pointed at per-thread slots, `code_cache_size` tuned per thread class.
3. **Patch: pass `hook_hint_instructions` through** in `a64_interface.cpp` (one line) so `YIELD` stops costing 87 ns.
4. **Patch: implement the ~40 LSE `INST(...)` entries**, or make absolutely sure `HWCAP_ATOMICS` is never advertised to the guest.
5. **Patch: add `CNTVCT_EL0`** (and `MIDR_EL1`, `ID_AA64ISAR0_EL1`, `ID_AA64PFR0_EL1`) to `system.cpp`'s `SystemRegisterEncoding`, so `clock_gettime` and CPU feature detection do not trap.
6. **Install Omnidroid's VEH before creating any `Jit`**, for demand paging; leave `recompile_on_fastmem_failure = true` as the backstop.
7. **Reserve the guest's address ranges at startup** so host allocations (including the 128 MiB-per-`Jit` code arena) cannot squat where the guest ELF wants to land.
8. **Evaluate `fastmem_exclusive_access = true`** — the single highest-value unverified experiment, since it should remove the global spinlock from `LDXR`/`STXR`.
9. **De-Boost it** (replace `boost::icl` and `boost::variant`) to drop a 240 MB build dependency.
10. **Build a guest-instruction-trace harness** and a per-`Jit` code-cache-usage counter (dynarmic exposes neither publicly) before tuning anything.

### Risks if we adopt dynarmic

| # | risk | severity | evidence | mitigation |
|---|---|---|---|---|
| R1 | **Upstream is dead.** `merryhime/dynarmic` 404s; we own all maintenance. | High | Q1 clone failure | Fork, pin, staff it; azahar-emu is an active downstream to track |
| R2 | **LSE atomics unimplemented.** Every `CAS`/`LDADD`/`SWP` costs ~87 ns and breaks block linking. | High | Q6, Q6b | Hide `HWCAP_ATOMICS`; implement ~40 encodings |
| R3 | **Contended `LDXR`/`STXR` anti-scales 21× from 1→16 threads** (one global spinlock in `ExclusiveMonitor`). | High | Q5c | Try `fastmem_exclusive_access`; else replace the monitor |
| R4 | **Cold-start translation is 0.15–0.31 Mguest-insn/s** (~7–25 s for a Roblox-sized working set) and translations are **duplicated per guest thread**. | High | Q4b, Q5 | Trim optimisation passes; pre-warm; accept a loading screen |
| R5 | **No cache eviction** — full flush at <1 MiB remaining, causing multi-second re-JIT stalls in long sessions. | Medium-High | Q7 | Large `code_cache_size`; add generational eviction (invasive) |
| R6 | **20–35 MiB of committed host RAM per guest thread**, floor independent of code volume. | Medium-High | Q5b | Tune `code_cache_size` down to ~4–8 MiB for minor threads; pool Jits |
| R7 | **Register-bound integer code runs ~33× native** (per-block regalloc, `lahf`/`sahf` flag round-trips). | Medium | Q4 B1 + `dis int` dump | Accept for v1; a new backend is the real fix |
| R8 | **`fastmem_address_space_bits` default of 36 silently degrades to callbacks** instead of erroring. | Medium | Q3c | Assert it at startup; add a regression test that counts slow-path hits |
| R9 | **FP16 arithmetic, BF16, i8mm, `FJCVTZS`, PAC register forms unimplemented.** | Medium | Q6 | Emulate in `InterpreterFallback`; FP16 arith is the likely one to hit |
| R10 | **`ExclusiveMonitor` needs a fixed, up-front processor count**, hostile to dynamic guest thread creation. | Medium | `exclusive_monitor.h` | Over-allocate + id pool |
| R11 | **Invalidation is per-`Jit`**; one guest `mprotect`/`dlclose` must fan out to every thread. | Medium | Q7 | Central invalidation broker in Omnidroid |
| R12 | **No SMC detection at all** — a guest-side JIT (Luau) will execute stale code unless we hook `IC IVAU`/`ISB`/`mprotect`. | Medium | Q7 | Wire all three hooks from day one |
| R13 | **Guest PC capped at sign-extended 56 bits.** | Low | `a64_location_descriptor.h:27` | Constrain the guest allocator; harmless on Win/Android |
| R14 | **Shared address space** means guest fixed-address mappings can collide with host allocations. | Medium | Q3 design consequence | Reserve guest ranges before any host allocation |
| R15 | **`hook_hint_instructions` silently ignored on A64** — a symptom that A64 config plumbing is less exercised than A32. | Low | Q6 | Patch; audit the other `UserConfig` fields against the A64 path |
| R16 | **Boost + CMake-policy + MAX_PATH build friction** in CI. | Low | Q1 | Vendor Boost subset, pin the CMake flag, short build paths |

### Explicitly UNVERIFIED in this spike

* `fastmem_exclusive_access = true` behaviour and performance (R3's mitigation).
* Real Roblox/Android code — everything here is synthetic AArch64 written for the spike.
* `A64::Jit::Step()` single-stepping.
* `DYNARMIC_ENABLE_NO_EXECUTE_SUPPORT` (W^X) builds.
* Behaviour when `code_cache_size` is actually exhausted mid-session (the flush path was read, not driven to exhaustion).
* Exception safety of `Jit` construction across the Rust FFI boundary (the shim does not yet `catch`).
* `MemoryReadCode` returning `std::nullopt` → `Exception::NoExecuteFault` (read in the source, not exercised).
