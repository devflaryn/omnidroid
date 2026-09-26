# macOS: a native CPU backend under Hypervisor.framework (D34)

`omni-cpu`'s `native-hvf` feature (aarch64, off by default) runs guest ARM64 on the M1's own cores
at EL0 in a VM, behind `GuestCpu`. Measured and **not adopted**: the decision and its conditions
are D34 in `docs/DECISIONS.md` (the draft once kept here). Test binaries must be signed with
`com.apple.security.hypervisor`: `tools/hvf_run.sh` as the cargo runner, the command is in
`omni-cpu/tests/native.rs`'s header; unsigned, every test fails at `hv_vm_create` (`HV_DENIED`).
All figures: Apple M1, release builds.

## 1. Facts the design stands on (C probes)

| question | answer |
|---|---|
| IPA size | 36 bits, default and maximum: guest-physical space is 64 GiB |
| vCPUs per VM | 64, each bound to the host thread that created it; create / destroy 9.3 / 4.3 us |
| a loop at EL0 in the VM vs on the host | 5,935 vs 5,934 M insn/s |
| `svc` -> EL1 -> `hvc` -> host -> resume by writing `PC`/`CPSR` | 786-790 ns (n = 200,000) |
| a branch to a stage-2 non-executable page | 2,092-2,293 ns |
| `hv_vm_unmap` + `hv_vm_map` of 16 KiB | 0.8 us |

1. **Host protection is not enforced at stage 2**: a range mapped read-write is read and written
   by the guest while the host has it `PROT_NONE`.
2. **`hv_vm_map` binds the memory object present at the call**: after the host replaces a range
   with `mmap(MAP_FIXED)` (a file view, a decommit) the guest sees the old pages until the range is
   unmapped and mapped again.
3. `hv_vm_map` over a mapped range fails; `hv_vm_unmap` of an unmapped one succeeds, so "unmap,
   then map" is an idempotent update.

## 2. Design

* **EL0 guest, EL1 owned by the backend** (a page of `hvc` vectors no guest address can equal):
  at EL1 the guest could rewrite its own translation tables.
* **Stage 1 is flat** (identity, set once); **stage 2 carries every permission, IPA == VA** (D4).
  Because of facts 1 and 2, `vm/macos.rs` calls a hook after each `mmap(MAP_FIXED)`, `mprotect`
  and `munmap`, which re-maps any attached range at stage 2 with the host's new protection.
* A lazily committed page is unmapped at stage 2; the first touch exits, `omni_mem::admit`
  commits it, and the instruction is retried (D10).
* Thunks are addresses that trap (a stage-2 overlay of `BRK #0xF00D`); there are no inline thunks,
  so **every import crossing is a VM exit**. No instruction counting: `RunLimit::Instructions` is
  refused and `HaltHandle` works through a 1 ms vtimer tick. Exclusives and atomics are the
  hardware's own.
* One vCPU per host thread that has run guest code; registers live in the context between runs.

Native gates that pass: the M2 functions on the real `libroblox.so` (`omni-cpu/tests/native.rs`)
and all 3,594 initializers (`omni-android/tests/native_initializers.rs`).

## 3. Not run natively

The game gate has never run on this backend: counted budgets remain in JNI, input, AAudio and
script paths, and a host pointer outside `GuestSpace` (unmapped at stage 2) would fault.

## 4. Measurements (`#[ignore]`d `measure_*` tests in those two files)

* **4.1 One import crossing**: native **1,614 ns** (638 of it saving the register file);
  dynarmic's inline dispatch **23.7 ns**. 68x.
* **4.2 The crossing rate at the landing screen** (`OMNI_CROSSING_RATE=1` on the dynarmic gate,
  +25..+55 s, two runs): the working threads, the looper's poll spin excluded, cross
  **0.40-0.52 M/s** (hottest `pthread_getspecific`, `clock_gettime`, `memcpy`, `memset`). Times 4.1:
  **0.64-0.84 of a core in VM exits**, against ~0.01 on dynarmic.
* **4.3 Guest code speed**: 33 real functions, identical results on both: native 7,553 M insn/s,
  dynarmic warm 1,320 (**5.7x**). 870 short leaves: 26x slower natively (the exit dominates).
* **4.4 The 3,594 initializers** (n = 8): native 795 ms and +23.5 MiB; dynarmic 2,417 ms and
  +99.8 MiB.
* **4.5 A demand-paged first touch**: 6.37 us per 64 KiB granule native, 24.77 dynarmic.
* **4.6 Memory per guest thread**: native 78.6 KiB (context + vCPU), not growing with the code run.
* **4.7 Robustness**: the native survey ran all 245,117 `.eh_frame` functions of `libroblox.so`
  with garbage-shaped arguments and the process survived every one. On dynarmic,
  `libroblox.so + 0x224822c` **aborted the process** ("Segfault ... wasn't at a fastmem patch
  location"): `0x2247264` leaves a pointer into `.data.rel.ro` and `0x224822c` swaps through it
  with the outlined `__aarch64_swp8_rel`, whose store patch 0007 had not registered. **Patch 0014**
  fixed it (`omni-cpu/tests/exclusive_store_fault.rs`, row `mac-cpu-E1`). Natively, a guest
  pointer to the host heap is a typed `MemoryFault`.
* **4.8 Limits**: a guest space must lie below 64 GiB; 64 vCPUs and one VM per process; each
  `mmap`/`mprotect` in an attached range costs an extra unmap and map.

## Open

No native gate run; no in-guest service of hot imports; no M:N vCPUs (the 65th thread is refused);
breakpoints and thunks inside real code are refused; the boundary's process-wide `crossings` lock
is taken on every exit-path crossing, which here is every import.
