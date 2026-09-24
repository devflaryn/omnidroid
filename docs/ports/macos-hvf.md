# A native CPU backend on macOS: Hypervisor.framework behind `GuestCpu`

Branch `mac-hvf`, from `port-macos` (`ef65a2d`). Host: Apple M1, 16 GB, macOS 26.5. The owner's
ask: *after parity, measure a native backend behind `GuestCpu`; adopt it only with numbers and a
DECISIONS record.* This is that measurement. **Everything marked MEASURED was run on that machine**
and carries its n and method; everything else is design, labelled as such.

> Written in order: the design first (below, before any code), then what was built, then the
> numbers, then the D34 draft. Where the build changed the design, the design section says so
> rather than being rewritten.

## 1. Facts the design stands on (MEASURED, C probes, before any Rust)

Probes: `hv_vm_create` + `hv_vcpu_run` from a C binary ad-hoc signed with
`com.apple.security.hypervisor`; guest at EL0 under a two-instruction EL1 vector stub; stage-1 on,
16 KiB granule. Each line is one probe run unless an n is given.

| Question | Answer |
|---|---|
| IPA size (`hv_vm_config_get_{default,max}_ipa_size`) | **36 bits, and 36 is also the maximum** on this M1: guest-physical space is 64 GiB |
| vCPUs per VM (`hv_vm_get_max_vcpu_count`) | **64** |
| Where `GuestSpace`'s default 16 GiB reservation lands | `0x3_0000_0000` (or `0x1_xxxx_xxxx`) — inside 64 GiB, so IPA == VA is possible |
| A native two-instruction loop at EL0 in the VM vs the same loop on the host | **5,935 vs 5,934 M insn/s** (4 G instructions each): no measurable cost |
| `svc` at EL0 → EL1 vector → `hvc` → host → `eret` → EL0 | **832-845 ns** per round trip (3 rounds, n = 200,000 each, incl. 2 sysreg reads) |
| same, host resumes by writing `PC`/`CPSR` instead of the stub's `eret` | **786-790 ns** (2 rounds, n = 200,000) |
| a branch to a stage-2 **non-executable** page, resumed by a host `PC` write | **2,092-2,293 ns** (3 rounds, n = 200,000): stage-2 aborts go through the kernel's fault path first |
| `hv_vcpu_get_reg` / `set_reg` / `get_sys_reg` / `get_simd_fp_reg` | 6.2 / 3.6 / 14.0 / 6.0 ns (n = 1,000,000 each) |
| `hv_vcpu_create` / `hv_vcpu_destroy` | 9.3 / 4.3 us (n = 200, mean) |
| `hv_vm_unmap` + `hv_vm_map` of one 16 KiB page; `hv_vm_protect` | 0.8 us; 1.2 us (n = 2,000) |
| Vtimer (`CNTV_CVAL_EL0` 1 ms ahead) | exits `VTIMER_ACTIVATED` after 0.999 ms; 100 us re-armed: 100.6 us mean (n = 1,000). Fires with `DAIF` masked too |
| `hv_vcpus_exit` from another thread | exits `CANCELED` |
| `TPIDR_EL0` at EL0 | readable (`mrs`), set per vCPU with `hv_vcpu_set_sys_reg` |
| `CNTVCT_EL0` at EL0 / `CNTFRQ_EL0` | readable; **24 MHz**, and `CNTVCT = mach_absolute_time() - vtimer_offset` (host `cntvct_el0` differs: macOS keeps its own offset) |
| `CNTPCT_EL0` at EL0 | **traps to the host** (EC 0x18) |
| `WFI` at EL0 / `WFE` / `YIELD` | `WFI` traps to the host (EC 0x01, PC at the `wfi`); `WFE` and `YIELD` complete in the guest |
| `CTR_EL0` at EL0 | `0x8444c004` — the value dynarmic's pin reports |

**Three facts about stage 2 that decide the memory design**, each observed directly:

1. **Host protection is not enforced at stage 2.** A range `hv_vm_map`ped read-write is read and
   written by the guest while the host has it `PROT_NONE` or `PROT_READ`.
2. **`hv_vm_map` binds the memory object present at the time of the call.** After the host replaces
   a range with `mmap(MAP_FIXED)` — a file view placed over the reservation, or omni-platform's
   decommit (a fresh anonymous mapping) — the guest still sees the **old** pages (stale data, and a
   file placed afterwards reads as zeroes to the guest) until the range is `hv_vm_unmap`ped and mapped
   again. A host copy-on-write privatisation of a mapped file view *is* seen without a remap (same
   object chain).
3. `hv_vm_map` over an already-mapped range fails (`HV_ERROR`); `hv_vm_unmap` of a range that is
   partly or wholly unmapped succeeds. So "unmap, then map" is an idempotent update.

## 2. Design (written before the code)

### Exception level: EL0

The guest runs at **EL0**, under an EL1 the backend owns and the guest cannot reach. EL1 would save
one exception-level change per trap, but that change costs tens of nanoseconds against a ~790 ns
exit, and guest code at EL1 could rewrite `VBAR_EL1`, `TTBR0_EL1` and `SCTLR_EL1` — untrusted code
(Global Constraint 11) owning its own translation. At EL0 it owns nothing but its registers.

EL1 is a **vector table of `hvc #n`** (one per vector slot), in a backend-owned page mapped at an
IPA outside every guest range (`0x1000_0000`, inside `__PAGEZERO`, so no guest address can equal
it) and stage-1-mapped EL1-only. Every synchronous exception from EL0 — `svc`, `brk`, an undefined
instruction, a stage-1 fault, a trapped system register — lands in slot 8 and exits to the host
with `ESR_EL1`/`ELR_EL1`/`FAR_EL1`/`SPSR_EL1` describing it. The host resumes by writing `PC` and
`CPSR` (the measured-faster of the two resumes). An exception taken from EL1 itself (any other slot)
is a backend defect and becomes `CpuError::Backend`.

### Translation: stage 1 flat, stage 2 is the truth

* **Stage 1** is one level-2 table (16 KiB granule, `T0SZ = 28`, 36-bit VA): 2,048 identity-mapped
  32 MiB blocks, EL0 read-write-execute, Normal write-back — except the backend's own block, which
  is EL1-only. It never changes after the VM is made. A guest VA at or above 64 GiB is a level-0
  translation fault (typed `MemoryFault`).
* **Stage 2 is where permissions live, with IPA == VA** (D4 unchanged: a guest address is a host
  address). Because of fact 1, stage 2 must *mirror the host's protections itself*, and because of
  fact 2 it must be re-established every time the host replaces an object. Both are done at the one
  place every mapping change in the process already passes through: omni-platform's macOS `vm`
  backend. Its `mmap(MAP_FIXED)`, `mprotect` and `munmap` sites call a hook in the new
  `omni_platform::hypervisor` module, which — only for ranges a backend has *attached* — unmaps and
  re-maps that range at stage 2 with the protection the host just set. So stage 2 equals the host's
  protection, synchronously, on the thread that changed it. A range nobody attached costs one
  relaxed atomic load.
* **Isolation, as a side effect.** Stage 2 maps only attached guest spaces and the backend's page.
  A guest pointer to the Rust heap, a driver mapping or the code cache is a stage-2 fault, not a
  silent access — the opposite of D4 amendment 1's finding for dynarmic.

### Guest faults and demand paging (D4, D10)

* A lazily committed page is `PROT_NONE` on the host, so it is **unmapped at stage 2**. The guest's
  first touch is a stage-2 abort, which exits to the host (EC 0x20/0x24, faulting address in
  `FAR_EL2`). The backend asks `omni_mem::admit` — **the same policy function the demand pager uses**
  — which commits the granule through `GuestSpace`; the commit's `mprotect` passes through the hook,
  which maps it at stage 2; the instruction is retried. D10's ceiling and accounting hold.
* A refusal from `admit` (unmapped, wrong protection, over the ceiling) is a typed
  `ExitReason::MemoryFault` at the faulting instruction, with the access kind from `ESR_EL2.WnR` /
  the exception class.
* A stage-1 fault (address ≥ 64 GiB, or the backend's own block) arrives through the EL1 vector
  instead, and becomes the same typed exit.

### Thunks, the sentinel, `svc`

* A thunk is **an address that traps**, exactly as `GuestCpu::add_thunk` says. The boundary's
  function area is `Protection::Read` — not executable — so a branch there would already be a stage-2
  abort, but that costs ~2.1 us. So the backend **overlays** each page holding a registered thunk:
  at stage 2 that IPA is pointed at a backend-owned page full of `brk #0xF00D`, mapped read+execute.
  A call to a thunk is then `brk` → EL1 → `hvc` → host, the ~0.8 us path. A branch into the middle
  of a slot hits a `brk` too, finds no registration at that address, and is a typed
  `MemoryFault { access: Execute }` — the behaviour `omni-android/src/region.rs` designed the area
  for. The divergence: a guest *load* from the function area reads `brk` words instead of zeroes.
* A thunk address in **executable** guest memory (a real PLT stub, say) cannot be overlaid without
  breaking the code beside it and would need a planted `brk`: refused with `CpuError::Unsupported`
  unless the word there already traps.
* The return sentinel is the same mechanism (`ExitReason::Returned` when the trap is at the armed
  address).
* The guest's own `svc` becomes `UnsupportedInstruction` naming the encoding, as on dynarmic.
* **There are no inline thunks.** `Capabilities::inline_thunks` is `false`: every import crossing
  is an exit, serviced by `Boundary::run` on the exit path (D17's design A). That is the number the
  decision turns on, so it is measured and multiplied by a real crossing rate (below) rather than
  argued.

### Registers, `TPIDR_EL0`, vCPUs and host threads

* A vCPU is bound to the host thread that created it (every `hv_vcpu_*` call must come from that
  thread). A `GuestCpu` is created on one thread and run on another (`pthread_create`), so a context
  cannot own a vCPU. Instead **each host thread lazily creates one vCPU the first time it runs guest
  code** and destroys it when the thread exits; a context loads its registers into the current
  thread's vCPU at `run` entry and saves them back at every exit. The register file between runs
  lives in the context, which is what makes `x()`, `set_sp()` and the rest correct from any thread.
  The save/restore is a cost on every crossing, and it is measured separately.
* `TPIDR_EL0` and `TPIDRRO_EL0` are per-vCPU system registers, loaded with the rest (D13).
* `FPCR` is the vCPU's own, so the MXCSR/FPCR leak dynarmic needed a guard for (D17) cannot happen.
* **64 vCPUs per VM, one VM per process, up to 256 guest threads.** One vCPU per host thread that
  has run guest code; the 65th is refused with a typed `Unsupported` naming the limit. ~39 threads
  at the landing screen fit; 256 would not. M:N (release a vCPU while its thread is blocked in a
  handler, 9.3 + 4.3 us per release and re-acquire) is a condition in the D34 draft, not built.

### Stopping a runaway guest (Global Constraint 11)

* `RunLimit::Instructions` is **refused** (`Capabilities::counted_step_limit = false`): nothing
  counts native instructions. The runtime passes counted budgets in several places
  (`bionic/threads.rs` windows, JNI, AAudio); those need a runtime change to run on this backend,
  and that is listed as open, not papered over.
* `HaltHandle` works through the **vtimer**: every `run` arms `CNTV_CVAL_EL0` one tick ahead
  (default 1 ms); each `VTIMER_ACTIVATED` exit checks the halt flag and re-arms. Latency is at most
  one tick; the cost is one ~0.8 us exit per tick per running vCPU (< 0.1%).
  `Capabilities::asynchronous_halt = true`. (`hv_vcpus_exit` would need a hook in `HaltHandle`,
  which is a plain flag; the vtimer needs none.)

### Exclusives and memory ordering

Native. `LDXR`/`STXR`, LSE atomics and barriers are the hardware's own, on the same physical pages
the host's atomics use, so the host runtime and guest threads synchronise through one coherent
memory system. There is no software monitor to size, no `processor_id`, and D5's risk 3 (a
global monitor that anti-scales and can be wrong) does not exist here. LSE atomics that dynarmic
reports as `UnsupportedInstruction` (D5: 231 unimplemented decoder entries) simply execute.

### Code invalidation

`invalidate_code` does `DC CVAU` / `IC IVAU` over the host-readable executable part of the range.
The guest's own cache maintenance at EL0 (`SCTLR_EL1.UCI`) runs natively.

## 3. What was built

Three layers, each where `ARCHITECTURE.md` §§2 and 6 put it. Everything is behind features that are
off by default; a default build, and every Windows build, compiles exactly what it did.

| Layer | Where | What |
|---|---|---|
| OS seam | `omni-platform/src/hypervisor/{mod,macos,unsupported}.rs`, feature `hypervisor` | `Vm` (one per process, limits read from the host), `Attachment` (the stage-2 mirror), overlays, private mappings, a thread-bound `Vcpu`; `HV_DENIED` is `HvError::Denied` naming the entitlement; every other host is `Unsupported` |
| the mirror's hook | `omni-platform/src/vm/macos.rs` | `mirrored(address, size, prot)` after every `mmap(MAP_FIXED)`, `mprotect` and `munmap` the macOS vm backend makes; nothing without the feature, one atomic load with it and nothing attached |
| CPU backend | `omni-cpu/src/native/{mod,system}.rs`, feature `native-hvf`, `aarch64` | `NativeBackend` / `NativeCpu` behind `GuestCpu` |
| runtime | `omni-android/src/bionic/{threads,mod}.rs` | a guest thread on a backend that cannot count runs unbounded and is stopped through its `HaltHandle` |
| signing | `tools/hvf_run.sh`, `tools/hvf.entitlements` | the cargo runner that ad-hoc signs a test binary with `com.apple.security.hypervisor` |

**As built, against the design above** -- three changes, each found by running real code:

1. **A thunk on a writable data page is not refused.** The boundary registers its eighteen data
   symbols as thunks so that a guest *calling* one is refused by name. Such a page is not
   executable, so a branch there is already a stage-2 instruction abort; the run loop reports it as
   the registered thunk. Slower (~2.1 us) and only reached by a guest calling a data symbol; no
   veneer hides the page's data. Found installing the real boundary.
2. **The watchdog is armed per thread vCPU, not per run** (three framework calls fewer per crossing):
   on the vCPU's first run and after every tick; a tick that fires while the vCPU is idle is the next
   run's first exit.
3. **Registered addresses are classified before the paging policy** on a stage-2 instruction abort,
   so a thunk on a non-executable page is a `Thunk`, not a `MemoryFault`.

Behaviour, stated per trait method: `run` refuses `RunLimit::Instructions` (`Unsupported`);
`last_run_instructions` is 0; `add_inline_thunk`, `add_breakpoint` refuse; `add_thunk` /
`set_return_sentinel` overlay a `BRK #0xF00D` page on a read-only or free page, accept an executable
page whose word already traps (`UDF`/`BRK`) or a writable data page (fetch abort), and refuse real
code; `invalidate_code` is `sys_icache_invalidate` over the host-readable executable part;
`cost` is the TLS block plus the register file (the vCPU is the thread's, measured below).
Guest `svc`, undefined words and trapped system registers are `UnsupportedInstruction` naming the
word; `CNTPCT_EL0` (which traps to EL2 here) is emulated from the same counter as `CNTVCT_EL0`;
`WFI` yields the host thread and continues, as dynarmic's hint arm does.

### Evidence

| Suite | Count | Run |
|---|---|---|
| `omni-platform/tests/hypervisor_macos.rs` | 7 | limits; every register incl. all 32 Q registers byte-exact; an EL1 `hvc`; **stage 2 follows host protect, decommit and detach** (fact 2, fixed); overlays; IPA/alignment refusals; the vCPU limit as `VcpuLimit` |
| `omni-cpu/tests/native.rs`, seam | 16 | capabilities and refusals; a loop, flags and untouched registers; D13 both ways; the vector file; a thunk and a branch into a slot's middle; a data-page thunk; six bad accesses incl. a **host heap pointer** and a VA at 64 GiB; demand paging charged per granule; `svc`/`UDF`/`ID_AA64ISAR0`; `WFI` and both counters; a real `CAS`; a runaway halted within a tick; a context across threads and two per thread; rewritten code; a thread's exit gives its vCPU back |
| `omni-cpu/tests/native.rs`, **the M2 gate** | 6 | the same three real `libroblox.so` functions and predictions as `tests/roblox.rs`: 256 base64 values, 18 timeval vectors, the stack guard in three directions (the second without a breakpoint), the real `CAS` word dynarmic refuses executing, a return into nothing, 8 threads x 1,000 calls |
| `omni-android/tests/native_initializers.rs`, **M3's gate** | 1 | **all 3,594 initializers in order**, the eight pinned words, **exactly 92,431 image pointers** written (the translating run's figure), and the guest thread they start stopped through its `HaltHandle` |
| `mac-hvf-*` mutation rows | see below | |

The translating backend's gates are untouched: `initializers` passes (dynarmic), and the macOS gate
(`gameactivity`, dynarmic) passed twice with the crossing-rate report on. It also failed twice, for
reasons that are not this branch's: once because this worktree's APK was a symlink (the merge
notes), and once when the engine stalled before `APP_READY(Landing)` and never reached Vulkan (0
entry points resolved; the game thread did not stop in 60 s) -- the network-dependent stall the
gate has shown before; that run is excluded from 4.2.

## 4. Measurements

All on the M1 above, release builds, test binaries signed by `tools/hvf_run.sh`. "Median [min]".

### 4.1 One import crossing -- the number the decision turns on

`measure_thunk_crossing_cost`: a guest loop calling a thunk through `BLR`, the host adding 1 to X0
and resuming at X30, n = 100,000 crossings per round, 7 rounds, same program on both backends.

| | ns per crossing |
|---|---|
| native: a VM exit per crossing (every import) | **1,614 [1,554]** (before the per-thread watchdog: 1,648 [1,630]) |
| of which: saving the register file (31 X, 32 Q, SP, FPCR/FPSR, TPIDR: 67 framework calls) | 638 [568] |
| of which: loading it in full (on a thread switch; a crossing loads only what the host changed) | 556 [510] |
| floor: `svc` -> EL1 -> `hvc` -> host -> resume, C probe, no register file | 786-790 |
| dynarmic: exit to the caller (D17 design A) | 37.6 [36.9] |
| dynarmic: inline dispatch (design B, what the runtime uses) | **23.7 [22.8]** |

**A native crossing costs 68x dynarmic's inline dispatch.** Over half of it is the hypervisor's
own exit; the register save is the backend's and is kept eager on purpose: a `GuestCpu` is `Send`,
a vCPU is bound to its thread, and a context whose registers were left in one thread's vCPU could not
be read from another.

### 4.2 The crossing rate at the landing screen (dynarmic, the real gate)

`OMNI_CROSSING_RATE=1` (this branch; off by default, read-only) on the macOS gate command,
`OMNI_SESSION_SECONDS=60`, 5 s windows; "steady" is +25..+55 s, after `APP_READY(Landing)`.
Two runs reached the landing screen (a third stalled before it on the network and is excluded).

| run | all crossings | guest thread 5 | every other thread | exit path |
|---|---|---|---|---|
| A (landing; presents erratic, window not in front) | 1.52 M/s | 1.12 M/s | **0.40 M/s** | 0.31-0.44 M/s |
| B (landing; ~60 fps; `OMNI_PROFILE=1`) | 3.97 M/s | 3.44 M/s | **0.52 M/s** | 1.16-1.19 M/s |

* **Guest thread 5 is a poll loop**: `ALooper_pollOnce`, `pthread_mutex_lock`, `pthread_mutex_unlock`
  in equal numbers (1.15 M/s each in run B), 17% of its samples in guest code, 83% in those three
  handlers (`OMNI_PROFILE`, 2 ms). It is a spin: it uses a core whatever a crossing costs, and its
  rate is simply how fast the host lets it go round.
* **Every other thread together crosses 0.40-0.52 M/s**, and without the looper's calls the hottest
  imports are (per second, steady, run A / run B): `pthread_getspecific` 203k / 264k, `clock_gettime`
  39k / 85k, `memcpy` 29k / 34k, `memset` 14k / 24k, `strcmp` 20k / -, `pthread_mutex_lock` and
  `_unlock` 15k / 19k each, `__errno` 8k / 12k, `memcmp`, `strlen`, `memmove`, `pthread_once`.
* **Multiplied out**: 0.40-0.52 M/s x 1.61 us = **0.64-0.84 of a core in VM exits** for the threads
  doing the work, against ~0.01 on dynarmic (23.7 ns inline). The busiest working threads cross
  75-155k/s each (render 108k/s at 13% in guest code: natively its guest code would shrink to ~2% of
  a core and its exits grow to ~17%).

### 4.3 Guest code speed on real engine code

| workload | dynarmic | native | |
|---|---|---|---|
| **compute**: 33 real functions, 69.7 M instructions (see method) | warm **1,320** M insn/s (cold 1,121) | **7,553** M insn/s | **5.7x** faster (per function 1.7-83x, most 2-7x) |
| the 870 runnable leaves, one call each (avg 9.3 instructions) | 62.7 ns per call | 1,639 ns per call | **26x slower**: per-call cost dominates |
| a two-instruction loop, 4 G instructions (C probe) | 4,873 M/s (CPU workstream) | 5,935 M/s | = the host's own 5,934 |

Method for the compute row (`measure_compute_throughput_on_real_functions`): **all 245,117**
`.eh_frame` functions were run natively once with X0/X2 pointing at two 1 MiB buffers, X1/X3 their
length and X4-X7 = 64 (5.4 s; 31,290 returned, 213,614 stopped with a typed exit, 213 halted after
20 ms, 0 refused); the 82 that returned after >= 40 us were run on dynarmic **over the same guest
space** (same library, buffers, stack) to count their instructions; the 33 with >= 200,000 that
returned were timed warm on both (median of 3, buffers refilled identically) and kept only when both
backends left the same X0 and the same buffer contents -- all 33 did.

### 4.4 Startup: the 3,594 initializers

`measure_the_initializer_run`, one instance per process (an instance is never released, and its
started thread slowed a second instance 1.4-1.9x, MEASURED), alternating, n = 8 each:

| | cold run | guest instructions | crossings on the initializer thread | `phys_footprint` added by the run (n = 3) |
|---|---|---|---|---|
| dynarmic | **2,417 ms** median (2,284-2,666) | 91,570,432 (counted) | 457,831 (all inline) | **+99.8 MiB** |
| native | **795 ms** median (761-852) | (not countable) | 457,831 (all VM exits) | **+23.5 MiB** |

**3.0x faster, and 76 MiB less.** Derived, not measured: 457,831 exits x ~1.6 us is ~0.73 s of
native's 0.80 s -- the native startup is almost entirely crossing cost, and dynarmic's almost
entirely translation.

### 4.5 A demand-paged first touch

`measure_demand_paging_fault_cost`: a store into each of 1,024 untouched 64 KiB granules, minus the
same loop over them committed, 5 rounds, median:

| native (stage-2 abort, `admit`, the mirror's remap, retry) | **6.37 us** per granule |
|---|---|
| dynarmic (host fault, Mach exception handler, pager, retry) | **24.77 us** per granule |

### 4.6 Memory per guest thread

`measure_memory_per_guest_thread`: `phys_footprint` with 32 parked threads that each created a
context and ran one tiny function, minus 32 parked threads that did nothing guest-related.

| | per thread |
|---|---|
| native (context + the thread's vCPU) | **78.6 KiB** |
| dynarmic (context + jit), one tiny function | 47.0 KiB |
| dynarmic at the landing screen (docs/ports/macos.md, `MallocStackLogging`) | **~33 MB** (per-jit block maps, fastmem patch maps; each thread's own translation) |

The native figure does not grow with the code a thread runs, because nothing is translated; the
initializer run above is the same fact at scale (+23.5 MiB against +99.8).

### 4.7 Robustness and isolation (not a speed, and not optional)

* **The survey above ran all 245,117 real functions natively with garbage-shaped arguments; the
  process survived every one.** Surveying on dynarmic first, `libroblox.so + 0x224822c` **aborted
  the whole test process** (`dynarmic: Segfault happened within JITted code ... wasn't at a fastmem
  patch location`, SIGABRT) -- a Global Constraint 11 violation on the translating arm64 path,
  reported here for the CPU workstream. **Deterministic, and state-dependent**: replaying the
  survey's sequence aborts at the same function and host PC offset every time
  (`repro_dynarmic_aborts_during_the_survey_at_libroblox_0x224822c`, `#[ignore]`d, aborts the
  binary), while that function **alone** is an ordinary typed `MemoryFault { address: 0x51 }` on
  both backends -- so what breaks is left behind by the functions before it.
  **Fixed on `port-macos` (patch 0014, orchestrator):** delta debugging reduced it to two
  functions -- `0x2247264` leaves a pointer into `.data.rel.ro`, `0x224822c` swaps through it with
  the outlined `__aarch64_swp8_rel` -- and the store-release of patch 0007's inline store-exclusive
  was not a fastmem patch location. With 0014 the function is a typed write fault and the dynarmic
  survey runs all 245,117 functions without an abort (`omni-cpu/tests/exclusive_store_fault.rs`,
  row `mac-cpu-E1`).
* **Stage 2 maps only the attached guest space**, so a guest pointer to the Rust heap, a driver
  mapping or the code cache is a typed `MemoryFault` (`every_bad_access_is_a_typed_fault...` reads a
  real host heap pointer). D4 amendment 1's "identity fastmem does not confine the guest" does not
  hold for this backend. The flip side is a condition below: anything that hands the guest a host
  pointer outside `GuestSpace` stops working.
* LSE atomics, FP16, `FJCVTZS` and the rest of dynarmic's 231 unimplemented decoder entries (D5)
  simply execute; exclusives and ordering are the hardware's own, shared with the host's atomics.

### 4.8 Limits found

* **Guest-physical space is 36 bits (64 GiB) on the M1**, default and maximum. With IPA == VA a
  guest space must lie below 64 GiB: the default placement does (`0x3_0000_0000`), the dynarmic
  harness's deliberately high space does not, and in one process the fifth 16 GiB space was placed
  above it and refused -- by name.
* **64 vCPUs per VM, one VM per process**; one vCPU per host thread that has run guest code (39 at
  the landing screen fit). `hv_vcpu_create` 9.3 us, `hv_vcpu_destroy` 4.3 us (n = 200).
* `hv_vm_map` binds the object present at the time; host protection is ignored (section 1). The
  mirror handles both, but every `mmap`/`mprotect` in an attached range costs an extra
  `hv_vm_unmap` + `hv_vm_map` (0.8 us for 16 KiB, 1.6 us for 64 MiB untouched).

## D34 (draft) — A native CPU backend under Hypervisor.framework: not the default now; adopt when hot imports stop being VM exits

**Status: draft, for the owner.** Nothing here changes the default backend; `native-hvf` is off by
default and dynarmic remains the backend every gate runs on.

**What the numbers say.** On this M1 the native backend runs real engine compute **5.7x** faster
than dynarmic's warm translated code (7.55 against 1.32 G insn/s, 33 real functions), starts the
3,594 initializers **3.0x** faster (795 against 2,417 ms), pays **76 MiB less** for doing so, costs
**79 KiB per guest thread** that does not grow with the code it runs (dynarmic: ~33 MB per thread at
the landing screen, its own translation), resolves a demand-paged first
touch **3.9x** cheaper, survived all 245,117 real functions where dynarmic aborted the process on
one, and confines the guest to its own space. Against that, **one import crossing costs 1.61 us --
68x dynarmic's 23.7 ns inline dispatch** -- and at the landing screen the threads doing real work
cross **0.40-0.52 M times a second**, which natively is **0.64-0.84 of a core in VM exits**, of the
same order as the guest compute the backend saves there. A short guest call is 26x slower.

**Decision (draft): do not adopt as the default now. Adopt when these hold, measured on the gate:**

1. **The hot imports are served in the guest.** `pthread_getspecific` (the largest single rate),
   `__errno`, `clock_gettime` (from `CNTVCT_EL0`, which the guest reads natively), `memcpy`, `memset`,
   `memmove`, `memcmp`, `strlen`, `strcmp`, and the uncontended paths of `pthread_mutex_lock`/`unlock`
   (atomics in guest code; exit only on contention). Together these are ~90% of the working
   threads' 0.40-0.52 M/s (4.2); the bar is **< 50k VM exits/s at the landing screen**
   (< 0.1 of a core). The looper's `ALooper_pollOnce` stays an exit and its loop stays a spin.
2. **Nothing in the runtime passes a counted budget to a backend that cannot count.** Done here
   for guest threads and their exit destructors; still counted in JNI native methods, input,
   AAudio callbacks, script downcalls and the lifecycle (`RunLimit::Instructions` in
   `jni/env.rs`, `jni/input.rs`, `jni/script.rs`, `aaudio/mod.rs`, `bionic/threads.rs` windows'
   callers). The native backend bounds them with its `HaltHandle` (a vtimer tick, 1 ms).
3. **The macOS gate passes natively** (graphics, network, no thread killed), with the host-pointer
   paths closed first: the backend confines the guest, so a `vkMapMemory` or callback pointer
   outside `GuestSpace` that works on dynarmic faults here (D4 amendment 1's recommendation to keep
   such memory inside the guest space becomes a requirement).
4. **More than 64 guest threads is handled**: one VM per process allows 64 vCPUs and the engine
   may make 256. Release a thread's vCPU while it blocks in a handler (9.3 + 4.3 us per release and
   re-acquire, measured), or keep the refusal and measure that the engine never exceeds it.
5. The shipping binary is signed with `com.apple.security.hypervisor`, and each instance is its own
   process (IPA == VA needs its space below 64 GiB; one VM per process).

**What would change the answer.**

* *Towards adopting sooner*: condition 1 measured below the bar -- then the memory, startup and
  robustness wins stand with little left against them. The memory win goes straight at the owner's
  per-instance target: the ~1.3 GB of per-thread translation state dynarmic holds at the landing
  screen (39 threads x ~33 MB) does not exist natively. (The native landing-screen footprint itself
  is not measured: condition 3.)
* *Towards not adopting*: the other workstream bringing dynarmic's per-thread memory down to a few
  MB (shared code cache / bookkeeping) removes the largest win; if in-guest import service proves
  impractical (the handlers are Rust; serving them in guest code means a second implementation of
  each in ARM64, which must agree with the first), the crossing cost stays, and native is a net
  loss for any thread whose guest code runs less than **~2 us between host calls** (break-even: the
  1.59 us a crossing adds against the 82% of guest time the 5.7x saves). At the landing screen the
  render thread runs ~1.2 us of guest code per crossing (13% of a core over 108k crossings/s).
* *Unmeasured and able to move it*: gameplay (not the landing screen) -- more compute per crossing
  favours native; the vCPU register save (0.6 us of every exit) could be halved by trapping FP/SIMD
  lazily (`CPACR_EL1`), at the cost of one extra exit in each run that uses them.

### Mutation rows

`python3 tools/mutate.py --only mac-hvf-`: **26/26 caught** -- 9 on the hypervisor seam and the vm
hook (each caught by `hypervisor_macos`), 14 on the backend (by `native`, the M2 gate among them),
3 on the runtime (two by the native M3 gate, and the B row -- dynarmic's guest threads losing their
windows -- by `bionic`'s `a_runaway_guest_thread_stops_at_a_window_boundary`). Pre-flight: 26/26
patterns unique, 4/4 commands pass unmutated; `git diff --exit-code crates tools` clean after.

## Merge notes

Shared files this workstream edits, each minimal and additive; nothing changes for Windows (every
edit is behind a feature that is off by default, or in the macOS vm backend):

| File | Edit |
|---|---|
| `crates/omni-platform/Cargo.toml` | `[features] hypervisor = []` |
| `crates/omni-platform/src/lib.rs` | `#[cfg(feature = "hypervisor")] pub mod hypervisor;` |
| `crates/omni-platform/src/vm/macos.rs` | `mirrored(address, size, prot)` after the five mapping changes (`fresh_reserved`, `mprotect`, `map_file`'s `mmap`, `unmap_and_release`, `release`) and the ten-line helper; compiles to nothing without the feature |
| `crates/omni-cpu/Cargo.toml` | `aarch64`-only optional `omni-platform` with `hypervisor`; feature `native-hvf` |
| `crates/omni-cpu/src/lib.rs` | `#[cfg(all(feature = "native-hvf", target_arch = "aarch64"))] pub mod native;` |
| `crates/omni-android/Cargo.toml` | `[features] native-hvf = ["omni-cpu/native-hvf"]` (test targets only) |
| `crates/omni-android/src/bionic/threads.rs` | `drive` and the exit destructors run unbounded, with the halt registered, **only** when `counted_step_limit` is false |
| `crates/omni-android/src/bionic/mod.rs` | the `uncounted_halts` registry and `UncountedHalt` guard; `stop_guest_threads` requests them (empty on dynarmic) |
| `crates/omni-android/tests/gameactivity.rs` | `OMNI_CROSSING_RATE=1` report in the frames block (off by default, read-only) |
| `tools/mutate_mac/cpu.py` | the `mac-hvf-*` rows and their command helpers |
| `docs/ports/macos.md` | one line under "A native CPU backend: first numbers" pointing here |

New files: `crates/omni-platform/src/hypervisor/{mod,macos,unsupported}.rs`,
`crates/omni-platform/tests/hypervisor_macos.rs`, `crates/omni-cpu/src/native/{mod,system}.rs`,
`crates/omni-cpu/tests/native.rs`, `crates/omni-android/tests/native_initializers.rs`,
`tools/hvf_run.sh`, `tools/hvf.entitlements`, this file.

Not code: this worktree's APK is a **hard link**, not the symlink it was set up with -- the gate
hard-links the APK into the guest's root as `base.apk`, a hard link of a symlink is a symlink, and the
guest filesystem refuses symlinks, so the render thread died (MEASURED, first gate run here).

## Open, and what each costs

* **The macOS gate has not run natively** (condition 2 and 3). Consequence: no native landing-screen
  figure for frames, memory or CPU; the D34 estimate multiplies measured costs by measured rates.
* **No in-guest import service** (condition 1). Consequence: every import is a 1.6 us exit.
* **No M:N vCPUs**: the 65th guest thread that runs code is refused (`Unsupported` naming the limit).
* **No breakpoints, no planted traps in guest code**: `add_breakpoint` and a thunk inside real code
  are refused by name.
* **A guest load from a veneered page reads `BRK` words** rather than the zeroes or fault it would
  get; only the boundary's function area is veneered, and nothing loads from it on a working path.
* **The thread-exit destructor path on a non-counting backend has no mutation-proven test**: nothing
  in the native suites makes a guest thread return with destructors registered.
* **The watchdog's re-arming has no mutation row**: a surviving mutation would leave the halt test
  spinning for ever and hang the harness. The halt test itself (`a_runaway_guest_is_halted...`) runs.
* **The boundary takes `crossings.lock()` on every exit-path crossing**, a process-wide lock that on
  this backend is on *every* import; contention across threads is unmeasured.
* ~~**dynarmic aborted the process on `libroblox.so + 0x224822c`**~~ (4.7) -- fixed by patch 0014.
