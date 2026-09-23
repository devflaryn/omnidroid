# A native CPU backend on macOS: Hypervisor.framework behind `GuestCpu`

Branch `mac-hvf`, from `port-macos` (`ef65a2d`). Host: Apple M1, 16 GB, macOS 26.5. The owner's
ask: *after parity, measure a native backend behind `GuestCpu`; adopt it only with numbers and a
DECISIONS record.* This is that measurement. **Everything marked MEASURED was run on that machine**
and carries its n and method; everything else is design, labelled as such.

> Written in order: the design first (below, before any code), then what was built, then the
> numbers, then the D31 draft. Where the build changed the design, the design section says so
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
  handler, 9.3 + 4.3 us per release and re-acquire) is a condition in the D31 draft, not built.

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

*(filled in below as it is built)*

## 4. Measurements

*(filled in below)*

## Merge notes

*(filled in below)*
