# One translation cache per guest address space

Status: **design, written before the code** (2026-09-25). The results section at the end is filled
in by the implementation and its evidence; the decision is D38.

## 1. The problem, as measured

Every guest thread has its own dynarmic `A64::Jit`, and every `Jit` owns a `BlockOfCode` (its code
cache and prelude) and an `A64EmitX64` (its block map, link table, fastmem table). So a block of
engine code is translated **once per guest thread that runs it**, and held once per thread.

MEASURED (Pet Simulator 99, Windows, HANDOFF 2026-09-25, runs w17-w21; Linux l6):

1. **Input stalls.** A drag or key press drops the frame rate from ~55 to 2-20 fps for several
   seconds while translation jumps from 5-20k to 150-310k guest instructions per second (w20). The
   input runs code no worker had run, the engine spreads it over its 8 TaskScheduler workers, and
   each worker translates it again into its own cache while the frame waits for them.
2. **Translation is 53% of dynarmic's own samples** in the settled world (w17); a busy worker still
   translates 56k instructions a second at +300 s.
3. **Memory**: ~0.4-0.5 GiB of translated code per instance, the same engine code once per thread;
   plus each thread's own prelude and constant pool (2 MiB committed each since patch 0017, D32).
4. **Linux (4 cores)**: translation (`dyn`) is 32-36% of the busy workers' samples.

Cold translation costs about 2 us per guest instruction (D5 amendment 2: 0.516 M guest insn/s on
real Roblox leaves), so 150-310k instructions a second is 0.3-0.6 of a core spent re-deriving code
that another thread already has.

## 2. What emitted code embeds that belongs to one `Jit` (x64 backend, the pin + 0001-0021)

A shared cache means code emitted by one thread's translation is executed by every thread. So
everything a block or the prelude bakes in has to be either **the same for every thread of the
space**, or **read from `JitState` at run time** (`r15` is the running thread's `A64JitState` in all
generated code: `BlockOfCode::GenRunCode` does `mov r15, ABI_PARAM1` and nothing reallocates it).
The inventory, from reading the backend:

| embedded value | where | per thread? | shared design |
|---|---|---|---|
| `UserCallbacks*` as the `this` of every callback (`Devirtualize<...>(conf.callbacks)` -> `mov rdi/rcx, imm64`) | `a64_emit_x64.cpp` SVC, `ExceptionRaised`, cache ops, ISB, `CNTPCT`, `InterpreterFallback`; `a64_emit_x64_memory.cpp` the 128-bit accessors and **all** fastmem fallbacks (prelude); `emit_x64_memory.cpp.inc` the no-fastmem callback path; `a64_interface.cpp` `AddTicks`/`GetTicksRemaining` in the run-code prelude | **yes** (it carries the thread's `ctx`) | loaded from a new `JitState` field: `mov param1, [r15 + od_callbacks]`. The function address is the same for every thread (one callback class; on MSVC the member pointer is a vcall thunk anyway) |
| the dispatcher's `LookupBlock` argument (`Jit::Impl*`) | `GenRunCodeCallbacks`, prelude | **yes** | `mov param1, [r15 + od_lookup_arg]` |
| `&conf` (the `UserConfig` a callback lambda reads `callbacks`, `processor_id`, `global_monitor` from) | non-inline exclusive read/write, `emit_x64_memory.cpp.inc` | **yes** | `mov param1, [r15 + od_conf]` |
| the thread's monitor slots, `GetExclusiveMonitor{Address,Value}Pointer(monitor, processor_id)` as `imm64` | inline `LDXR`/`STXR` (`EmitExclusive{Read,Write}MemoryInline`) | **yes** | `mov tmp, [r15 + od_exclusive_address]` / `od_exclusive_value` |
| the global monitor's scan, unrolled over every slot **except the thread's own** | `EmitExclusiveTestAndClear`, `emit_x64_memory.h` (global monitor only) | **yes** (which slot is skipped) | scan every slot including one's own. This is exactly dynarmic's own reference path: `ExclusiveMonitor::CheckAndClear` clears every matching slot including the caller's, and the caller's slot is not read again before its next `LDXR` rewrites it. Under value-compare (the default, D31) there is no scan at all |
| the lock word of the global monitor | `EmitExclusiveLock/Unlock` | no (one monitor per space) | unchanged; attach requires the same monitor |
| `TPIDR_EL0`/`TPIDRRO_EL0` box addresses (`mov r, imm64; mov r, [r]`) | `EmitA64GetTPIDR`, `GetTPIDRRO`, `SetTPIDR` | **yes** | `mov r, [r15 + od_tpidr_el0]; mov r, [r]` |
| the fast-dispatch table's address (0019's 64 KiB table) | `GenTerminalHandlers` (prelude), `fast_dispatch_table_lookup` | **yes** -- and it must stay per thread: an entry is 16 bytes written non-atomically by the thread that owns it, so a shared table could pair one location's descriptor with another's code pointer | `mov r12, [r15 + od_fast_dispatch_table]`; each thread keeps its own 64 KiB table |
| block-link patch sites: `jg/jz/jmp rel32` rewritten in place by `Patch`/`Unpatch`, 22-23 bytes each, from `mov rax, pc; mov [r15+pc], rax; jg RFRC` to `jg target; nop...` | `EmitPatchJg/Jz/Jmp`, `EmitX64::Patch` | no, but **rewritten while the code runs** | see section 4: shared code is never rewritten; a link goes through an 8-byte data slot |
| the RSB push's `mov rcx, imm64 target_code` patch site | `PushRSBHelper`, `EmitPatchMovRcx` | no, but rewritten | `mov rcx, [rip + slot]` |
| `r13 = fastmem_pointer`, `r14 = page_table` loaded in the run-code prelude (`GenRCP`) | prelude | no (one address space) | unchanged; attach requires them equal |
| return-stack buffer contents, `rsb_ptr`, `halt_reason`, `exclusive_state`, registers | `A64JitState` via `r15` | yes, already in `JitState` | unchanged |
| cycle budget (`cycles_remaining`, `cycles_to_run`) | the run-code frame on the host stack (`StackLayout`) | yes, already per call | unchanged |
| constant pool entries (`rip`-relative) | `ConstantPool` | no | shared; written only under the emission lock, read-only once published |
| the prelude's own routines (`return_from_run_code[]`, terminal handlers, fallbacks, 128-bit accessors) reached by `rel32` from every block | `BlockOfCode`, `A64EmitX64` | per `Jit` today because the prelude is | **one prelude per shared cache**, all its per-thread values read from `JitState` as above |
| Windows SEH registration (`RtlAddFunctionTable` over the whole buffer) and the POSIX signal handler's code-range record | `ExceptionHandler::Register` | per buffer | one per shared buffer |
| `fastmem_patch_info` (faulting `rip` -> fallback), `do_not_fastmem` | `A64EmitX64` | per emitter | shared, read under the cache lock by the fault handler |

`A64JitState` gains eight 8-byte fields **at its end**, so no existing offset moves and the
per-thread path emits byte-for-byte what it did.

The arm64 backend embeds the same classes of value in different places (the prelude trampolines
carry `this` and `&conf` as literals, `emit_arm64_a64.cpp` inlines the TPIDR box addresses, patch
0007's inline exclusives the monitor slots; blocks reach the prelude with `BL`), and its address
space was rebuilt by 0010-0016 into compact records. Section 8 says why it is not done in this
change.

## 3. Ownership

A new object, `A64::SharedCodeCache` (vendored patch 0022, x64 backend), owns **one**
`BlockOfCode` (prelude, constant pool, code) and **one** `A64EmitX64` (block map, link table, guest
ranges, fastmem table). It is created from a *template* `UserConfig` -- every field that shapes
emitted code -- and a size. A `Jit` whose `UserConfig::shared_code_cache` is set does not build
either; it keeps only what is the thread's:

* `A64JitState` (registers, RSB, halt word, exclusive state) plus the eight new fields pointing at
  its own callbacks object, its own `UserConfig`, its `Jit::Impl`, its two monitor slots, its two
  TPIDR boxes and its fast-dispatch table;
* its fast-dispatch table (64 KiB, as 0019);
* its pending invalidation requests;
* its epoch word (section 6).

Attach refuses (the shim returns null) a `Jit` whose configuration differs from the template in
anything that shapes code: optimization flags, the unsafe gate, fastmem pointer/width/mirroring,
`fastmem_exclusive_access`, the recompile flags, the monitor, cycle counting,
`check_halt_on_memory_access`, `define_unpredictable_behaviour`, `wall_clock_cntpct`, hint hooking,
`CNTFRQ/CTR/DCZID`, a page table, and the presence of the TPIDR boxes. Only `callbacks`,
`processor_id` and the TPIDR box addresses may differ, and those are exactly the fields that became
`JitState` loads.

## 4. Linking without rewriting code another thread is running

Upstream links block `A` to block `B` by rewriting a 22-23-byte sequence in `A` (`mov rax, pc; mov
[r15+pc], rax; jg RFRC`) into `jg B; nop...` when `B` is emitted, and back when `B` is invalidated.
With per-thread caches no other thread can be executing `A` while that happens. With one cache,
another core may be anywhere inside those bytes -- e.g. past the 10-byte `mov rax, imm64`, about to
fetch at offset 10, which after the rewrite is the middle of a multi-byte `nop`. Cross-modifying a
multi-instruction sequence is not safe on x64 or arm64.

So in shared mode **emitted code is never modified after it is published**. Every link site jumps
through its own 8-byte **slot**:

```
    cmp qword [rsp + cycles_remaining], 0     ; unchanged: the budget check (or the halt word
    jng .exit                                 ;   without cycle counting)
    jmp qword [rip + slot]                    ; slot = B's entry, or .unlinked
.unlinked:
    mov rax, B.pc
    mov [r15 + pc], rax
    jmp ReturnFromRunCode                     ; the dispatcher: checks halt and budget, looks up B
.exit:
    mov rax, B.pc
    mov [r15 + pc], rax
    ForceReturnFromRunCode
```

which is upstream's terminal with the same outcomes on every path (linked: straight to `B`;
unlinked: dispatcher; budget spent or halt raised: leave `Run`). Linking and unlinking are one
aligned 8-byte store to the slot, which every x64 and arm64 core performs atomically, and a thread
that loads the slot gets either the old or the new target -- both valid code. The RSB push loads its
code pointer the same way (`mov rcx, [rip + slot]`, unlinked value `ReturnFromRunCode`, as upstream).

*As designed*, slots were allocated from the top of the current region, growing down, apart from
the code. **As built** they are 8 aligned bytes right after the block that reads them: MEASURED,
slots a region away cost an extra cache and TLB miss per linked transition once the working set is
large (section 11), and a store to a slot next to code is a self-modifying-code event for a core
that has the line only at link and unlink time, which are rare. A dropped block's own slots are
unlinked and taken out of their targets' records (without that, a target's record grew by one
slot per translation of every block linking to it, and relinking it walked them all: MEASURED in
the stress test, emission slowed from ~9 to ~42 us per block).

Cost: one load-and-indirect-jump per linked transition instead of a direct `jg`. Predicted well by
the branch target buffer; measured in section 9.

## 5. Translation is serialized; execution is not

One `std::shared_mutex` per cache.

* **Lookup** (the dispatcher's `LookupBlock`, taken on a fast-dispatch miss, an unlinked slot, and
  at `Run` entry): first the running thread's **own fast-dispatch table**, without any lock, then the
  block map under the lock, shared, and the answer goes into the thread's table. MEASURED: without
  the first step every run entry and dispatcher return of eight threads took the lock, and a warm
  call on the real Roblox leaves cost 0.56 us instead of 0.20.
* **Miss**: the frontend and the IR passes run **outside** the lock, with the location marked in
  flight; a thread that needs a location another is translating waits for it (spinning with yields,
  then on a condition), so a burst of new code reaching eight threads is translated once, not
  eight times. Emission takes the lock exclusively: probe again, and if an invalidation applied
  meanwhile touches the block's guest range (a serial number and the last 64 ranges are kept),
  translate again under the lock; emit, register, link waiting slots, unlock. Translation reads
  guest code through the *translating* thread's `MemoryReadCode` -- the same bytes in one address
  space; the omni-cpu side (section 7) makes what it plants there a property of the space, not of
  the thread.
* **Found while measuring, and fixed**: the emitted fast-dispatch handler, on a miss, writes the
  location into the table entry *before* it calls the lookup, and fills in the code pointer after.
  Once the lookup consults that table, it can find its own location paired with the code pointer
  of whichever location last held the entry, and return it. MEASURED: intermittently, per process
  (the hash takes the table's address), every call of a thread ran into another function's code
  (`the_cost_of_eight_threads_meeting_the_same_real_roblox_code` saw `StepLimitReached` on every
  call in 1 run of 5). In shared code the entry is now written whole after the lookup;
  `indirect_calls_that_collide_in_the_fast_dispatch_table_reach_their_own_targets` overfills the
  table (8,192 targets) and fails without it.
* Code, slots and the constant pool are written only under the exclusive lock, into memory no
  thread has been given a pointer to yet, and published by the block-map insert and the slot stores
  (release). On x64 instruction fetch is coherent with stores from other cores; fresh code published
  this way is how every multithreaded JIT hands code between threads.
* **Execution** takes no lock. Generated code only ever reads the cache's code and slots.

The fault handler (`FastmemCallback`) runs on the faulting thread and only *reads*
`fastmem_patch_info` under the shared lock. A thread never faults in generated code while it holds
the lock (generated code never runs inside `GetBlock`), so this cannot deadlock on itself. In shared
mode `recompile_on_fastmem_failure` and `recompile_on_exclusive_fastmem_failure` are **off**: a
declined fault is still routed to the fallback callback on every occurrence (that is the `FakeCall`
the handler returns either way), but the block is not recompiled onto the callback path for every
thread -- which would both rewrite shared code from inside a fault handler and put other threads'
healthy accesses of that instruction on the 30-49x path, tripping their per-slice
`DegradedMemoryPath` invariant. Guest-visible behaviour is the same: a declined access is serviced by
the callback, which reports the typed fault.

## 6. Invalidation, per-thread routing state, and reclaiming memory

**Invalidating** (`InvalidateCacheRange`, `ClearCache`, a guest `IC IVAU`/`IALLU`, a thunk or
sentinel planted or removed) is applied to the shared structures under the exclusive lock: the
blocks in range are dropped from the block map, and every slot linking to them is stored back to its
`.unlinked` value. Their code is **not** freed: threads may be executing it, and it stays valid,
unreachable through the map and the slots.

What still reaches a dropped block is **per-thread routing state**: each thread's return-stack
buffer and fast-dispatch table. A cache-wide `generation` counter is bumped by every invalidation
that dropped a block; each thread compares it with the value it last saw **at every `Run` entry**
and, if it moved, clears its RSB and its fast-dispatch table before running. The thread that
requested the invalidation does the same immediately.

The guarantee this gives, stated against today's: a thread stops executing a dropped translation no
later than its next `Run` (an omni-cpu budget slice, a million instructions by default) -- and much
sooner in practice, since its direct links now leave through `.unlinked`. Today another thread keeps
its **own** stale translation until omni-android's queue reaches it at its next run *segment*
(`boundary.rs` `drain_invalidations`), which is at least as late: a segment is one or more slices.
The requesting thread itself sees its own invalidation before it runs again, exactly as now.

omni-android's cross-thread queue exists only because translations were per thread. In shared mode
the backend says so (`Capabilities::shared_translation`), and `ReentrantCall::invalidate_code`
applies the range once instead of broadcasting it to every other context. That also removes the
"queue overflowed, invalidate the whole space" collapse (`MAX_PENDING_INVALIDATIONS`), which with a
shared cache would throw away every thread's code whenever one idle thread fell 64 ranges behind.

**When is a range applied?** Called on a thread that is not executing (omni-android's exit path),
`od_jit_invalidate_range` applies it at once -- not at that thread's next `Run`, because that thread
may never run again, and with a shared cache the stale translation would then outlive it for every
other thread. Called from inside a callback, it is queued and the thread halts, as upstream; the
halt ends the block, `Run` returns, and the queue is applied before anything else runs.

**Reclaiming memory.** Upstream frees code only by resetting the whole cache (full cache,
`ClearCache`), which with other threads running would pull code out from under them. The shared
buffer is therefore split into **regions** after the prelude. Blocks are emitted into the current
region. When it is full, it is **retired**: every slot in it is unlinked, the maps are emptied, the
`generation` and an `epoch` counter are bumped, and emission moves to a free region. A retired
region is **reclaimed** -- decommitted and made free -- once every attached thread either is not
inside `RunCode` or entered it after the retirement: each thread publishes, at `Run` entry, the
epoch it entered at (seq-cst, before it reads `generation`), and `UINT64_MAX` when it leaves. A
thread that entered before the retirement may still be executing retired code or hold RSB/table
entries into it; a thread that entered after has flushed them. This is epoch-based reclamation with
`Run` as the quiescent point.

**A thread blocked inside a host callback** (an inline import that waits, e.g.
`pthread_cond_wait`) is inside `RunCode` for as long as it waits, and will return into the block it
called from. *As designed* it pinned every region retired while it waited.

*As first built* it published its return site while the callback ran, and the reclaimer kept the
pages around each parked thread's site as a hole and reused a region only if 4 MiB of it was free
of holes. **That failed in the world** (D38 amendment 1, run w24): a game keeps dozens of threads
parked in imports that wait, their sites spread through the region, so a retired region came back
in small pieces or not at all; a working set larger than the piece it got filled it, retired it --
forgetting every block -- and started again, for minutes; and every thread leaving `Run` retried
the reclaim each millisecond, each try an asymmetric barrier (an IPI to every core) and a
re-decommit.

**As built now** a parked thread holds nothing of any region. Shared code calls every `SVC`
callback through a prelude trampoline: the block jumps there with its resume address in a
register; the trampoline publishes it in `JitState::od_callback_return`, calls, takes it back with
an `xchg` (clearing it) and jumps to it. The thread's stack holds only prelude addresses. The
reclaimer moves a parked thread whose resume address is in a retiring region with one
compare-exchange to `svc_resume_retired`, a prelude stub that clears the exclusive state, as the
block's tail would, and leaves the run, as the tail's halt test would (the retirement raised the
halt). If the thread took its address first, the exchange fails and the region is kept -- the
thread is running it and will leave within its budget slice. No barrier, no holes: a region is
given back whole. Threads waiting in the dispatcher (for a region, or for a location another thread
is translating) re-publish their epoch while they wait, so they do not hold one either.
`threads_parked_all_over_a_region_do_not_fragment_it` parks twelve threads through a region that
two fills retire, and fails on the hole design (two regions left held, 23 MB committed).

Regions are a quarter of the cache (256 MiB by default), not an eighth: a retirement forgets every
block, so a region must hold the whole working set with room to spare.

`ClearCache` (and a guest `IC IALLU`) in shared mode drops every block and unlinks every slot, but
does not retire the region: it is an invalidation of everything, not a reset of memory.

## 7. What the host plants into translated code (omni-cpu)

omni-cpu's `read_code` callback does not always return guest memory. For a **thunk**, an **inline
thunk** or the **return sentinel** it returns `SVC #0xFFFF` (`STOP_SVC`), and for a **breakpoint**
`BRK #0`. These sets are per context (`CpuCtx`). With a shared cache, a block translated by thread
A carries A's plants into thread B's execution. So in shared mode:

* the **planted addresses become a property of the address space**: a shared, reference-counted
  set; a context adding a thunk, an inline thunk or a sentinel counts it in, removing one counts it
  out, and the translated word changes (with an invalidation) only when the set's membership does.
  What happens at a planted `SVC` is still decided per context at execution (`call_svc` looks at the
  running context's own tables), so each thread still gets its own handler and its own sentinel
  stop. omni-android installs the **same** slots and the same sentinel on every context
  (`Boundary::install`, and `pthread_create` installs before the thread runs), so for the runtime the
  union is every context's set;
* **breakpoints** (a debugging overlay; nothing in the runtime uses them) are **refused** in shared
  mode: a breakpoint on one thread cannot be expressed in code every thread runs;
* a context that reaches a planted `SVC` it did not register gets `UnsupportedInstruction` naming
  `SVC #0xFFFF` -- a stop, never a silent wrong answer. Today it would have executed the guest word;
  the difference only exists for a context that has not installed the boundary.

Translation counters (`JitCounters::fetched`, `blocks`) accrue to the translating context, which is
what makes the translation saving visible per thread.

## 8. Operating systems

* **Windows x64**: the code buffer is one `MEM_RESERVE` region, committed `PAGE_EXECUTE_READWRITE`
  on demand (D12 exception: the translator's cache is W+X, as today); retired regions are
  `MEM_DECOMMIT`ted when reclaimed. One `RtlAddFunctionTable` covers the buffer.
* **Linux x64**: `mmap` RW then RWX (xbyak), pages on first touch; reclaimed regions get
  `madvise(MADV_DONTNEED)`. The POSIX fault handler records one code range.
* **macOS arm64: not in this change; the per-thread cache stays.** Why, exactly: (1) the Mac is not
  reachable over SSH for this work, so nothing could be built, run or measured there, and a
  concurrency change to a JIT backend that has never executed is not shippable; (2) the arm64 backend
  is a different code base -- `A64AddressSpace` with patches 0010-0016's compact records, the prelude
  trampolines that embed `this` and `&conf` as literals, `BL`s from blocks into a per-jit prelude,
  0020's out-of-line RSB check -- so it needs its own version of sections 2-6, not a port of this
  diff. The design carries over: `JitState`-relative loads from `Xstate`, `LDR Xt, slot; BR Xt` for
  links, the same regions and epochs. `pthread_jit_write_protect_np` being per thread suits it: only
  the emitting thread (holding the lock) opens its write window. The shim refuses a shared cache on
  arm64, so omni-cpu keeps per-thread caches there and says so.

## 9. Configuration, size, memory

`OMNI_JIT_SHARED_CACHE=1` (announced on stderr as every JIT switch is) turns it on; **off by
default**. `OMNI_JIT_SHARED_CACHE_MB` sizes it (default 1024 MiB of *address space*; regions of
`size / 8`). Memory is committed as code is emitted -- 1 MiB ahead, as upstream -- and given back
when a region is reclaimed. Per thread, what remains is `JitState`, the 64 KiB fast-dispatch table
and the shim's small allocations; no prelude, no constant pool, no code.

## 10. Expected gain (an estimate, to be replaced by measurements)

* **Translation work** falls by the number of threads that run the same code: up to 8x for the
  TaskScheduler workers (D37 caps them at 8). The 150-310k instructions a second of an input stall
  are, by the HANDOFF's own reading, mostly the same new code once per worker: expect ~20-40k
  unique, i.e. 0.3-0.6 cores of translation down to ~0.05-0.08. The frame waits on the *slowest*
  worker's first touch of the new code, not on each worker's.
* **Settled translation** (53% of dynarmic's samples): today partly duplicates and partly 32 MiB
  caches evacuating and refilling (gameactivity's `CODE_CACHE_BYTES` note measured 2,600-3,800
  re-translated instructions a second at 32 MiB). One 128 MiB region holds what all threads run.
* **Memory**: the ~0.4-0.5 GiB of per-thread code copies becomes one copy; the largest single cache
  was ~22 MiB (88k blocks x 250 B, macOS census), so the union should be well under 128 MiB. Plus
  ~2 MiB of prelude and constant-pool commit per thread, ~0.1 GiB at 50-60 threads.
* **Costs**: one indirect jump per linked transition; a shared lock on dispatcher lookups (after
  the per-thread fast-dispatch table misses); translation serialized behind one lock (at the
  measured unique rates, well under 10% of the lock's time).
* **Linux, 4 cores**: translation 32-36% of the busy workers should fall to a few percent.

## 11. Evidence plan

* dynarmic's own suite (unchanged flags, so the per-thread path) -- the default path must be
  unchanged.
* `dynarmic-sys` whole suite with `OD_TEST_SHARED_CACHE=1`, which makes the harness build every
  `Vm` on a shared cache of its own: every existing test then exercises the shared-mode emission
  (JitState-relative callbacks, slots, per-thread tables), including `the_stoppability_matrix`.
* new `dynarmic-sys` tests: N threads on one cache compute correctly and translate once; a stress
  test with N threads running while another rewrites and invalidates the code they run; region
  retirement and reclamation under running threads; a thread parked inside a callback pins a region;
  attach refuses a mismatched configuration.
* `omni-cpu`, `omni-android` suites (bionic, hostile, roundtrip, threads) with
  `OMNI_JIT_SHARED_CACHE=1`.
* benchmarks: translation count and time for 8 threads running the same code, per-thread vs
  shared; per-thread and total memory; the cost of a linked transition and of a call and return.
* hand mutations of the vendored C++, restored and SHA-1-checked.

## Results

Built as vendored patch 0022 (x64) and omni-cpu's `OMNI_JIT_SHARED_CACHE=1`, off by default. The
decision record, with every figure and its n, is **D38**; in short:

* **Correct in every suite, both ways**: dynarmic-sys 108 (Windows) / 164 (Linux), omni-cpu 139 /
  141, omni-android (bionic, hostile, roundtrip, thread_memory, the 3,594 initializers ...) 797 on
  Windows, per-thread and shared alike; the stoppability matrix included. 15 new detectors;
  13 hand mutations of the vendored C++, 13 caught.
* **Translation work divided by the threads**: eight threads meeting the 870 real Roblox leaves
  fetch 6,363 guest instructions for translation instead of 50,904.
* **Memory**: 4.548 MiB per guest thread -> 0.055 MiB, plus 4 MiB once; eight cold threads commit
  +1.8-5.6 MiB instead of +54 MiB.
* **Time**: on the 4-core Linux host the cold pass of eight threads is 63 -> 43 ms (same order) and
  62 -> 56-59 ms (spread), at half and a third of the CPU. On the 24-thread Windows host it is
  22 -> 31 ms and 20 -> 26 ms: there each thread had an idle core to translate its own copy on, and
  emission (two thirds of translation) is serialized. Warm code runs at the same speed; a lookup
  that misses the thread's table costs +15-50 ns.
* **What the build changed in this design** is marked *As designed* / *As built* above: slots next
  to their block, lookups through the thread's own table first, waiting on in-flight translations,
  translation outside the lock, parked threads holding only a hole, and three defects the
  measurements found (the fast-dispatch miss ordering, glibc's reader-preferring rwlock, the Linux
  barrier compiled out).
* **arm64** keeps per-thread caches (section 8).
