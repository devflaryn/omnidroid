# Decisions log

Every entry records what was decided, why, and what it costs if the decision turns out wrong.
Decisions made without the user present are marked **Ruling**; they are reversible and are
surfaced so they can be overridden.

---

## D1 — Implementation language: Rust
**Ruling.** Rust is the primary language; C/C++ is reachable via FFI where a specific reusable
component earns its integration cost.

Why: it is the only systems toolchain verified working end-to-end on this machine with no extra
installs (`cargo build` compiles *and* links; `clang`/`gcc` are absent). Cargo's target model
serves the five-platform requirement. The JIT and guest-memory layers still get raw pointers via
localized `unsafe`, while the loader, object model, and resource tracking stay memory-safe — which
matters in a process whose entire job is hosting foreign binaries.

Cost if wrong: a language switch is expensive. Mitigated by keeping module seams
language-agnostic, and by putting the most likely C++ borrow (a guest CPU JIT) behind a narrow
interface.

Evidence: `research/host-environment.md`.

---

## D2 — Baseline host ISA is x86-64-v3, AVX-512 optional only
**Ruling.** The translator targets AVX2 + BMI2 + FMA + F16C + LZCNT + MOVBE + CMPXCHG16B. AVX-512
may only ever be an opportunistic fast path.

Why: measured on the dev machine — AVX2/BMI2 present, AVX-512 absent (Raptor Lake disables it).
BMI2's flag-preserving shifts map directly onto AArch64 shifted-register operands, which are
pervasive. `CMPXCHG16B` is required for AArch64 128-bit atomics and is already part of x86-64-v2,
so requiring it is safe.

Cost if wrong: hosts older than ~2013 (pre-Haswell) are excluded. Acceptable; a v2 fallback path
could be added later behind runtime feature detection.

Evidence: `research/host-environment.md`.

---

## D3 — Reuse only permissively-licensed components
**Ruling.** Omnidroid links only MIT / BSD / Apache-2.0 / ISC / 0BSD components. Copyleft projects
(GPL/LGPL) may be *studied* as architecture references but their code is not linked or copied.

Why: this keeps every future licensing option for Omnidroid open, including a closed or
permissively-licensed release. Choosing the most restrictive-safe path now costs little and
avoids a decision that would be very expensive to unwind. The user has not stated an intended
license for Omnidroid, so assuming the permissive-only constraint is the safe default rather than
a blocking question.

Consequence: `libhybris` (mixed, incl. LGPL/GPL3) and `android_translation_layer` (GPL-3.0+) are
reference-only. A bionic-compatible ELF loader must be written from scratch, because no
permissively-licensed standalone one exists.

Cost if wrong: if the user is happy with GPL, we did more original work than strictly necessary —
but the original work is clean-room and unencumbered, which has independent value.

Evidence: `research/prior-art.md`.

---

## D4 — Design target: guest virtual address == host virtual address
**Ruling (provisional, under test).** The guest ARM64 code runs in the host process's own address
space with no address translation: a guest pointer is a host pointer.

Why: it removes memory-translation overhead from every guest load and store, and it is what makes
the demand-driven memory requirement achievable — guest `mmap` becomes a host reservation/commit
directly, rather than carving out of a preallocated guest RAM blob. This is the design difference
between Omnidroid and a QEMU-style guest.

Cost if wrong: if the chosen CPU core cannot support identity mapping, every guest memory access
pays either a base-register add or a callback, and the memory model has to be reconsidered. This
is exactly why it is being verified by spike before the architecture is finalized, rather than
assumed.

Status: being measured (dynarmic spike, question 3; Windows memory model, questions 2-5).

---

## D5 — CPU core: pending spike
Prior art establishes dynarmic as the only permissively-licensed, direction-correct
A64-guest → x86-64-host JIT. Whether Omnidroid adopts it, adopts-then-replaces it, or writes a
custom JIT is **not yet decided** — it depends on whether it builds here, whether it supports
identity-mapped memory, its real throughput, and its 64-bit address-space assumptions.

No code will be written against either choice until the spike reports.
