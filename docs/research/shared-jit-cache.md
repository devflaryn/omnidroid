# One translation cache per guest address space

Design and measurements of vendored patch 0022 (and 0024-0028), x64 only. The decision, with every
figure and its n, is D38 in `docs/DECISIONS.md`; the patch notes are
`crates/dynarmic-sys/patches/README.md` under 0022-0028.

## 1. The problem, as measured

Each guest thread had its own `A64::Jit`, so each owned a `BlockOfCode` (prelude, constant pool,
code) and an `A64EmitX64` (block map, links, fastmem table). Engine code was translated once per
thread that ran it. MEASURED in PS99 on Windows (runs w17-w21) and Linux (l6):

- Input stalls: a drag drops ~55 fps to 2-20 fps for seconds while translation jumps from 5-20k to
  150-310k guest insn/s; the 8 TaskScheduler workers each translate the same new code.
- Translation was 53% of dynarmic's samples in a settled world (Windows), 32-36% of busy workers'
  samples on the 4-core Linux host.
- ~0.4-0.5 GiB of translated code per instance, plus ~2 MiB prelude/constant pool per thread.

Cold translation costs ~2 us per guest instruction (D5 amendment 2).

## 2. What emitted code embeds per `Jit`

Shared code runs on every thread, so each per-thread value baked into a block or the prelude is
instead loaded from the running thread's `A64JitState` (`r15`). Patch 0022 appends these fields to
`A64JitState` (`a64_jitstate.h`), so no existing offset moves and a jit without a shared cache emits
byte for byte what it did: `od_callbacks`, `od_conf`, `od_lookup_arg`, `od_exclusive_address`,
`od_exclusive_value`, `od_tpidr_el0`, `od_tpidrro_el0`, `od_fast_dispatch_table`, and
`od_callback_return` (section 6).

| value | shared-mode handling |
|---|---|
| `UserCallbacks*` (`this` of every callback) | `[r15 + od_callbacks]` |
| dispatcher `LookupBlock` argument | `[r15 + od_lookup_arg]` |
| `&conf` for non-inline exclusives | `[r15 + od_conf]` |
| monitor slots for inline `LDXR`/`STXR` | `[r15 + od_exclusive_address/value]` |
| global-monitor scan skipping own slot | scans every slot (same as `CheckAndClear`); none under value-compare (D31) |
| TPIDR box addresses | `[r15 + od_tpidr_el0]` then deref |
| fast-dispatch table (0019) | stays per thread (entries are 16 bytes written non-atomically) |
| block-link patch sites, RSB `mov rcx, imm64` | never rewritten; go through 8-byte slots (section 4) |
| `r13` fastmem base, `r14` page table, constant pool, prelude | one per cache; attach requires equal config |
| SEH `RtlAddFunctionTable` / POSIX code range | one per shared buffer |

## 3. Ownership

`A64::SharedCodeCache` owns one `BlockOfCode` and one `A64EmitX64`, built from a template
`UserConfig`. Shim: `od_code_cache_new(template, total_bytes, region_bytes, live_bytes)`,
`od_jit_new_shared(config, cache)`, `od_code_cache_free`, `od_code_cache_stats_of`,
`od_code_cache_tables_of`, `od_code_cache_invalidate_range`, `od_code_cache_clear`
(`crates/dynarmic-sys/shim/od_dynarmic.h`). A jit keeps only its `A64JitState`, its 64 KiB
fast-dispatch table, its pending invalidations and its epoch word. Attach returns null for a config
that differs from the template in anything that shapes code; only `callbacks`, `processor_id` and
the TPIDR box addresses may differ. On an arm64 host `od_code_cache_new` returns null.

## 4. Linking without rewriting running code

Upstream rewrites a 22-23-byte sequence in place to link blocks; another core could be mid-sequence.
In shared mode published code is never modified. Each link site is
`cmp budget; jng .exit; jmp [rip + slot]` with the slot holding the target or `.unlinked`
(dispatcher). Link/unlink is one aligned 8-byte store. Slots sit right after their block
(MEASURED: slots a region away cost an extra cache/TLB miss per transition). A dropped block's own
slots are unlinked and removed from their targets' records (without it, emission slowed from ~9 to
~42 us per block in the stress test).

## 5. Translation serialized, execution not

One `std::shared_mutex` per cache. Lookup checks the thread's own fast-dispatch table first without
a lock (MEASURED: otherwise a warm call on real Roblox leaves cost 0.56 us instead of 0.20), then the
block map under a shared lock. Frontend and IR passes run outside the lock with the location marked
in flight; other threads needing it wait. Emission takes the lock exclusively and re-translates if
an invalidation touched the range meanwhile. Execution takes no lock.

Defect found and fixed: the fast-dispatch miss handler wrote the location before the lookup, so a
lookup consulting the table could pair a location with another's code
(`indirect_calls_that_collide_in_the_fast_dispatch_table_reach_their_own_targets` catches it).

In shared mode `recompile_on_fastmem_failure` and `recompile_on_exclusive_fastmem_failure` are off:
a declined fault still goes to the fallback callback each time, but shared code is not recompiled.

## 6. Invalidation, routing state, reclaiming memory

Invalidation drops blocks from the map and unlinks slots to them; their code is not freed. A
cache-wide `generation` bumps; each thread compares it at every `Run` entry and clears its RSB and
fast-dispatch table if it moved. With a shared cache the backend reports
`Capabilities::shared_translation` (`crates/omni-cpu/src/cpu.rs`), and omni-android applies a range
once instead of queueing it to every context (`crates/omni-android/src/boundary.rs`; the per-thread
queue and its `MAX_PENDING_INVALIDATIONS` = 64 overflow remain for per-thread caches).

Regions (patch 0028, D38 amendment 3): the buffer after the prelude is cut into regions filled in
turn. A full region stays live. Past `live_bytes` of live regions the oldest is retired: only its
blocks are forgotten (map entries, incoming links, own slots), `generation` and `epoch` bump, and
the region is decommitted once no thread holds it. Each thread publishes its entry epoch at `Run`
entry and `UINT64_MAX` on leaving (epoch-based reclamation, `Run` as quiescent point).

A thread parked in a host callback holds no region: shared code calls every `SVC` callback through
a prelude trampoline that keeps the resume address in `od_callback_return`; the reclaimer moves a
parked thread whose resume address is in a retiring region to the prelude stub
`svc_resume_retired` with one compare-exchange. (The first design kept holes around parked
threads' sites; it thrashed in the world, run w24, D38 amendment 1.)
`threads_parked_all_over_a_region_do_not_fragment_it` covers it.

## 7. What the host plants (omni-cpu)

omni-cpu's `read_code` returns `SVC #0xFFFF` for thunks, inline thunks and the return sentinel.
In shared mode those planted addresses are a reference-counted set per address space
(`shared_plants`); what a planted `SVC` does is still decided per context at execution.
Breakpoints are refused in shared mode. Translation counters accrue to the translating context.

## 8. Operating systems

- Windows x64: one `MEM_RESERVE`, committed `PAGE_EXECUTE_READWRITE` on demand (D12 exception),
  retired regions `MEM_DECOMMIT`ted.
- Linux x64: `mmap` RWX, reclaimed regions `madvise(MADV_DONTNEED)`.
- macOS arm64: not implemented; the arm64 backend (patches 0010-0016 records, prelude literals,
  `BL` to a per-jit prelude) needs its own version. Per-thread caches stay there.

## 9. Configuration

| env var | default (`crates/omni-cpu/src/dynarmic/mod.rs`) |
|---|---|
| `OMNI_JIT_SHARED_CACHE=0\|1` | on for x86_64 (D38 amendment 2), off elsewhere |
| `OMNI_JIT_SHARED_CACHE_MB` | 1024 (`SHARED_CODE_CACHE_BYTES`), range 64..2048 |
| `OMNI_JIT_SHARED_CACHE_REGION_MB` | 16 (`SHARED_CODE_REGION_BYTES`), >= 8 |
| `OMNI_JIT_SHARED_CACHE_LIVE_MB` | 256 (`SHARED_CODE_LIVE_BYTES`, D38 amendment 4; was 128) |

Test harness: `OD_TEST_SHARED_CACHE=1` makes the dynarmic-sys suite build every `Vm` on its own
shared cache.

## 10. Expected gain

Superseded by the measurements below and in D38.

## 11. Evidence

- All suites pass per-thread and shared: dynarmic-sys 108 (Windows) / 164 (Linux), omni-cpu
  139 / 141, omni-android 797 (Windows). 13 of 13 hand mutations of the vendored C++ caught.
- Eight threads over the 870 real Roblox leaves fetch 6,363 guest instructions for translation
  instead of 50,904. Memory 4.548 MiB per thread -> 0.055 MiB plus 4 MiB once.
- Cold pass of eight threads: Linux 4-core 63 -> 43 ms; Windows 24-thread 22 -> 31 ms (emission is
  serialized; each thread had an idle core before). Warm code unchanged; a table miss +15-50 ns.
- In PS99 with a drag script (w27/w29 vs w28): 0 s under 20 fps during input vs 14 s; translation
  peak 16 vs 326 kinsn/s; 3.2 vs 4.1 GiB private; settled fps unchanged.
- A world emits ~245 MiB in its first minutes (w27-w30). At 128 MiB live, PS99 evicted all session
  (w35); 256 MiB holds the working set (amendment 4).
